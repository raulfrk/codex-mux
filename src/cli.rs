//! Command-line contract for interactive use and tmux configuration management.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::config::MatchScope;

/// Parsed `codex-mux` command line.
#[derive(Clone, Debug, Eq, Parser, PartialEq)]
#[command(
    name = "codex-mux",
    version,
    about = "Switch between Codex sessions running in tmux",
    long_about = None
)]
pub struct Cli {
    /// Absolute Codex executable used for pane discovery and launches.
    #[arg(long, global = true, value_name = "PATH")]
    pub codex: Option<PathBuf>,

    /// Absolute executable used only to launch Codex and its app-server.
    #[arg(long, global = true, value_name = "PATH", conflicts_with = "codex")]
    pub launch_executable: Option<PathBuf>,

    /// Absolute executable or interpreted-script path accepted during discovery.
    #[arg(long, global = true, value_name = "PATH", conflicts_with = "codex")]
    pub match_executable: Vec<PathBuf>,

    /// Exact tmux pane_current_command value accepted by Smart Left.
    #[arg(long, global = true, value_name = "COMMAND", conflicts_with = "codex")]
    pub pane_command: Vec<String>,

    /// Process candidates accepted during discovery.
    #[arg(long, global = true, value_name = "SCOPE", conflicts_with = "codex")]
    pub match_scope: Option<MatchScope>,

    /// Regex applied to a normalized readable process argv during discovery.
    #[arg(long, global = true, value_name = "REGEX", conflicts_with = "codex")]
    pub match_command_regex: Vec<String>,

    /// Regex applied to tmux pane_current_command before Smart Left probes.
    #[arg(long, global = true, value_name = "REGEX", conflicts_with = "codex")]
    pub pane_command_regex: Vec<String>,

    /// Exact tmux client that opened the popup.
    #[arg(long, value_name = "CLIENT")]
    pub client: Option<String>,

    /// tmux pane from which the popup was opened.
    #[arg(long, value_name = "PANE")]
    pub invoking_pane: Option<String>,

    /// tmux session in which new Codex windows should be created.
    #[arg(long, value_name = "SESSION")]
    pub invoking_session: Option<String>,

    /// tmux window after which new Codex windows should be created.
    #[arg(long, value_name = "WINDOW")]
    pub invoking_window: Option<String>,

    /// Working-directory fallback supplied by the invoking pane.
    #[arg(long, value_name = "PATH")]
    pub invoking_path: Option<PathBuf>,

    /// Optional management command. Without one, the interactive TUI runs.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Top-level management commands.
#[derive(Clone, Debug, Eq, PartialEq, Subcommand)]
pub enum Command {
    /// Download, verify, and atomically install a Codex Mux release.
    Update(UpdateArgs),
    /// Configure tmux plus prompt-aware Bash and Zsh Smart Left.
    Setup(SetupArgs),
    /// Remove owned tmux, Bash, and Zsh configuration blocks.
    Remove(RemoveArgs),
    /// Manage the tmux prefix binding owned by codex-mux.
    Tmux(TmuxArgs),
    /// Internal entrypoint used by the marker-managed Smart Left binding.
    #[command(hide = true)]
    SmartLeft,
    /// Internal tmux-server-scoped smart-naming daemon.
    #[command(hide = true)]
    SmartNamingWorker,
    /// Internal launcher for the tmux-owned smart-naming daemon.
    #[command(hide = true)]
    SmartNamingStart,
}

/// Optional exact release selected by the self-update command.
#[derive(Clone, Debug, Eq, PartialEq, Args)]
pub struct UpdateArgs {
    /// Stable release version; omit to install the latest stable release.
    #[arg(value_name = "VERSION")]
    pub version: Option<String>,
}

/// Standard and explicitly overridden configuration paths for setup.
#[derive(Clone, Debug, Eq, PartialEq, Args)]
pub struct SetupArgs {
    /// Key pressed after the tmux prefix.
    #[arg(long, default_value = "a", value_name = "KEY")]
    pub key: String,

    /// Explicit host-owned tmux entrypoint.
    #[arg(long, value_name = "PATH")]
    pub tmux_config: Option<PathBuf>,

    /// Bash startup file; defaults to HOME/.bashrc.
    #[arg(long, value_name = "PATH")]
    pub bash_config: Option<PathBuf>,

    /// Zsh startup file; defaults to ZDOTDIR/.zshrc or HOME/.zshrc.
    #[arg(long, value_name = "PATH")]
    pub zsh_config: Option<PathBuf>,
}

/// Standard and explicitly overridden configuration paths for removal.
#[derive(Clone, Debug, Eq, PartialEq, Args)]
pub struct RemoveArgs {
    /// Explicit host-owned tmux entrypoint.
    #[arg(long, value_name = "PATH")]
    pub tmux_config: Option<PathBuf>,

    /// Bash startup file; defaults to HOME/.bashrc.
    #[arg(long, value_name = "PATH")]
    pub bash_config: Option<PathBuf>,

