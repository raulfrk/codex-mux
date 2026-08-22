//! Stable domain values and injectable application ports.

use std::{
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
};

use serde::{Deserialize, Serialize};

use crate::{MuxError, Result};

macro_rules! identifier {
    ($name:ident, $field:literal, $docs:literal) => {
        #[doc = $docs]
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            #[doc = concat!("Creates a validated `", stringify!($name), "`.")]
            pub fn new(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                if value.trim().is_empty() {
                    return Err(MuxError::InvalidValue {
                        field: $field,
                        message: "must not be empty".to_owned(),
                    });
                }
                Ok(Self(value))
            }

            #[doc = concat!("Returns this `", stringify!($name), "` as text.")]
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

identifier!(
    PaneId,
    "pane ID",
    "Stable tmux pane identity used for targeting."
);
identifier!(
    ClientId,
    "client ID",
    "Exact tmux client identity that invoked the popup."
);
identifier!(
    SessionId,
    "session ID",
    "Stable tmux session identity used for new windows."
);
identifier!(
    WindowId,
    "window ID",
    "Stable tmux window identity used for adjacent window creation."
);

/// An absolute executable path used for Codex discovery and launches.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexExecutable(PathBuf);

impl CodexExecutable {
    /// Creates an executable path, rejecting relative or empty paths.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if path.as_os_str().is_empty() || !path.is_absolute() {
            return Err(MuxError::InvalidValue {
                field: "Codex executable",
                message: "must be an absolute path".to_owned(),
            });
        }
        Ok(Self(path))
    }

    /// Returns the configured executable path.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// A discoverable Codex pane and the data allowed in the visible row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pane {
    /// Stable target retained internally and never rendered as row text.
    pub id: PaneId,
    /// Session that owns this pane.
    pub session_id: SessionId,
    /// Codex thread title exposed through tmux's supported pane-title channel.
    pub title: Option<String>,
    /// Smart title owned by codex-mux, when the tmux ownership marker is valid.
    pub generated_title: Option<String>,
    /// Exact thread identity that owns the generated title metadata.
    pub generated_thread_id: Option<String>,
    /// Whether the live Codex title itself proves that thread identity.
    pub generated_source_stable: bool,
    /// Unix timestamp of the last successful smart-title generation.
    pub generated_at_unix: Option<u64>,
    /// A pane-local request to refresh its smart title without waiting for the
    /// normal refresh interval.
    pub immediate_naming: bool,
    /// A user-owned pane title that Smart Naming must never replace.
    pub manual_name: bool,
    /// Original Codex thread title retained while a pane is manually named.
    pub manual_name_source: Option<String>,
    /// Pane leader PID captured when the manual name was saved.
    pub manual_name_pid: Option<u32>,
    /// Exact pane-local PID metadata retained for stale-action guards, including legacy text.
    pub manual_name_pid_raw: String,
    /// Tmux session captured when the manual name was saved.
    pub manual_name_session: Option<SessionId>,
    /// Exact pane-local session metadata retained for stale-action guards, including legacy text.
    pub manual_name_session_raw: String,
    /// A source-less manual unpin awaiting a changed exact Codex thread title.
    pub unpin_waiting: bool,
    /// Manual title captured before source-less unpin.
    pub unpin_waiting_title: Option<String>,
    /// Pane leader captured before source-less unpin.
    pub unpin_waiting_pid: Option<u32>,
    /// Tmux session captured before source-less unpin.
    pub unpin_waiting_session: Option<SessionId>,
    /// Live pane leader PID used to reject stale rename/unpin actions.
    pub pane_pid: u32,
    /// Current working directory exposed by tmux.
    pub current_path: PathBuf,
}

impl Pane {
    /// Returns the visible title, falling back to the current directory name.
    #[must_use]
    pub fn display_title(&self) -> String {
        self.generated_title
            .as_deref()
            .or(self.title.as_deref())
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| {
                self.current_path
                    .file_name()
                    .filter(|name| !name.is_empty())
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "unnamed".to_owned())
    }
}

/// Context supplied by the tmux binding for client-scoped operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvocationContext {
    /// Exact invoking tmux client.
    pub client_id: ClientId,
    /// Pane from which the popup was opened.
    pub pane_id: PaneId,
    /// Session in which new windows should be created.
    pub session_id: SessionId,
    /// Window after which new windows should be inserted.
    pub window_id: WindowId,
    /// Working-directory fallback when no selected pane is available.
    pub current_path: PathBuf,
}

/// Tmux evidence used to select candidate processes for a pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneProcess {
    /// Tmux pane leader PID.
    pub pid: u32,
    /// Tmux pane TTY path, when tmux supplied one.
    pub tty: PathBuf,
}

/// Stable identity of the exact process that satisfied a pane match.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessMatchIdentity {
    /// Matched process ID.
    pub pid: u32,
    /// Kernel process start time, preventing PID-reuse equivalence.
    pub start_time: u64,
}

/// Terminal dimensions used to select a responsive layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSize {
    /// Terminal width in cells.
    pub width: u16,
    /// Terminal height in cells.
    pub height: u16,
}

impl TerminalSize {
    /// Returns true when the approved full-screen popup breakpoint applies.
    #[must_use]
    pub const fn is_compact(self) -> bool {
        self.width < 90 || self.height < 28
    }
}

