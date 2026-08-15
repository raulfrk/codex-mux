//! Command-line contract for interactive use and tmux configuration management.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

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

    /// Exact tmux client that opened the popup.
    #[arg(long, value_name = "CLIENT")]
    pub client: Option<String>,

    /// tmux pane from which the popup was opened.
    #[arg(long, value_name = "PANE")]
    pub invoking_pane: Option<String>,

    /// tmux session in which new Codex windows should be created.
    #[arg(long, value_name = "SESSION")]
    pub invoking_session: Option<String>,

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
    /// Manage the tmux prefix binding owned by codex-mux.
    Tmux(TmuxArgs),
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
        assert_eq!(install.config, Some(PathBuf::from("/home/me/.tmux.conf")));
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
