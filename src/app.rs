//! Runtime composition for the interactive popup and tmux management commands.

use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    env,
    ffi::OsString,
    fs,
    fs::OpenOptions,
    hash::{Hash, Hasher},
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crossterm::event::{self, Event};

use crate::{
    MuxError, Result,
    cli::{Cli, Command, ConfigPathArgs, InstallArgs, RemoveArgs, SetupArgs, TmuxCommand},
    config::{
        MatchScope, PermissionPreset, ProcessSettings, ThemePreference, XdgThemeStore,
        no_color_requested, validate_process_settings,
    },
    domain::{
        ClientId, CodexExecutable, InvocationContext, PaneId, SessionId, ThemeStore,
        TmuxCommandRunner,
    },
    install::{
        DiscoveryContext, ExecutablePaths, InstallError, ProcessMetadata, ServerEvidence,
        TmuxReloader, atomic_replace, discover_config, install_with_options, read,
        smart_left_owner, status, uninstall, validate_regular_writable,
    },
    linux_process::LinuxProcessInspector,
    shell_integration::{ShellKind, ShellOutcome, ShellTransaction},
    smart_naming::{AppServerNamer, AppServerProcess, NamingTarget, NamingWorker},
    tmux::{
        actions::TmuxActions,
        inventory::PaneInventory,
        owned_names::OwnedTmuxNames,
        runner::SystemTmuxRunner,
        smart_left::{SmartLeftMatcher, SmartLeftProbe, SystemSleeper},
    },
    ui::{self, Action, App, ColorPolicy},
    update::{UpdateOutcome, update},
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Clone, Debug)]
struct ProcessArguments {
    codex: Option<PathBuf>,
    launch_executable: Option<PathBuf>,
    match_executables: Vec<PathBuf>,
    pane_commands: Vec<String>,
    match_scope: Option<MatchScope>,
    match_command_regexes: Vec<String>,
    pane_command_regexes: Vec<String>,
}

#[derive(Clone, Debug)]
struct ResolvedProcessConfig {
    launch: CodexExecutable,
    matches: Vec<CodexExecutable>,
    pane_commands: Vec<String>,
    match_scope: MatchScope,
    match_command_regexes: Vec<String>,
    pane_command_regexes: Vec<String>,
    legacy_shorthand: bool,
}

struct RefreshSnapshot {
    generation: u64,
    panes: Result<Vec<crate::domain::Pane>>,
}

enum RefreshCommand {
    Request(u64),
    Stop,
}

struct PaneRefreshWorker {
    commands: mpsc::Sender<RefreshCommand>,
    snapshots: mpsc::Receiver<RefreshSnapshot>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PaneRefreshWorker {
    fn spawn<F>(mut discover: F) -> Self
    where
        F: FnMut() -> Result<Vec<crate::domain::Pane>> + Send + 'static,
    {
        let (commands, command_receiver) = mpsc::channel();
        let (snapshot_sender, snapshots) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut generation = 0;
            loop {
                let panes = discover();
                if snapshot_sender
                    .send(RefreshSnapshot { generation, panes })
                    .is_err()
                {
                    return;
                }
                let mut command = match command_receiver.recv_timeout(REFRESH_INTERVAL) {
                    Ok(command) => command,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                };
                while let Ok(next) = command_receiver.try_recv() {
                    if matches!(next, RefreshCommand::Stop) {
                        command = RefreshCommand::Stop;
                        break;
                    }
                    if let RefreshCommand::Request(next_generation) = next {
                        generation = generation.max(next_generation);
                    }
                }
                match command {
                    RefreshCommand::Request(next_generation) => {
                        generation = generation.max(next_generation);
                    }
                    RefreshCommand::Stop => return,
                }
            }
        });
        Self {
            commands,
            snapshots,
            thread: Some(thread),
        }
    }

    fn request(&self, generation: u64) {
        let _ = self.commands.send(RefreshCommand::Request(generation));
    }
}

