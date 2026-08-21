//! Shared error types.

use std::path::PathBuf;

/// Result type used throughout `codex-mux`.
pub type Result<T> = std::result::Result<T, MuxError>;

/// Errors produced by core contracts and application adapters.
#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    /// A required identifier or path did not satisfy a domain invariant.
    #[error("invalid {field}: {message}")]
    InvalidValue {
        /// Name of the invalid field.
        field: &'static str,
        /// Human-readable reason the value is invalid.
        message: String,
    },

    /// A filesystem operation failed for a known path.
    #[error("filesystem operation failed for {path}: {source}")]
    Filesystem {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// A child process could not be launched or completed unsuccessfully.
    #[error("command failed: {0}")]
    Command(String),

    /// A launch committed, but the invoking tmux client could not be selected.
    #[error("created Codex pane {pane}, but could not select it: {detail}")]
    CreatedPaneNotSelected {
        /// Exact pane created by the successful launch.
        pane: String,
        /// Selection failure reported by tmux.
        detail: String,
    },

    /// Cooperative cancellation interrupted an in-flight operation.
    #[error("operation cancelled")]
    Cancelled,

    /// The requested surface exists in the CLI but is not wired yet.
    #[error("{0} is not available in this bootstrap build")]
    Unavailable(&'static str),
}
