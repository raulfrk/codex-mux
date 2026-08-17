//! Runtime composition for the interactive popup and tmux management commands.

use std::{
    env,
    ffi::OsString,
    fs, io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Duration,
};

use crossterm::event::{self, Event};

use crate::{
    MuxError, Result,
    cli::{Cli, Command, ConfigPathArgs, InstallArgs, RemoveArgs, SetupArgs, TmuxCommand},
    config::{XdgThemeStore, no_color_requested},
    domain::{
        ClientId, CodexExecutable, InvocationContext, PaneId, SessionId, ThemeStore,
        TmuxCommandRunner,
    },
    install::{
        DiscoveryContext, ExecutablePaths, InstallError, ServerEvidence, TmuxReloader,
        atomic_replace, discover_config, install_with_options, read, smart_left_owner, status,
        uninstall, validate_regular_writable,
    },
    linux_process::LinuxProcessInspector,
    shell_integration::{ShellKind, ShellOutcome, ShellTransaction},
    tmux::{
        actions::TmuxActions,
        inventory::PaneInventory,
        runner::SystemTmuxRunner,
        smart_left::{SmartLeftProbe, SystemSleeper},
    },
    ui::{self, Action, App, ColorPolicy},
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Runs the parsed command and writes management results to standard output.
pub fn run(cli: Cli) -> Result<()> {
    let codex_argument = cli.codex.clone();
    match cli.command.clone() {
        Some(Command::Setup(arguments)) => run_setup(arguments, codex_argument),
        Some(Command::Remove(arguments)) => run_remove(arguments),
        Some(Command::Tmux(tmux)) => run_tmux_command(tmux.command, codex_argument),
        Some(Command::SmartLeft) => run_smart_left(&cli, codex_argument),
        None => run_interactive(cli, codex_argument),
    }
}

fn run_setup(arguments: SetupArgs, codex_argument: Option<PathBuf>) -> Result<()> {
    let home = home_directory()?;
    let tmux_path = resolve_config(arguments.tmux_config)?;
    let shell_paths = shell_paths(&home, arguments.bash_config, arguments.zsh_config)?;
    validate_distinct_config_targets(&tmux_path, &shell_paths)?;
    let executables = executable_paths(codex_argument)?;
    let mut reloader = SystemTmuxReloader::for_path(&tmux_path)?;
    let tmux_snapshot = ConfigSnapshot::read(&tmux_path)?;
    let mut shells =
        ShellTransaction::prepare_install(shell_paths.clone()).map_err(install_error)?;
    let shell_outcomes = shells.apply().map_err(install_error)?;
    let tmux_outcome = match install_with_options(
        &tmux_path,
        &arguments.key,
        true,
        &executables,
        &mut reloader,
    ) {
        Ok(outcome) => outcome,
        Err(error) => {
            rollback_aggregate(&mut shells, &tmux_snapshot, &mut reloader, &error)?;
            return Err(install_error(error));
        }
    };
    print_shell_outcomes(&shell_outcomes, "installed");
    if tmux_outcome.changed {
        println!("installed codex-mux binding in {}", tmux_path.display());
    } else {
        println!(
            "codex-mux binding is already current in {}",
            tmux_path.display()
        );
    }
    if let Some(backup) = tmux_outcome.backup {
        println!("backup: {}", backup.display());
    }
    if tmux_outcome.reloaded {
        println!("reloaded running tmux server");
    }
    println!("open a new Bash/Zsh shell or source its startup file to activate shell Smart Left");
    Ok(())
}

fn run_remove(arguments: RemoveArgs) -> Result<()> {
    let home = home_directory()?;
    let tmux_path = resolve_config(arguments.tmux_config)?;
    let shell_paths = shell_paths(&home, arguments.bash_config, arguments.zsh_config)?;
    validate_distinct_config_targets(&tmux_path, &shell_paths)?;
    let mut reloader = SystemTmuxReloader::for_path(&tmux_path)?;
    let tmux_snapshot = ConfigSnapshot::read(&tmux_path)?;
    let mut shells = ShellTransaction::prepare_remove(shell_paths).map_err(install_error)?;
    let shell_outcomes = shells.apply().map_err(install_error)?;
    let removed = match uninstall(&tmux_path, &mut reloader) {
        Ok(removed) => removed,
        Err(error) => {
            rollback_aggregate(&mut shells, &tmux_snapshot, &mut reloader, &error)?;
            return Err(install_error(error));
        }
    };
    print_shell_outcomes(&shell_outcomes, "removed");
    if removed {
        println!("removed codex-mux binding from {}", tmux_path.display());
    } else {
        println!(
            "codex-mux binding was not installed in {}",
            tmux_path.display()
        );
    }
    Ok(())
}

fn shell_paths(
    home: &Path,
    bash: Option<PathBuf>,
    zsh: Option<PathBuf>,
) -> Result<Vec<(ShellKind, PathBuf)>> {
    let bash = absolute_target(bash.unwrap_or_else(|| home.join(".bashrc")))?;
    let zsh_root = env::var_os("ZDOTDIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.to_owned());
    let zsh = absolute_target(zsh.unwrap_or_else(|| zsh_root.join(".zshrc")))?;
    Ok(vec![(ShellKind::Bash, bash), (ShellKind::Zsh, zsh)])
}

fn absolute_target(path: PathBuf) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .map_err(|source| MuxError::Filesystem {
                path: PathBuf::from("current directory"),
                source,
            })?
            .join(path)
    };
    let parent = absolute
        .parent()
        .ok_or_else(|| MuxError::InvalidValue {
            field: "shell configuration path",
            message: "must have a parent directory".to_owned(),
        })?
        .canonicalize()
        .map_err(|source| MuxError::Filesystem {
            path: absolute.clone(),
            source,
        })?;
    let name = absolute.file_name().ok_or_else(|| MuxError::InvalidValue {
        field: "shell configuration path",
        message: "must have a file name".to_owned(),
    })?;
    Ok(parent.join(name))
}