impl Drop for PaneRefreshWorker {
    fn drop(&mut self) {
        let _ = self.commands.send(RefreshCommand::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Runs the parsed command and writes management results to standard output.
pub fn run(cli: Cli) -> Result<()> {
    let process_arguments = ProcessArguments {
        codex: cli.codex.clone(),
        launch_executable: cli.launch_executable.clone(),
        match_executables: cli.match_executable.clone(),
        pane_commands: cli.pane_command.clone(),
        match_scope: cli.match_scope,
        match_command_regexes: cli.match_command_regex.clone(),
        pane_command_regexes: cli.pane_command_regex.clone(),
    };
    match cli.command.clone() {
        Some(Command::Update(arguments)) => {
            match update(arguments.version.as_deref())? {
                UpdateOutcome::AlreadyCurrent(version) => {
                    println!("codex-mux {version} is already current");
                }
                UpdateOutcome::Installed(version) => {
                    println!("updated codex-mux to {version}");
                }
            }
            Ok(())
        }
        Some(Command::Setup(arguments)) => run_setup(arguments, process_arguments),
        Some(Command::Remove(arguments)) => run_remove(arguments),
        Some(Command::Tmux(tmux)) => run_tmux_command(tmux.command, process_arguments),
        Some(Command::SmartLeft) => run_smart_left(&cli, &process_arguments),
        Some(Command::SmartNamingWorker) => run_smart_naming_worker(&process_arguments),
        Some(Command::SmartNamingStart) => {
            let preference = process_preference(&process_arguments)?;
            ensure_naming_daemon(&resolve_process(&process_arguments, preference.as_ref())?)
        }
        None => run_interactive(cli, &process_arguments),
    }
}

fn run_setup(arguments: SetupArgs, process_arguments: ProcessArguments) -> Result<()> {
    let home = home_directory()?;
    let tmux_path = resolve_config(arguments.tmux_config)?;
    let shell_paths = shell_paths(&home, arguments.bash_config, arguments.zsh_config)?;
    validate_distinct_config_targets(&tmux_path, &shell_paths)?;
    let persist_process = process_arguments.launch_executable.is_some()
        || !process_arguments.match_executables.is_empty()
        || !process_arguments.pane_commands.is_empty()
        || process_arguments.match_scope.is_some()
        || !process_arguments.match_command_regexes.is_empty()
        || !process_arguments.pane_command_regexes.is_empty();
    let preference_required = process_preference_required(&process_arguments);
    let store = (persist_process || preference_required)
        .then(XdgThemeStore::discover)
        .transpose()?;
    let preference = if preference_required {
        Some(store.as_ref().expect("store required").load_preference())
    } else {
        None
    };
    let process = resolve_process(&process_arguments, preference.as_ref())?;
    let executables = executable_paths(&process)?;
    let mut reloader = SystemTmuxReloader::for_path(&tmux_path)?;
    let tmux_snapshot = ConfigSnapshot::read(&tmux_path)?;
    let process_snapshot = if persist_process {
        let store = store.as_ref().expect("store required");
        let snapshot = PreferenceSnapshot::read(store.path())?;
        store.save_process(ProcessSettings {
            launch_executable: process.launch.as_path().to_owned(),
            match_executables: process
                .matches
                .iter()
                .map(|executable| executable.as_path().to_owned())
                .collect(),
            pane_commands: process.pane_commands.clone(),
            match_scope: process.match_scope,
            match_command_regexes: process.match_command_regexes.clone(),
            pane_command_regexes: process.pane_command_regexes.clone(),
        })?;
        Some(snapshot)
    } else {
        None
    };
    let mut shells = match ShellTransaction::prepare_install(shell_paths.clone()) {
        Ok(shells) => shells,
        Err(error) => {
            restore_process_snapshot(process_snapshot.as_ref())?;
            return Err(install_error(error));
        }
    };
    let shell_outcomes = match shells.apply() {
        Ok(outcomes) => outcomes,
        Err(error) => {
            restore_process_snapshot(process_snapshot.as_ref())?;
            return Err(install_error(error));
        }
    };
    let tmux_outcome = match install_with_options(
        &tmux_path,
        &arguments.key,
        true,
        &executables,
        &mut reloader,
    ) {
        Ok(outcome) => outcome,
        Err(error) => {
            let rollback = rollback_aggregate(&mut shells, &tmux_snapshot, &mut reloader, &error);
            let process_rollback = restore_process_snapshot(process_snapshot.as_ref());
            rollback?;
            process_rollback?;
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

struct PreferenceSnapshot {
    path: PathBuf,
    previous: Option<(Vec<u8>, u32)>,
}

impl PreferenceSnapshot {
    fn read(path: &Path) -> Result<Self> {
        let previous = match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => Some((
                fs::read(path).map_err(|source| MuxError::Filesystem {
                    path: path.to_owned(),
                    source,
                })?,
                metadata.mode(),
            )),
            Ok(_) => {
                return Err(MuxError::InvalidValue {
                    field: "process configuration path",
                    message: "must be a regular file".to_owned(),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(MuxError::Filesystem {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        Ok(Self {
            path: path.to_owned(),
            previous,
        })
    }

    fn restore(&self) -> Result<()> {
        match &self.previous {
            Some((bytes, mode)) => atomic_replace(&self.path, bytes, *mode).map_err(install_error),
            None => match fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(source) => Err(MuxError::Filesystem {
                    path: self.path.clone(),
                    source,
                }),
            },
        }
    }
}

fn restore_process_snapshot(snapshot: Option<&PreferenceSnapshot>) -> Result<()> {
    snapshot.map_or(Ok(()), PreferenceSnapshot::restore)
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

fn run_smart_left(cli: &Cli, process_arguments: &ProcessArguments) -> Result<()> {
    let context = invocation_context(cli)?;
    let preference = process_preference(process_arguments)?;
    let process = resolve_process(process_arguments, preference.as_ref())?;
    let mux = env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|source| MuxError::Filesystem {
            path: PathBuf::from("current executable"),
            source,
        })?;
    let runner = SystemTmuxRunner::default();
    let inspector = LinuxProcessInspector::with_matcher(
        process.matches.clone(),
        process.match_scope,
        &process.match_command_regexes,
    )?;
    SmartLeftProbe::with_process_matcher(
        &runner,
        &inspector,
        &SystemSleeper,
        &mux,
        &process.launch,
        SmartLeftMatcher {
            pane_commands: &process.pane_commands,
            match_executables: &process.matches,
            pane_command_regexes: &process.pane_command_regexes,
            match_scope: match process.match_scope {
                MatchScope::Foreground => "foreground",
                MatchScope::PaneTree => "pane-tree",
                MatchScope::PaneTty => "pane-tty",
            },
            match_command_regexes: &process.match_command_regexes,
        },
    )
    .run(&context)?;
    Ok(())
}

fn run_interactive(cli: Cli, process_arguments: &ProcessArguments) -> Result<()> {
    let context = invocation_context(&cli)?;
    let theme_store = XdgThemeStore::discover()?;
    let preference = theme_store.load_preference();
    let process = resolve_process(process_arguments, Some(&preference))?;
    let codex = process.launch.clone();
    let mut codex_executables = process.matches.clone();
    for path in preference
        .profiles
        .iter()
        .filter_map(|profile| profile.executable.clone())
    {
        let executable = CodexExecutable::new(path)?;
        if !codex_executables.contains(&executable) {
            codex_executables.push(executable);
        }
    }
    let inspector = LinuxProcessInspector::with_matcher(
        codex_executables.clone(),
        process.match_scope,
        &process.match_command_regexes,
    )?;
    let inventory =
        PaneInventory::with_executables(SystemTmuxRunner::default(), inspector, codex_executables);
    let refresh_worker = PaneRefreshWorker::spawn(move || inventory.discover());
    let color_policy = if no_color_requested() {
        ColorPolicy::ForceMonochrome
    } else {
        ColorPolicy::Allow
    };
    let mut app = App::with_settings(
        Vec::new(),
        preference.selected,
        preference.warning,
        color_policy,
        preference.profiles,
        preference.smart_naming,
    );
    let mut initial_selection_pending = true;
    let mut minimum_refresh_generation = 0_u64;
    let runner = SystemTmuxRunner::default();
    let actions = TmuxActions::new(&runner, &codex);
    if preference.smart_naming {
        if let Err(error) = ensure_naming_daemon(&process) {
            app.smart_naming_runtime_failed(error.to_string());
        }
    }
    let (shutdown_sender, shutdown_receiver) = mpsc::channel::<Result<()>>();
    let mut shutdown_pending = false;
    let mut dirty = true;

    ui::terminal::with_terminal(io::stdout(), |terminal| {
        loop {
            let mut newest_snapshot = None;
            while let Ok(snapshot) = refresh_worker.snapshots.try_recv() {
                if snapshot.generation >= minimum_refresh_generation {
                    newest_snapshot = Some(snapshot);
                }
            }
            if let Some(snapshot) = newest_snapshot {
                match snapshot.panes {
                    Ok(panes) => {
                        app.inventory_refreshed(panes);
                        if initial_selection_pending {
                            app.select_pane(&context.pane_id);
                            initial_selection_pending = false;
                        }
                        dirty = true;
                    }
                    Err(error) => {
                        app.inventory_failed(error.to_string());
                        dirty = true;
                    }
                }
            }
            if shutdown_pending {
                match shutdown_receiver.try_recv() {
                    Ok(Ok(())) => {
                        app.smart_naming_saved(false);
                        shutdown_pending = false;
                        dirty = true;
                    }
                    Ok(Err(error)) => {
                        app.smart_naming_runtime_failed(error.to_string());
                        shutdown_pending = false;
                        dirty = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => {
                        app.smart_naming_runtime_failed("smart-naming shutdown channel closed");
                        shutdown_pending = false;
                        dirty = true;
                    }
                }
            }
            if dirty {
                terminal
                    .draw(|frame| ui::render(frame, &app))
                    .map_err(terminal_error)?;
                dirty = false;
            }

            if !event::poll(INPUT_POLL_INTERVAL).map_err(terminal_error)? {
                continue;
            }
            let event = event::read().map_err(terminal_error)?;
            dirty = true;
            let Event::Key(key) = event else {
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
                Action::LaunchProfile(profile) => {
                    actions.new_session_with_profile(
                        &context,
                        selected_pane(&app),
                        &codex,
                        profile.permissions == PermissionPreset::Yolo,
                    )?;
                    return Ok(());
                }
                Action::PersistProfiles(profiles) => match theme_store.save_profiles(&profiles) {
                    Ok(()) => app.profiles_saved(profiles),
                    Err(error) => app.profile_save_failed(error.to_string()),
                },
                Action::PersistSmartNaming(enabled) => {
                    match theme_store.save_smart_naming(enabled) {
                        Err(error) => app.smart_naming_save_failed(error.to_string()),
                        Ok(()) => {
                            if enabled {
                                app.smart_naming_saved(true);
                                if let Err(error) = ensure_naming_daemon(&process) {
                                    app.smart_naming_runtime_failed(error.to_string());
                                }
                            } else {
                                app.smart_naming_stopping();
                                shutdown_pending = true;
                                let sender = shutdown_sender.clone();
                                thread::spawn(move || {
                                    let _ = sender.send(wait_for_naming_daemon_stop());
                                });
                            }
                        }
                    }
                }
                Action::Resume => {
                    let profile = app.resume_profile().ok_or_else(|| {
                        MuxError::Command("resume profile selection disappeared".to_owned())
                    })?;
                    actions.resume_all_with_profile(
                        &context,
                        selected_pane(&app),
                        &codex,
                        profile.permissions == PermissionPreset::Yolo,
                    )?;
                    return Ok(());
                }
                Action::Close(id) => {
                    let pane = app
                        .panes()
                        .iter()
                        .find(|pane| pane.id == id)
                        .cloned()
                        .ok_or_else(|| MuxError::Command("selected pane disappeared".to_owned()))?;
                    actions.close_pane(&pane)?;
                    app.replace_panes(
                        app.panes()
                            .iter()
                            .filter(|candidate| candidate.id != id)
                            .cloned()
                            .collect(),
                    );
                    minimum_refresh_generation = minimum_refresh_generation.saturating_add(1);
                    refresh_worker.request(minimum_refresh_generation);
                }
                Action::PersistTheme(theme) => theme_store.save(theme)?,
                Action::Quit => return Ok(()),
            }
        }
    })
}

fn start_naming_worker(process: &ResolvedProcessConfig) -> NamingWorker {
    let codex_path = process.launch.as_path().to_owned();
    let inspector = LinuxProcessInspector::with_matcher(
        process.matches.clone(),
        process.match_scope,
        &process.match_command_regexes,
    )
    .expect("resolved process matcher is valid");
    let inventory = PaneInventory::with_executables(
        SystemTmuxRunner::default(),
        inspector,
        process.matches.clone(),
    );
    NamingWorker::spawn(
        move |cancelled| {
            AppServerProcess::spawn_with_cancel(&codex_path, cancelled).map(AppServerNamer::new)
        },
        move || {
            inventory
                .discover()
                .map(|panes| panes.iter().filter_map(NamingTarget::from_pane).collect())
        },
        Duration::from_secs(30),
    )
}

fn run_smart_naming_worker(process_arguments: &ProcessArguments) -> Result<()> {
    let Some(_lock) = try_naming_daemon_lock()? else {
        return Ok(());
    };
    let store = XdgThemeStore::discover()?;
    let owned_names = OwnedTmuxNames::new(SystemTmuxRunner::default());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    owned_names.migrate_legacy_window_names(now);
    let preference = store.load_preference();
    if !preference.smart_naming {
        owned_names.clear_all();
        return Ok(());
    }
    let mut process = resolve_process(process_arguments, Some(&preference))?;
    for path in preference
        .profiles
        .iter()
        .filter_map(|profile| profile.executable.clone())
    {
        let executable = CodexExecutable::new(path)?;
        if !process.matches.contains(&executable) {
            process.matches.push(executable);
        }
    }
    let mut worker = Some(start_naming_worker(&process));
    let mut applied_names = HashMap::new();
    let mut last_name_reconcile = Instant::now() - Duration::from_secs(2);
    let identity = naming_server_identity_from_environment()?;
    let mut retry = Duration::from_millis(100);
    while store.load_preference().smart_naming && tmux_server_matches(&identity) {
        let names = worker
            .as_ref()
            .expect("worker exists while daemon loop runs")
            .names()
            .lock()
            .unwrap()
            .clone();
        if names != applied_names || last_name_reconcile.elapsed() >= Duration::from_secs(2) {
            owned_names.reconcile(&names);
            applied_names = names;
            last_name_reconcile = Instant::now();
        }
        if worker.as_ref().is_some_and(NamingWorker::is_finished) {
            worker.take().expect("finished worker exists").stop();
            if !wait_for_naming_retry(&store, &identity, retry) {
                break;
            }
            retry = (retry * 2).min(Duration::from_secs(5));
            worker = Some(start_naming_worker(&process));
        }
        thread::sleep(Duration::from_millis(100));
    }
    if let Some(worker) = worker {
        worker.stop();
    }
    if !store.load_preference().smart_naming {
        owned_names.clear_all();
    }
    Ok(())
}

fn wait_for_naming_retry(
    store: &XdgThemeStore,
    identity: &NamingServerIdentity,
    duration: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + duration;
    loop {
        if !store.load_preference().smart_naming || !tmux_server_matches(identity) {
            return false;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return true;
        }
        thread::sleep(remaining.min(Duration::from_millis(50)));
    }
}

fn ensure_naming_daemon(process: &ResolvedProcessConfig) -> Result<()> {
    let executable = env::current_exe().map_err(|source| MuxError::Filesystem {
        path: PathBuf::from("current executable"),
        source,
    })?;
    let mut command = vec!["exec".to_owned(), shell_word(&executable)?];
    command.extend(process_cli_words(process)?);
    command.push("smart-naming-worker".to_owned());
    let command = command.join(" ");
    let output = SystemTmuxRunner::default().run(&[
        OsString::from("run-shell"),
        OsString::from("-b"),
        OsString::from(command),
    ])?;
    if output.status == Some(0) {
        Ok(())
    } else {
        Err(MuxError::Command(format!(
            "tmux could not start smart-naming worker: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn wait_for_naming_daemon_stop() -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(lock) = try_naming_daemon_lock()? {
            drop(lock);
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(MuxError::Command(
                "smart-naming worker did not stop within 3 seconds".to_owned(),
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn try_naming_daemon_lock() -> Result<Option<fs::File>> {
    let path = naming_daemon_lock_path()?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path).map_err(|source| MuxError::Filesystem {
        path: path.clone(),
        source,
    })?;
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(source) => Err(MuxError::Filesystem {
            path,
            source: source.into(),
        }),
    }
}

fn naming_daemon_lock_path() -> Result<PathBuf> {
    let server = naming_server_identity_from_environment()?;
    let mut hasher = DefaultHasher::new();
    server.hash(&mut hasher);
    let root = env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    Ok(root.join(format!("codex-mux-namer-{:016x}.lock", hasher.finish())))
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

fn run_tmux_command(command: TmuxCommand, process_arguments: ProcessArguments) -> Result<()> {
    match command {
        TmuxCommand::Install(arguments) => install_binding(arguments, &process_arguments),
        TmuxCommand::Status(arguments) => show_status(arguments, &process_arguments),
        TmuxCommand::Uninstall(arguments) => uninstall_binding(arguments),
    }
}

fn install_binding(arguments: InstallArgs, process_arguments: &ProcessArguments) -> Result<()> {
    let path = resolve_config(arguments.config)?;
    let preference = process_preference(process_arguments)?;
    let executables = executable_paths(&resolve_process(process_arguments, preference.as_ref())?)?;
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

fn show_status(arguments: ConfigPathArgs, process_arguments: &ProcessArguments) -> Result<()> {
    let path = resolve_config(arguments.config)?;
    let preference = process_preference(process_arguments)?;
    let expected = executable_paths(&resolve_process(process_arguments, preference.as_ref())?)?;
    let report = status(&path, &expected).map_err(install_error)?;
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
        "launch-executable: {}",
        report
            .codex
            .as_deref()
            .map_or("<missing>".to_owned(), |path| path.display().to_string())
    );
    for executable in &report.match_executables {
        println!("match-executable: {}", executable.display());
    }
    for command in &report.pane_commands {
        println!("pane-command: {command}");
    }
    println!(
        "match-scope: {}",
        report.match_scope.as_deref().unwrap_or("<missing>")
    );
    for expression in &report.match_command_regexes {
        println!("match-command-regex: {expression}");
    }
    for expression in &report.pane_command_regexes {
        println!("pane-command-regex: {expression}");
    }
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

fn executable_paths(process: &ResolvedProcessConfig) -> Result<ExecutablePaths> {
    let mux = env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|source| MuxError::Filesystem {
            path: PathBuf::from("current executable"),
            source,
        })?;
    ExecutablePaths::with_process(
        mux,
        process.launch.as_path().to_owned(),
        ProcessMetadata {
            match_executables: process
                .matches
                .iter()
                .map(|path| path.as_path().to_owned())
                .collect(),
            pane_commands: process.pane_commands.clone(),
            match_scope: match process.match_scope {
                MatchScope::Foreground => "foreground",
                MatchScope::PaneTree => "pane-tree",
                MatchScope::PaneTty => "pane-tty",
            }
            .to_owned(),
            match_command_regexes: process.match_command_regexes.clone(),
            pane_command_regexes: process.pane_command_regexes.clone(),
        },
        process.legacy_shorthand,
    )
    .map_err(install_error)
}

fn discover_codex() -> Result<PathBuf> {
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
    Ok(path)
}

fn resolve_process(
    arguments: &ProcessArguments,
    preference: Option<&ThemePreference>,
) -> Result<ResolvedProcessConfig> {
    let stored = preference.and_then(|preference| preference.process.clone());
    let legacy_shorthand = arguments.codex.is_some();
    let launch = if let Some(path) = &arguments.codex {
        path.clone()
    } else if let Some(path) = &arguments.launch_executable {
        path.clone()
    } else if let Some(settings) = &stored {
        settings.launch_executable.clone()
    } else {
        discover_codex()?
    };
    let match_executables = if let Some(path) = &arguments.codex {
        vec![path.clone()]
    } else if !arguments.match_executables.is_empty() {
        arguments.match_executables.clone()
    } else if let Some(settings) = &stored {
        settings.match_executables.clone()
    } else {
        vec![launch.clone()]
    };
    let pane_commands = if arguments.codex.is_some() {
        vec![legacy_pane_command(&launch)?]
    } else if !arguments.pane_commands.is_empty() {
        arguments.pane_commands.clone()
    } else if let Some(settings) = &stored {
        settings.pane_commands.clone()
    } else {
        vec![legacy_pane_command(&launch)?]
    };
    let match_scope = if arguments.codex.is_some() {
        MatchScope::Foreground
    } else if let Some(scope) = arguments.match_scope {
        scope
    } else if let Some(settings) = &stored {
        settings.match_scope
    } else {
        MatchScope::Foreground
    };
    let match_command_regexes = if arguments.codex.is_some() {
        Vec::new()
    } else if !arguments.match_command_regexes.is_empty() {
        arguments.match_command_regexes.clone()
    } else if let Some(settings) = &stored {
        settings.match_command_regexes.clone()
    } else {
        Vec::new()
    };
    let pane_command_regexes = if arguments.codex.is_some() {
        Vec::new()
    } else if !arguments.pane_command_regexes.is_empty() {
        arguments.pane_command_regexes.clone()
    } else if let Some(settings) = &stored {
        settings.pane_command_regexes.clone()
    } else {
        Vec::new()
    };
    let settings = ProcessSettings {
        launch_executable: launch.clone(),
        match_executables: match_executables.clone(),
        pane_commands: pane_commands.clone(),
        match_scope,
        match_command_regexes: match_command_regexes.clone(),
        pane_command_regexes: pane_command_regexes.clone(),
    };
    validate_process_settings(&settings)?;
    let mut matches = match_executables
        .into_iter()
        .map(CodexExecutable::new)
        .collect::<Result<Vec<_>>>()?;
    if let Some(preference) = preference {
        for path in preference
            .profiles
            .iter()
            .filter_map(|profile| profile.executable.clone())
        {
            let settings = ProcessSettings {
                launch_executable: path.clone(),
                match_executables: vec![path.clone()],
                pane_commands: vec![legacy_pane_command(&path)?],
                match_scope: MatchScope::Foreground,
                match_command_regexes: Vec::new(),
                pane_command_regexes: Vec::new(),
            };
            validate_process_settings(&settings)?;
            let executable = CodexExecutable::new(path)?;
            if !matches.contains(&executable) {
                matches.push(executable);
            }
        }
    }
    Ok(ResolvedProcessConfig {
        launch: CodexExecutable::new(launch)?,
        matches,
        pane_commands,
        match_scope,
        match_command_regexes,
        pane_command_regexes,
        legacy_shorthand,
    })
}

fn process_preference(arguments: &ProcessArguments) -> Result<Option<ThemePreference>> {
    if !process_preference_required(arguments) {
        Ok(None)
    } else {
        Ok(Some(XdgThemeStore::discover()?.load_preference()))
    }
}

fn process_preference_required(arguments: &ProcessArguments) -> bool {
    arguments.codex.is_none()
        && !(arguments.launch_executable.is_some()
            && !arguments.match_executables.is_empty()
            && (!arguments.pane_commands.is_empty() || !arguments.pane_command_regexes.is_empty()))
}

fn legacy_pane_command(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| MuxError::InvalidValue {
            field: "Codex executable",
            message: "must have a UTF-8 file name".to_owned(),
        })
}

fn process_cli_words(process: &ResolvedProcessConfig) -> Result<Vec<String>> {
    if process.legacy_shorthand {
        return Ok(vec![
            "--codex".to_owned(),
            shell_word(process.launch.as_path())?,
        ]);
    }
    let mut words = vec![
        "--launch-executable".to_owned(),
        shell_word(process.launch.as_path())?,
    ];
    for executable in &process.matches {
        words.push("--match-executable".to_owned());
        words.push(shell_word(executable.as_path())?);
    }
    for command in &process.pane_commands {
        words.push("--pane-command".to_owned());
        words.push(format!("'{}'", command.replace('\'', "'\\''")));
    }
    if !process.legacy_shorthand {
        words.push("--match-scope".to_owned());
        words.push(
            match process.match_scope {
                MatchScope::Foreground => "foreground",
                MatchScope::PaneTree => "pane-tree",
                MatchScope::PaneTty => "pane-tty",
            }
            .to_owned(),
        );
        for expression in &process.match_command_regexes {
            words.push("--match-command-regex".to_owned());
            words.push(format!("'{}'", expression.replace('\'', "'\\''")));
        }
        for expression in &process.pane_command_regexes {
            words.push("--pane-command-regex".to_owned());
            words.push(format!("'{}'", expression.replace('\'', "'\\''")));
        }
    }
    Ok(words)
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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct NamingServerIdentity {
    socket: PathBuf,
    pid: u32,
    socket_device: u64,
    socket_inode: u64,
    process_start_time: u64,
}

fn naming_server_identity_from_environment() -> Result<NamingServerIdentity> {
    let value = env::var_os("TMUX")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| MuxError::InvalidValue {
            field: "TMUX",
            message: "is required for the smart-naming worker".to_owned(),
        })?;
    let (socket, pid) = parse_naming_server_identity(&value)?;
    let metadata = fs::metadata(&socket).map_err(|source| MuxError::Filesystem {
        path: socket.clone(),
        source,
    })?;
    Ok(NamingServerIdentity {
        socket,
        pid,
        socket_device: metadata.dev(),
        socket_inode: metadata.ino(),
        process_start_time: process_start_time(pid)?,
    })
}

fn parse_naming_server_identity(value: &std::ffi::OsStr) -> Result<(PathBuf, u32)> {
    let value = value.to_str().ok_or_else(|| MuxError::InvalidValue {
        field: "TMUX",
        message: "must be valid UTF-8".to_owned(),
    })?;
    let mut fields = value.rsplitn(3, ',');
    let _client = fields.next();
    let pid = fields
        .next()
        .and_then(|pid| pid.parse::<u32>().ok())
        .filter(|pid| *pid > 0)
        .ok_or_else(|| MuxError::InvalidValue {
            field: "TMUX",
            message: "must contain a positive server PID".to_owned(),
        })?;
    let socket = fields
        .next()
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| MuxError::InvalidValue {
            field: "TMUX",
            message: "must contain an absolute server socket".to_owned(),
        })?;
    Ok((socket, pid))
}

fn tmux_server_matches(expected: &NamingServerIdentity) -> bool {
    fs::metadata(&expected.socket).is_ok_and(|metadata| {
        metadata.dev() == expected.socket_device && metadata.ino() == expected.socket_inode
    }) && process_start_time(expected.pid).ok() == Some(expected.process_start_time)
}

fn process_start_time(pid: u32) -> Result<u64> {
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    let stat = fs::read_to_string(&path).map_err(|source| MuxError::Filesystem {
        path: path.clone(),
        source,
    })?;
    stat.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| {
            MuxError::Command(format!(
                "could not parse process start time from {}",
                path.display()
            ))
        })
}

fn shell_word(path: &Path) -> Result<String> {
    let path = path.to_str().ok_or_else(|| MuxError::InvalidValue {
        field: "smart-naming executable path",
        message: "must be valid UTF-8 for tmux run-shell".to_owned(),
    })?;
    Ok(format!("'{}'", path.replace('\'', "'\\''")))
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
    use std::{path::PathBuf, sync::mpsc, time::Duration};

    use super::{PaneRefreshWorker, invocation_context, parse_naming_server_identity};
    use crate::cli::Cli;

    #[test]
    fn interactive_context_requires_every_absolute_tmux_value() {
        let mut cli = Cli {
            codex: None,
            launch_executable: None,
            match_executable: Vec::new(),
            pane_command: Vec::new(),
            match_scope: None,
            match_command_regex: Vec::new(),
            pane_command_regex: Vec::new(),
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

    #[test]
    fn naming_identity_ignores_tmux_client_suffix() {
        let first =
            parse_naming_server_identity(std::ffi::OsStr::new("/tmp/tmux/socket,4321,0")).unwrap();
        let second =
            parse_naming_server_identity(std::ffi::OsStr::new("/tmp/tmux/socket,4321,9")).unwrap();
        assert_eq!(first, second);
        assert_ne!(
            first,
            parse_naming_server_identity(std::ffi::OsStr::new("/tmp/tmux/socket,9876,0")).unwrap()
        );
    }

    #[test]
    fn refresh_worker_starts_without_waiting_and_coalesces_generations() {
        let (started_sender, started) = mpsc::channel();
        let (release_sender, release) = mpsc::channel();
        let mut calls = 0;
        let worker = PaneRefreshWorker::spawn(move || {
            calls += 1;
            if calls == 1 {
                started_sender.send(()).unwrap();
                release.recv().unwrap();
            }
            Ok(Vec::new())
        });

        started.recv_timeout(Duration::from_secs(1)).unwrap();
        worker.request(1);
        worker.request(2);
        release_sender.send(()).unwrap();

        let stale = worker
            .snapshots
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        let current = worker
            .snapshots
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(stale.generation, 0);
        assert_eq!(current.generation, 2);
    }

    #[test]
    fn refresh_worker_recovers_after_a_discovery_error() {
        let mut calls = 0;
        let worker = PaneRefreshWorker::spawn(move || {
            calls += 1;
            if calls == 1 {
                Err(crate::MuxError::Command(
                    "temporary refresh failure".to_owned(),
                ))
            } else {
                Ok(Vec::new())
            }
        });

        let failed = worker
            .snapshots
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(failed.panes.is_err());
        worker.request(1);
        let recovered = worker
            .snapshots
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(recovered.generation, 1);
        assert!(recovered.panes.is_ok());
    }
}
