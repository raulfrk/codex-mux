//! Shell-free execution of tmux commands.

use std::{ffi::OsString, path::PathBuf, process::Command};

use crate::{
    MuxError, Result,
    domain::{CommandOutput, TmuxCommandRunner},
};

/// Runs tmux directly with an argument vector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemTmuxRunner {
    executable: PathBuf,
}

impl Default for SystemTmuxRunner {
    fn default() -> Self {
        Self::new("tmux")
    }
}

impl SystemTmuxRunner {
    /// Creates a runner using `executable` as the tmux program.
    #[must_use]
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }
}

impl TmuxCommandRunner for SystemTmuxRunner {
    fn run(&self, arguments: &[OsString]) -> Result<CommandOutput> {
        let output = Command::new(&self.executable)
            .args(arguments)
            .output()
            .map_err(|source| MuxError::Command(format!("could not start tmux: {source}")))?;

        Ok(CommandOutput {
            stdout: output.stdout,
            stderr: output.stderr,
            status: output.status.code(),
        })
    }
}