fn validate_distinct_config_targets(tmux: &Path, shells: &[(ShellKind, PathBuf)]) -> Result<()> {
    let mut targets = vec![("tmux", tmux.to_owned())];
    targets.extend(shells.iter().map(|(kind, path)| {
        (
            match kind {
                ShellKind::Bash => "Bash",
                ShellKind::Zsh => "Zsh",
            },
            path.clone(),
        )
    }));
    for left in 0..targets.len() {
        for right in left + 1..targets.len() {
            if targets[left].1 == targets[right].1
                || same_existing_file(&targets[left].1, &targets[right].1)?
            {
                return Err(MuxError::InvalidValue {
                    field: "configuration paths",
                    message: format!(
                        "{} and {} targets must be distinct files",
                        targets[left].0, targets[right].0
                    ),
                });
            }
        }
    }
    Ok(())
}

fn same_existing_file(left: &Path, right: &Path) -> Result<bool> {
    let metadata = |path: &Path| match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(MuxError::Filesystem {
            path: path.to_owned(),
            source,
        }),
    };
    match (metadata(left)?, metadata(right)?) {
        (Some(left), Some(right)) => Ok(left.dev() == right.dev() && left.ino() == right.ino()),
        _ => Ok(false),
    }
}

fn home_directory() -> Result<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| MuxError::InvalidValue {
            field: "HOME",
            message: "must be set for zero-argument setup and removal".to_owned(),
        })
}

struct ConfigSnapshot {
    path: PathBuf,
    bytes: Vec<u8>,
    mode: u32,
}

impl ConfigSnapshot {
    fn read(path: &Path) -> Result<Self> {
        let metadata = validate_regular_writable(path).map_err(install_error)?;
        Ok(Self {
            path: path.to_owned(),
            bytes: read(path).map_err(install_error)?,
            mode: metadata.mode(),
        })
    }
}

fn rollback_aggregate(
    shells: &mut ShellTransaction,
    tmux: &ConfigSnapshot,
    reloader: &mut dyn TmuxReloader,
    cause: &InstallError,
) -> Result<()> {
    let mut failures = Vec::new();
    match read(&tmux.path) {
        Ok(current) if current == tmux.bytes => {}
        Ok(_) => {
            if let Err(error) = atomic_replace(&tmux.path, &tmux.bytes, tmux.mode) {
                failures.push(format!("restoring tmux configuration failed: {error}"));
            }
        }
        Err(error) => failures.push(format!(
            "reading tmux configuration for rollback failed: {error}"
        )),
    }
    if reloader.is_running() {
        if let Err(error) = reloader.reload(&tmux.path) {
            failures.push(format!("restoring live tmux configuration failed: {error}"));
        }
    }
    if let Err(error) = shells.rollback() {
        failures.push(format!("restoring shell configuration failed: {error}"));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(MuxError::Command(format!(
            "{cause}; aggregate rollback also failed: {}",
            failures.join("; ")
        )))
    }
}

