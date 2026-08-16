//! Linux `/proc` foreground-process inspection.

use std::{
    ffi::OsString,
    fs,
    io::ErrorKind,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

use crate::{
    MuxError, Result,
    domain::{CodexExecutable, ProcessInspector},
};

/// Resolves foreground process groups from a configurable procfs root.
#[derive(Clone, Debug)]
pub struct LinuxProcessInspector {
    proc_root: PathBuf,
    codex: CodexExecutable,
}

impl LinuxProcessInspector {
    /// Creates an inspector for the host `/proc` filesystem.
    #[must_use]
    pub fn new(codex: CodexExecutable) -> Self {
        Self::with_proc_root(codex, "/proc")
    }

    /// Creates an inspector with an alternate procfs root for hermetic tests.
    #[must_use]
    pub fn with_proc_root(codex: CodexExecutable, proc_root: impl Into<PathBuf>) -> Self {
        Self {
            proc_root: proc_root.into(),
            codex,
        }
    }

    fn inspect(&self, pane_pid: u32) -> std::io::Result<Option<PathBuf>> {
        let pane = match self.read_process(pane_pid)? {
            Some(process) => process,
            None => return Ok(None),
        };
        if self.is_exact_configured_executable(&pane) {
            return Ok(Some(self.codex.as_path().to_owned()));
        }
        if pane.tty_nr == 0 || pane.tpgid <= 0 {
            return Ok(None);
        }

        let mut candidates = Vec::new();
        for entry in fs::read_dir(&self.proc_root)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
            else {
                continue;
            };
            let process = match self.read_process(pid) {
                Ok(Some(process)) => process,
                Ok(None) | Err(_) => continue,
            };
            if process.pgrp == pane.tpgid && process.tty_nr == pane.tty_nr {
                candidates.push(process);
            }
        }
        candidates.sort_by_key(|process| process.pid);

        // Prefer incontrovertible identity anywhere in the foreground group.
        // This covers an `exec`-style rename and wrappers whose argv retains the
        // configured absolute launcher path.
        for candidate in &candidates {
            if self.is_configured(candidate) {
                return Ok(Some(self.codex.as_path().to_owned()));
            }
        }

        // The process-group leader is the deterministic conservative fallback.
        // Inventory matching still rejects it unless canonical identity agrees.
        Ok(candidates
            .iter()
            .find(|process| i64::from(process.pid) == pane.tpgid)
            .and_then(|process| process.executable.clone()))
    }

    fn is_configured(&self, process: &ProcessEvidence) -> bool {
        if self.is_exact_configured_executable(process) {
            return true;
        }

        process.executable.as_deref().is_some_and(is_wrapper)
            && process
                .arguments
                .get(1)
                .is_some_and(|argument| same_file(Path::new(argument), self.codex.as_path()))
    }

    fn is_exact_configured_executable(&self, process: &ProcessEvidence) -> bool {
        process
            .executable
            .as_deref()
            .is_some_and(|path| same_file(path, self.codex.as_path()))
    }

    fn read_process(&self, pid: u32) -> std::io::Result<Option<ProcessEvidence>> {
        let directory = self.proc_root.join(pid.to_string());
        let stat = match fs::read(directory.join("stat")) {
            Ok(stat) => stat,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::PermissionDenied
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let Some((pgrp, tty_nr, tpgid)) = parse_stat(&stat) else {
            return Ok(None);
        };
        let executable = fs::read_link(directory.join("exe")).ok();
        let arguments = fs::read(directory.join("cmdline"))
            .map(|bytes| {
                bytes
                    .split(|byte| *byte == 0)
                    .filter(|argument| !argument.is_empty())
                    .map(|argument| OsString::from_vec(argument.to_vec()))
                    .collect()
            })
            .unwrap_or_default();

        Ok(Some(ProcessEvidence {
            pid,
            pgrp,
            tty_nr,
            tpgid,
            executable,
            arguments,
        }))
    }
}

impl ProcessInspector for LinuxProcessInspector {
    fn foreground_executable(&self, pane_pid: u32) -> Result<Option<PathBuf>> {
        self.inspect(pane_pid)
            .map_err(|source| MuxError::Filesystem {
                path: self.proc_root.clone(),
                source,
            })
    }
}

struct ProcessEvidence {
    pid: u32,
    pgrp: i64,
    tty_nr: i64,
    tpgid: i64,
    executable: Option<PathBuf>,
    arguments: Vec<OsString>,
}

fn parse_stat(stat: &[u8]) -> Option<(i64, i64, i64)> {
    // comm is parenthesized and may itself contain spaces or right parentheses;
    // the last `)` precedes the fixed-position numeric fields.
    let close = stat.iter().rposition(|byte| *byte == b')')?;
    let remainder = std::str::from_utf8(stat.get(close + 1..)?).ok()?;
    let fields = remainder.split_ascii_whitespace().collect::<Vec<_>>();
    if fields.len() < 6 {
        return None;
    }
    Some((
        fields[2].parse().ok()?,
        fields[4].parse().ok()?,
        fields[5].parse().ok()?,
    ))
}

fn same_file(candidate: &Path, configured: &Path) -> bool {
    candidate == configured
        || matches!(
            (candidate.canonicalize(), configured.canonicalize()),
            (Ok(candidate), Ok(configured)) if candidate == configured
        )
}

fn is_wrapper(executable: &Path) -> bool {
    executable.file_name().is_some_and(|name| {
        [
            "env", "sh", "bash", "dash", "zsh", "node", "nodejs", "bun", "deno",
        ]
        .iter()
        .any(|wrapper| name == *wrapper)
    })
}

#[cfg(test)]
mod tests {
    use super::parse_stat;

    #[test]
    fn parses_stat_with_spaces_and_parentheses_in_comm() {
        let stat = b"42 (odd ) process) S 1 42 42 34816 42 0 0 0";
        assert_eq!(parse_stat(stat), Some((42, 34816, 42)));
    }

    #[test]
    fn malformed_stat_is_ignored() {
        assert_eq!(parse_stat(b"42 incomplete"), None);
    }
}