    /// Zsh startup file; defaults to ZDOTDIR/.zshrc or HOME/.zshrc.
    #[arg(long, value_name = "PATH")]
    pub zsh_config: Option<PathBuf>,
}

/// Arguments for the `tmux` command group.
#[derive(Clone, Debug, Eq, PartialEq, Args)]
pub struct TmuxArgs {
    /// tmux binding operation.
    #[command(subcommand)]
    pub command: TmuxCommand,
}

/// Operations on the marker-managed tmux binding.
#[derive(Clone, Debug, Eq, PartialEq, Subcommand)]
pub enum TmuxCommand {
    /// Install or update the marker-managed binding.
    Install(InstallArgs),
    /// Inspect installation state without writing.
    Status(ConfigPathArgs),
    /// Remove only the marker-managed binding.
    Uninstall(ConfigPathArgs),
}

/// Arguments controlling binding installation.
#[derive(Clone, Debug, Eq, PartialEq, Args)]
pub struct InstallArgs {
    /// Key pressed after the tmux prefix.
    #[arg(long, default_value = "a", value_name = "KEY")]
    pub key: String,

    /// Open codex-mux when Left cannot move the focused Codex composer cursor.
    #[arg(long)]
    pub smart_left: bool,

    /// Explicit host-owned tmux entrypoint.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

/// Optional explicit tmux entrypoint used by status and uninstall.
#[derive(Clone, Debug, Eq, PartialEq, Args)]
pub struct ConfigPathArgs {
    /// Explicit host-owned tmux entrypoint.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;

    use super::{Cli, Command, TmuxCommand};

    #[test]
    fn parses_custom_install_contract() {
        let cli = Cli::try_parse_from([
            "codex-mux",
            "--codex",
            "/opt/tools/codex-custom",
            "tmux",
            "install",
            "--key",
            "g",
            "--config",
            "/home/me/.tmux.conf",
        ])
        .unwrap();

        assert_eq!(cli.codex, Some(PathBuf::from("/opt/tools/codex-custom")));
        let Some(Command::Tmux(tmux)) = cli.command else {
            panic!("expected tmux command");
        };
        let TmuxCommand::Install(install) = tmux.command else {
            panic!("expected install command");
        };
        assert_eq!(install.key, "g");
        assert!(!install.smart_left);
        assert_eq!(install.config, Some(PathBuf::from("/home/me/.tmux.conf")));
    }

    #[test]
    fn parses_opt_in_smart_left_installation() {
        let cli = Cli::try_parse_from(["codex-mux", "tmux", "install", "--smart-left"]).unwrap();
        let Some(Command::Tmux(tmux)) = cli.command else {
            panic!("expected tmux command");
        };
        let TmuxCommand::Install(install) = tmux.command else {
            panic!("expected install command");
        };
        assert!(install.smart_left);
    }

    #[test]
    fn parses_scoped_regex_process_overrides() {
        let cli = Cli::try_parse_from([
            "codex-mux",
            "--launch-executable",
            "/opt/launcher",
            "--match-executable",
            "/opt/launcher",
            "--match-scope",
            "pane-tree",
            "--match-command-regex",
            "launcher-[0-9]+",
            "--pane-command-regex",
            "^supervisor$",
            "setup",
        ])
        .unwrap();
        assert_eq!(cli.match_scope.unwrap().to_string(), "pane-tree");
        assert_eq!(cli.match_command_regex, ["launcher-[0-9]+"]);
        assert_eq!(cli.pane_command_regex, ["^supervisor$"]);
    }

    #[test]
    fn parses_zero_argument_setup_and_remove() {
        let setup = Cli::try_parse_from(["codex-mux", "setup"]).unwrap();
        let Some(Command::Setup(setup)) = setup.command else {
            panic!("expected setup command");
        };
        assert_eq!(setup.key, "a");
        assert_eq!(setup.tmux_config, None);
        assert_eq!(setup.bash_config, None);
        assert_eq!(setup.zsh_config, None);

        let remove = Cli::try_parse_from(["codex-mux", "remove"]).unwrap();
        let Some(Command::Remove(remove)) = remove.command else {
            panic!("expected remove command");
        };
        assert_eq!(remove.tmux_config, None);
        assert_eq!(remove.bash_config, None);
        assert_eq!(remove.zsh_config, None);
    }

    #[test]
    fn parses_latest_and_exact_update_requests() {
        let latest = Cli::try_parse_from(["codex-mux", "update"]).unwrap();
        let Some(Command::Update(latest)) = latest.command else {
            panic!("expected update command");
        };
        assert_eq!(latest.version, None);

        let exact = Cli::try_parse_from(["codex-mux", "update", "v0.5.0"]).unwrap();
        let Some(Command::Update(exact)) = exact.command else {
            panic!("expected update command");
        };
        assert_eq!(exact.version.as_deref(), Some("v0.5.0"));
    }

    #[test]
    fn parses_interactive_invocation_context() {
        let cli = Cli::try_parse_from([
            "codex-mux",
            "--client",
            "/dev/pts/3",
            "--invoking-pane",
            "%7",
            "--invoking-session",
            "$2",
            "--invoking-path",
            "/work/project",
        ])
        .unwrap();

        assert!(cli.command.is_none());
        assert_eq!(cli.client.as_deref(), Some("/dev/pts/3"));
        assert_eq!(cli.invoking_pane.as_deref(), Some("%7"));
        assert_eq!(cli.invoking_session.as_deref(), Some("$2"));
        assert_eq!(cli.invoking_path, Some(PathBuf::from("/work/project")));
    }
}