fn print_shell_outcomes(outcomes: &[ShellOutcome], operation: &str) {
    for outcome in outcomes {
        if outcome.changed {
            println!(
                "{operation} codex-mux {} integration in {}",
                match outcome.kind {
                    ShellKind::Bash => "Bash",
                    ShellKind::Zsh => "Zsh",
                },
                outcome.path.display()
            );
            if let Some(backup) = &outcome.backup {
                println!("backup: {}", backup.display());
            }
        } else {
            println!(
                "codex-mux {} integration {} in {}",
                match outcome.kind {
                    ShellKind::Bash => "Bash",
                    ShellKind::Zsh => "Zsh",
                },
                if operation == "removed" {
                    "was not installed"
                } else {
                    "is already current"
                },
                outcome.path.display()
            );
        }
    }
}

fn run_smart_left(cli: &Cli, codex_argument: Option<PathBuf>) -> Result<()> {
    let context = invocation_context(cli)?;
    let codex = resolve_codex(codex_argument)?;
    let mux = env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|source| MuxError::Filesystem {
            path: PathBuf::from("current executable"),
            source,
        })?;
    let runner = SystemTmuxRunner::default();
    let inspector = LinuxProcessInspector::new(codex.clone());
    SmartLeftProbe::new(&runner, &inspector, &SystemSleeper, &mux, &codex).run(&context)?;
    Ok(())
}

fn run_interactive(cli: Cli, codex_argument: Option<PathBuf>) -> Result<()> {
    let context = invocation_context(&cli)?;
    let codex = resolve_codex(codex_argument)?;
    let inventory = PaneInventory::new(
        SystemTmuxRunner::default(),
        LinuxProcessInspector::new(codex.clone()),
        codex.clone(),
    );
    let panes = inventory.discover()?;
    let theme_store = XdgThemeStore::discover()?;
    let preference = theme_store.load_preference();
    let color_policy = if no_color_requested() {
        ColorPolicy::ForceMonochrome
    } else {
        ColorPolicy::Allow
    };
    let mut app =
        App::with_color_policy(panes, preference.selected, preference.warning, color_policy);
    app.select_pane(&context.pane_id);
    let runner = SystemTmuxRunner::default();
    let actions = TmuxActions::new(&runner, &codex);

    ui::terminal::with_terminal(io::stdout(), |terminal| {
        loop {
            terminal
                .draw(|frame| ui::render(frame, &app))
                .map_err(terminal_error)?;

            if !event::poll(REFRESH_INTERVAL).map_err(terminal_error)? {
                app.replace_panes(inventory.discover()?);
                continue;
            }
            let Event::Key(key) = event::read().map_err(terminal_error)? else {
                continue;
            };
            let Some(action) = app.handle_key(key) else {
                continue;
            };
            match action {
                Action::Activate(id) => {
                    let pane = app
                        .panes()
                        .iter()
                        .find(|pane| pane.id == id)
                        .ok_or_else(|| MuxError::Command("selected pane disappeared".to_owned()))?;
                    actions.switch_and_zoom(&context, pane)?;
                    return Ok(());
                }
                Action::New => {
                    actions.new_session(&context, selected_pane(&app))?;
                    return Ok(());
                }
                Action::Resume => {
                    actions.resume_all(&context, selected_pane(&app))?;
                    return Ok(());
                }
                Action::Close(id) => {
                    let pane = app
                        .panes()
                        .iter()
                        .find(|pane| pane.id == id)
                        .ok_or_else(|| MuxError::Command("selected pane disappeared".to_owned()))?;
                    actions.close_pane(pane)?;
                    app.replace_panes(inventory.discover()?);
                }
                Action::PersistTheme(theme) => theme_store.save(theme)?,
                Action::Quit => return Ok(()),
            }
        }
    })
}

fn selected_pane(app: &App) -> Option<&crate::domain::Pane> {
    let selected = app.selected_pane_id()?;
    app.panes().iter().find(|pane| &pane.id == selected)
}

fn invocation_context(cli: &Cli) -> Result<InvocationContext> {
    let required = |value: &Option<String>, field: &'static str| {
        value.clone().ok_or_else(|| MuxError::InvalidValue {
            field,
            message: "is required when opening the interactive popup from tmux".to_owned(),
        })
    };
    let current_path = cli
        .invoking_path
        .clone()
        .ok_or_else(|| MuxError::InvalidValue {
            field: "invoking path",
            message: "is required when opening the interactive popup from tmux".to_owned(),
        })?;
    if !current_path.is_absolute() {
        return Err(MuxError::InvalidValue {
            field: "invoking path",
            message: "must be absolute".to_owned(),
        });
    }
    Ok(InvocationContext {
        client_id: ClientId::new(required(&cli.client, "tmux client")?)?,
        pane_id: PaneId::new(required(&cli.invoking_pane, "invoking pane")?)?,
        session_id: SessionId::new(required(&cli.invoking_session, "invoking session")?)?,
        current_path,
    })
}