/// Built-in visual profile.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeId {
    /// Cyan-accented first-run profile.
    #[default]
    AdaptiveCyan,
    /// Blue command-palette profile.
    BlueCommandPalette,
    /// Amber operator profile.
    AmberOperator,
    /// Orange-accented profile.
    EmberOrange,
    /// Color-free profile.
    Monochrome,
}

impl ThemeId {
    /// All themes in picker order.
    pub const ALL: [Self; 5] = [
        Self::AdaptiveCyan,
        Self::BlueCommandPalette,
        Self::AmberOperator,
        Self::EmberOrange,
        Self::Monochrome,
    ];

    /// Stable configuration name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AdaptiveCyan => "adaptive-cyan",
            Self::BlueCommandPalette => "blue-command-palette",
            Self::AmberOperator => "amber-operator",
            Self::EmberOrange => "ember-orange",
            Self::Monochrome => "monochrome",
        }
    }
}

impl fmt::Display for ThemeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(formatter)
    }
}

impl FromStr for ThemeId {
    type Err = MuxError;

    fn from_str(value: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|theme| theme.as_str() == value)
            .ok_or_else(|| MuxError::InvalidValue {
                field: "theme",
                message: format!("unknown profile {value:?}"),
            })
    }
}

/// Raw output from an executed tmux command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutput {
    /// Standard output bytes.
    pub stdout: Vec<u8>,
    /// Standard error bytes.
    pub stderr: Vec<u8>,
    /// Process exit code, or `None` when terminated by signal.
    pub status: Option<i32>,
}

/// Injectable boundary for invoking tmux without shell interpolation.
pub trait TmuxCommandRunner {
    /// Executes tmux with an argument vector.
    fn run(&self, arguments: &[OsString]) -> Result<CommandOutput>;
}

/// Injectable boundary for Linux foreground-process discovery.
pub trait ProcessInspector {
    /// Resolves the foreground executable associated with a tmux pane process.
    fn foreground_executable(&self, pane_pid: u32) -> Result<Option<PathBuf>>;

    /// Resolves a pane using its PID and exact tmux TTY evidence.
    fn pane_executable(&self, pane: &PaneProcess) -> Result<Option<PathBuf>> {
        self.foreground_executable(pane.pid)
    }

    /// Resolves a pane set through one batch boundary.
    ///
    /// Implementations may override this to share one coherent process snapshot.
    /// The returned vector must contain exactly one positionally corresponding
    /// result for every input PID.
    fn foreground_executables(&self, pane_pids: &[u32]) -> Vec<Result<Option<PathBuf>>> {
        pane_pids
            .iter()
            .map(|pane_pid| self.foreground_executable(*pane_pid))
            .collect()
    }

    /// Resolves panes through one batch boundary.
    fn pane_executables(&self, panes: &[PaneProcess]) -> Vec<Result<Option<PathBuf>>> {
        let pids = panes.iter().map(|pane| pane.pid).collect::<Vec<_>>();
        self.foreground_executables(&pids)
    }
}

/// Read/write boundary for the codex-mux-owned theme preference.
pub trait ThemeStore {
    /// Loads a saved theme, returning `None` when no preference exists.
    fn load(&self) -> Result<Option<ThemeId>>;

    /// Atomically persists a selected theme.
    fn save(&self, theme: ThemeId) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{CodexExecutable, Pane, PaneId, SessionId, TerminalSize, ThemeId};

    #[test]
    fn identifiers_reject_empty_values() {
        assert!(PaneId::new("  ").is_err());
        assert!(PaneId::new("%7").is_ok());
    }

    #[test]
    fn codex_executable_must_be_absolute() {
        assert!(CodexExecutable::new("codex").is_err());
        assert!(CodexExecutable::new("/opt/codex/bin/codex").is_ok());
    }

    #[test]
    fn pane_title_falls_back_to_project_directory() {
        let pane = Pane {
            id: PaneId::new("%3").unwrap(),
            session_id: SessionId::new("$1").unwrap(),
            title: None,
            generated_title: None,
            generated_thread_id: None,
            generated_source_stable: false,
            generated_at_unix: None,
            immediate_naming: false,
            manual_name: false,

            manual_name_source: None,

            manual_name_pid: None,

            manual_name_pid_raw: String::new(),

            manual_name_session: None,

            manual_name_session_raw: String::new(),

            unpin_waiting: false,
            unpin_waiting_title: None,
            unpin_waiting_pid: None,
            unpin_waiting_session: None,

            pane_pid: 100,
            current_path: PathBuf::from("/work/codex-mux"),
        };

        assert_eq!(pane.display_title(), "codex-mux");
    }

    #[test]
    fn compact_breakpoint_matches_contract() {
        assert!(
            !TerminalSize {
                width: 90,
                height: 28,
            }
            .is_compact()
        );
        assert!(
            TerminalSize {
                width: 89,
                height: 40,
            }
            .is_compact()
        );
        assert!(
            TerminalSize {
                width: 120,
                height: 27,
            }
            .is_compact()
        );
    }

    #[test]
    fn themes_round_trip_through_stable_names() {
        for theme in ThemeId::ALL {
            assert_eq!(theme.as_str().parse::<ThemeId>().unwrap(), theme);
        }
    }
}
