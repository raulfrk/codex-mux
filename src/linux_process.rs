//! Linux `/proc` foreground-process inspection.

use std::{
    env,
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

    /// Returns whether the pane's foreground process group contains the exact
    /// configured Codex executable.
    ///
    /// Unlike inventory discovery, this deliberately excludes wrappers. Smart
    /// Left uses it to keep its prefixless interception fail-closed while still
    /// supporting the normal case where tmux's pane PID is the parent shell.
    pub fn foreground_process_is_exact(&self, pane_pid: u32) -> Result<bool> {
        self.foreground_contains_exact(pane_pid)
            .map_err(|source| MuxError::Filesystem {
                path: self.proc_root.clone(),
                source,
            })
    }

    /// Returns whether the pane process itself is an exact foreground Bash or
    /// Zsh process with an interactive-compatible invocation shape.
    pub fn foreground_process_is_shell(&self, pane_pid: u32, command: &str) -> Result<bool> {
        self.foreground_is_shell(pane_pid, command)
            .map_err(|source| MuxError::Filesystem {
                path: self.proc_root.clone(),
                source,
            })
    }

    fn foreground_is_shell(&self, pane_pid: u32, command: &str) -> std::io::Result<bool> {
        if !matches!(command, "bash" | "zsh") {
            return Ok(false);
        }
        let Some(pane) = self.read_process(pane_pid)? else {
            return Ok(false);
        };
        if pane.tty_nr == 0
            || pane.tpgid <= 0
            || i64::from(pane.pid) != pane.pgrp
            || pane.pgrp != pane.tpgid
            || !pane
                .executable
                .as_deref()
                .is_some_and(|path| shell_executable_matches(path, command))
            || !interactive_shell_arguments(&pane.arguments, command)
        {
            return Ok(false);
        }
        let Some(current) = self.read_process(pane_pid)? else {
            return Ok(false);
        };
        Ok(same_foreground_snapshot(&pane, &current)
            && current.executable == pane.executable
            && current.arguments == pane.arguments)
    }

    fn foreground_contains_exact(&self, pane_pid: u32) -> std::io::Result<bool> {
        let Some(pane) = self.read_process(pane_pid)? else {
            return Ok(false);
        };
        if self.is_exact_configured_executable(&pane) {
            return Ok(true);
        }
        if pane.tty_nr == 0 || pane.tpgid <= 0 {
            return Ok(false);
        }

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
            if process.pgrp == pane.tpgid
                && process.tty_nr == pane.tty_nr
                && self.is_exact_configured_executable(&process)
            {
                let Some(current_pane) = self.read_process(pane_pid)? else {
                    return Ok(false);
                };
                return Ok(same_foreground_snapshot(&pane, &current_pane));
            }
        }
        Ok(false)
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
        let Some((pgrp, tty_nr, tpgid, start_time)) = parse_stat(&stat) else {
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
        let Some((current_pgrp, current_tty_nr, current_tpgid, current_start_time)) =
            fs::read(directory.join("stat"))
                .ok()
                .as_deref()
                .and_then(parse_stat)
        else {
            return Ok(None);
        };
        if (pgrp, tty_nr, tpgid, start_time)
            != (
                current_pgrp,
                current_tty_nr,
                current_tpgid,
                current_start_time,
            )
        {
            return Ok(None);
        }

        Ok(Some(ProcessEvidence {
            pid,
            pgrp,
            tty_nr,
            tpgid,
            start_time,
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
    start_time: u64,
    executable: Option<PathBuf>,
    arguments: Vec<OsString>,
}

fn same_foreground_snapshot(initial: &ProcessEvidence, current: &ProcessEvidence) -> bool {
    initial.pid == current.pid
        && initial.start_time == current.start_time
        && initial.tty_nr == current.tty_nr
        && initial.tpgid == current.tpgid
        && current.tty_nr != 0
        && current.tpgid > 0
}

fn interactive_shell_arguments(arguments: &[OsString], command: &str) -> bool {
    let Some(argv_zero) = arguments.first().and_then(|argument| argument.to_str()) else {
        return false;
    };
    if Path::new(argv_zero)
        .file_name()
        .and_then(|name| name.to_str())
        .is_none_or(|name| name.trim_start_matches('-') != command)
    {
        return false;
    }
    let mut index = 1;
    while index < arguments.len() {
        let Some(argument) = arguments[index].to_str() else {
            return false;
        };
        if argument == "--" {
            return index + 1 == arguments.len();
        }
        if argument == "--command"
            || (argument.starts_with('-')
                && !argument.starts_with("--")
                && argument[1..].contains('c'))
        {
            return false;
        }
        let takes_value = match command {
            "bash" => matches!(argument, "--rcfile" | "--init-file" | "-O" | "+O"),
            "zsh" => matches!(argument, "-o" | "+o"),
            _ => false,
        };
        if takes_value {
            index += 1;
            if index >= arguments.len() || arguments[index].is_empty() {
                return false;
            }
        } else if !(argument.starts_with('-') || argument.starts_with('+')) {
            return false;
        }
        index += 1;
    }
    true
}

fn shell_executable_matches(executable: &Path, command: &str) -> bool {
    executable.file_name() == Some(std::ffi::OsStr::new(command))
        && env::var_os("PATH")
            .into_iter()
            .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
            .map(|directory| directory.join(command))
            .any(|candidate| candidate.is_file() && same_file(&candidate, executable))
}

fn parse_stat(stat: &[u8]) -> Option<(i64, i64, i64, u64)> {
    // comm is parenthesized and may itself contain spaces or right parentheses;
    // the last `)` precedes the fixed-position numeric fields.
    let close = stat.iter().rposition(|byte| *byte == b')')?;
    let remainder = std::str::from_utf8(stat.get(close + 1..)?).ok()?;
    let fields = remainder.split_ascii_whitespace().collect::<Vec<_>>();
    if fields.len() < 20 {
        return None;
    }
    Some((
        fields[2].parse().ok()?,
        fields[4].parse().ok()?,
        fields[5].parse().ok()?,
        fields[19].parse().ok()?,
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
    use std::{ffi::OsString, path::PathBuf};

    use super::{ProcessEvidence, parse_stat, same_foreground_snapshot};

    #[test]
    fn parses_stat_with_spaces_and_parentheses_in_comm() {
        let stat = b"42 (odd ) process) S 1 42 42 34816 42 0 0 0 0 0 0 0 0 0 0 0 0 0 1234";
        assert_eq!(parse_stat(stat), Some((42, 34816, 42, 1234)));
    }

    #[test]
    fn malformed_stat_is_ignored() {
        assert_eq!(parse_stat(b"42 incomplete"), None);
    }

    #[test]
    fn foreground_snapshot_rejects_pid_reuse() {
        let evidence = |start_time| ProcessEvidence {
            pid: 42,
            pgrp: 42,
            tty_nr: 34816,
            tpgid: 42,
            start_time,
            executable: Some(PathBuf::from("/opt/codex")),
            arguments: vec![OsString::from("/opt/codex")],
        };

        assert!(same_foreground_snapshot(&evidence(100), &evidence(100)));
        assert!(!same_foreground_snapshot(&evidence(100), &evidence(101)));
    }
}