fn run_tmux_command(command: TmuxCommand, codex_argument: Option<PathBuf>) -> Result<()> {
    match command {
        TmuxCommand::Install(arguments) => install_binding(arguments, codex_argument),
        TmuxCommand::Status(arguments) => show_status(arguments, codex_argument),
        TmuxCommand::Uninstall(arguments) => uninstall_binding(arguments),
    }
}

fn install_binding(arguments: InstallArgs, codex_argument: Option<PathBuf>) -> Result<()> {
    let path = resolve_config(arguments.config)?;
    let executables = executable_paths(codex_argument)?;
    let mut reloader = SystemTmuxReloader::for_path(&path)?;
    let outcome = install_with_options(
        &path,
        &arguments.key,
        arguments.smart_left,
        &executables,
        &mut reloader,
    )
    .map_err(install_error)?;
    if outcome.changed {
        println!("installed codex-mux binding in {}", outcome.path.display());
        if let Some(backup) = outcome.backup {
            println!("backup: {}", backup.display());
        }
        if outcome.reloaded {
            println!("reloaded running tmux server");
        }
    } else {
        println!("codex-mux binding is already current in {}", path.display());
    }
    Ok(())
}

fn show_status(arguments: ConfigPathArgs, codex_argument: Option<PathBuf>) -> Result<()> {
    let path = resolve_config(arguments.config)?;
    let report = status(&path, &executable_paths(codex_argument)?).map_err(install_error)?;
    if !report.installed {
        println!("not installed: {}", report.path.display());
        return Ok(());
    }
    println!("installed: {}", report.path.display());
    println!("key: {}", report.key.as_deref().unwrap_or("<missing>"));
    println!(
        "codex-mux: {}",
        report
            .mux
            .as_deref()
            .map_or("<missing>".to_owned(), |path| path.display().to_string())
    );
    println!(
        "codex: {}",
        report
            .codex
            .as_deref()
            .map_or("<missing>".to_owned(), |path| path.display().to_string())
    );
    println!(
        "smart-left: {}",
        if report.smart_left {
            "enabled"
        } else {
            "disabled"
        }
    );
    if report.drift.is_empty() {
        println!("drift: none");
    } else {
        for drift in report.drift {
            println!("drift: {drift}");
        }
    }
    Ok(())
}

fn uninstall_binding(arguments: ConfigPathArgs) -> Result<()> {
    let path = resolve_config(arguments.config)?;
    let mut reloader = SystemTmuxReloader::for_path(&path)?;
    if uninstall(&path, &mut reloader).map_err(install_error)? {
        println!("removed codex-mux binding from {}", path.display());
    } else {
        println!("codex-mux binding was not installed in {}", path.display());
    }
    Ok(())
}

fn executable_paths(codex_argument: Option<PathBuf>) -> Result<ExecutablePaths> {
    let mux = env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|source| MuxError::Filesystem {
            path: PathBuf::from("current executable"),
            source,
        })?;
    ExecutablePaths::new(mux, resolve_codex(codex_argument)?.as_path().to_owned())
        .map_err(install_error)
}

fn resolve_codex(argument: Option<PathBuf>) -> Result<CodexExecutable> {
    if let Some(path) = argument {
        return CodexExecutable::new(path);
    }
    let path = env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
        .map(|directory| directory.join("codex"))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| MuxError::InvalidValue {
            field: "Codex executable",
            message: "pass --codex /absolute/path or place codex on PATH".to_owned(),
        })?;
    let path = path.canonicalize().map_err(|source| MuxError::Filesystem {
        path: path.clone(),
        source,
    })?;
    CodexExecutable::new(path)
}

fn resolve_config(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        let selected = discover_config(&DiscoveryContext {
            explicit: Some(path),
            server: ServerEvidence::NotRunning,
            home: PathBuf::new(),
            xdg_config_home: None,
        })
        .map_err(install_error)?;
        return canonical_path(&selected);
    }

    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| MuxError::InvalidValue {
            field: "HOME",
            message: "must be set for tmux configuration discovery".to_owned(),
        })?;
    let selected = discover_config(&DiscoveryContext {
        explicit: None,
        server: loaded_configs()?,
        home,
        xdg_config_home: env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
    })
    .map_err(install_error)?;
    canonical_path(&selected)
}

fn loaded_configs() -> Result<ServerEvidence> {
    let output = SystemTmuxRunner::default().run(&os_strings([
        "display-message",
        "-p",
        "#{config_files}",
    ]))?;
    if output.status == Some(0) {
        let text = String::from_utf8(output.stdout)
            .map_err(|_| MuxError::Command("tmux returned non-UTF-8 config paths".to_owned()))?;
        let paths = text
            .trim()
            .split(',')
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .collect();
        return Ok(ServerEvidence::Running(paths));
    }
    let error = String::from_utf8_lossy(&output.stderr);
    if error.contains("no server running")
        || error.contains("failed to connect")
        || error.contains("No such file or directory")
    {
        Ok(ServerEvidence::NotRunning)
    } else {
        Err(MuxError::Command(format!(
            "could not inspect tmux configuration files: {}",
            error.trim()
        )))
    }
}

struct SystemTmuxReloader {
    runner: SystemTmuxRunner,
    selected_is_loaded: bool,
}

impl SystemTmuxReloader {
    fn for_path(path: &Path) -> Result<Self> {
        let selected = canonical_path(path)?;
        let selected_is_loaded = match loaded_configs()? {
            ServerEvidence::NotRunning => false,
            ServerEvidence::Running(paths) => paths
                .iter()
                .map(|loaded| canonical_path(loaded))
                .collect::<Result<Vec<_>>>()?
                .iter()
                .any(|loaded| loaded == &selected),
        };
        Ok(Self {
            runner: SystemTmuxRunner::default(),
            selected_is_loaded,
        })
    }
}

impl TmuxReloader for SystemTmuxReloader {
    fn is_running(&self) -> bool {
        self.selected_is_loaded
    }

    fn unbind(&mut self, key: &str) -> std::result::Result<(), String> {
        let output = self
            .runner
            .run(&os_strings(["unbind-key", "-T", "prefix", key]))
            .map_err(|error| error.to_string())?;
        if output.status == Some(0) {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    }

    fn root_left_bound(&mut self) -> std::result::Result<bool, String> {
        let output = self
            .runner
            .run(&os_strings(["list-keys", "-T", "root", "Left"]))
            .map_err(|error| error.to_string())?;
        if output.status == Some(0) {
            return Ok(true);
        }
        let error = String::from_utf8_lossy(&output.stderr);
        if error.contains("unknown key") || error.contains("no key binding") {
            Ok(false)
        } else {
            Err(error.trim().to_owned())
        }
    }

    fn unbind_root_left(&mut self, expected_mux: &Path) -> std::result::Result<(), String> {
        let current = self
            .runner
            .run(&os_strings(["list-keys", "-T", "root", "Left"]))
            .map_err(|error| error.to_string())?;
        if current.status != Some(0) {
            return Err(String::from_utf8_lossy(&current.stderr).trim().to_owned());
        }
        let binding = String::from_utf8_lossy(&current.stdout);
        let signature = format!("owner={}; if [ -x ", smart_left_owner(expected_mux));
        if !binding.contains(&signature)
            || !binding.contains(" smart-left; else ")
            || !binding.contains("@codex_mux_smart_left_active")
        {
            return Err("root-table Left no longer matches the codex-mux-owned binding".to_owned());
        }
        let output = self
            .runner
            .run(&os_strings(["unbind-key", "-T", "root", "Left"]))
            .map_err(|error| error.to_string())?;
        if output.status == Some(0) {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    }

    fn reload(&mut self, path: &Path) -> std::result::Result<(), String> {
        let arguments = vec![OsString::from("source-file"), path.as_os_str().to_owned()];
        let output = self
            .runner
            .run(&arguments)
            .map_err(|error| error.to_string())?;
        if output.status == Some(0) {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    }
}

fn install_error(error: InstallError) -> MuxError {
    MuxError::Command(error.to_string())
}

fn terminal_error(source: io::Error) -> MuxError {
    MuxError::Filesystem {
        path: PathBuf::from("terminal"),
        source,
    }
}

fn canonical_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize().map_err(|source| MuxError::Filesystem {
        path: path.to_owned(),
        source,
    })
}

fn os_strings<const N: usize>(values: [&str; N]) -> Vec<OsString> {
    values.into_iter().map(OsString::from).collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::invocation_context;
    use crate::cli::Cli;

    #[test]
    fn interactive_context_requires_every_absolute_tmux_value() {
        let mut cli = Cli {
            codex: None,
            client: Some("/dev/pts/3".to_owned()),
            invoking_pane: Some("%4".to_owned()),
            invoking_session: Some("$2".to_owned()),
            invoking_path: Some(PathBuf::from("/work/project")),
            command: None,
        };
        assert!(invocation_context(&cli).is_ok());
        cli.invoking_path = Some(PathBuf::from("relative"));
        assert!(invocation_context(&cli).is_err());
        cli.invoking_path = None;
        assert!(invocation_context(&cli).is_err());
    }
}
