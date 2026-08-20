//! Linux `/proc` foreground-process inspection.

use std::{
    collections::HashMap,
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
    recognized: Vec<CodexExecutable>,
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
            recognized: vec![codex.clone()],
        }
    }

    /// Creates a host inspector that recognizes the primary and profile binaries.
    #[must_use]
    pub fn with_executables(codex: CodexExecutable, recognized: Vec<CodexExecutable>) -> Self {
        Self::with_proc_root_and_executables(codex, recognized, "/proc")
    }

    /// Creates a host inspector accepting every exact configured process identity.
    #[must_use]
    pub fn matching_executables(recognized: Vec<CodexExecutable>) -> Self {
        let primary = recognized
            .first()
            .cloned()
            .expect("validated non-empty matches");
        Self::with_proc_root_and_executables(primary, recognized, "/proc")
    }

    /// Creates a multi-executable inspector with an alternate procfs root for tests.
    #[must_use]
    pub fn with_proc_root_and_executables(
        _codex: CodexExecutable,
        recognized: Vec<CodexExecutable>,
        proc_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            proc_root: proc_root.into(),
            recognized,
        }
    }

    /// Returns whether the pane foreground matches any configured exact binary
    /// or interpreted-script identity using the same matcher as inventory.
    pub fn foreground_process_matches(&self, pane_pid: u32) -> Result<bool> {
        self.inspect(pane_pid)
            .map(|matched| {
                matched.is_some_and(|path| {
                    self.recognized
                        .iter()
                        .any(|executable| same_file(&path, executable.as_path()))
                })
            })
            .map_err(|source| MuxError::Filesystem {
                path: self.proc_root.clone(),
                source,
            })
    }

    /// Legacy exact-binary probe retained for API compatibility.
    pub fn foreground_process_is_exact(&self, pane_pid: u32) -> Result<bool> {
        self.foreground_contains_exact(pane_pid)
            .map_err(|source| MuxError::Filesystem {
                path: self.proc_root.clone(),
                source,
            })
    }

    fn foreground_contains_exact(&self, pane_pid: u32) -> std::io::Result<bool> {
        let Some(pane) = self.read_process(pane_pid)? else {
            return Ok(false);
        };
        if self.exact_recognized_match(&pane).is_some() {
            return Ok(true);
        }
        if pane.tty_nr == 0 || pane.tpgid <= 0 {
            return Ok(false);
        }
        for process in self.process_snapshot()? {
            if process.pgrp == pane.tpgid
                && process.tty_nr == pane.tty_nr
                && self.exact_recognized_match(&process).is_some()
            {
                let Some(current) = self.read_process(pane_pid)? else {
                    return Ok(false);
                };
                return Ok(same_foreground_snapshot(&pane, &current));
            }
        }
        Ok(false)
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

    fn inspect(&self, pane_pid: u32) -> std::io::Result<Option<PathBuf>> {
        let pane = match self.read_process(pane_pid)? {
            Some(process) => process,
            None => return Ok(None),
        };
        if let Some(executable) = self.exact_recognized_match(&pane) {
            let Some(current) = self.read_process(pane_pid)? else {
                return Ok(None);
            };
            return Ok(
                same_process_evidence(&pane, &current).then(|| executable.as_path().to_owned())
            );
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
            if let Some(executable) = self.recognized_match(candidate) {
                let Some(current_candidate) = self.read_process(candidate.pid)? else {
                    return Ok(None);
                };
                let Some(current_pane) = self.read_process(pane_pid)? else {
                    return Ok(None);
                };
                return Ok((same_process_evidence(candidate, &current_candidate)
                    && same_foreground_snapshot(&pane, &current_pane))
                .then(|| executable.as_path().to_owned()));
            }
        }

        // The process-group leader is the deterministic conservative fallback.
        // Inventory matching still rejects it unless canonical identity agrees.
        Ok(candidates
            .iter()
            .find(|process| i64::from(process.pid) == pane.tpgid)
            .and_then(|process| process.executable.clone()))
    }

    fn inspect_batch(&self, pane_pids: &[u32]) -> Vec<std::io::Result<Option<PathBuf>>> {
        let recognized_paths = self
            .recognized
            .iter()
            .map(|executable| {
                executable
                    .as_path()
                    .canonicalize()
                    .unwrap_or_else(|_| executable.as_path().to_owned())
            })
            .collect::<Vec<_>>();
        let panes = pane_pids
            .iter()
            .map(|pane_pid| self.read_process(*pane_pid))
            .collect::<Vec<_>>();
        let needs_snapshot = panes.iter().any(|pane| {
            pane.as_ref()
                .ok()
                .and_then(Option::as_ref)
                .is_some_and(|pane| {
                    self.exact_recognized_match_cached(pane, &recognized_paths)
                        .is_none()
                        && pane.tty_nr != 0
                        && pane.tpgid > 0
                })
        });
        let snapshot = if needs_snapshot {
            self.process_snapshot().map(|processes| {
                let mut groups = HashMap::<(i64, i64), Vec<ProcessEvidence>>::new();
                for process in processes {
                    groups
                        .entry((process.pgrp, process.tty_nr))
                        .or_default()
                        .push(process);
                }
                groups
            })
        } else {
            Ok(HashMap::new())
        };

        panes
            .into_iter()
            .map(|pane| {
                let pane = pane?;
                let Some(pane) = pane else {
                    return Ok(None);
                };
                if let Some(executable) =
                    self.exact_recognized_match_cached(&pane, &recognized_paths)
                {
                    let Some(current) = self.read_process(pane.pid)? else {
                        return Ok(None);
                    };
                    return Ok(same_process_evidence(&pane, &current)
                        .then(|| executable.as_path().to_owned()));
                }
                if pane.tty_nr == 0 || pane.tpgid <= 0 {
                    return Ok(None);
                }

                let groups = snapshot.as_ref().map_err(clone_io_error)?;
                let candidates = groups
                    .get(&(pane.tpgid, pane.tty_nr))
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let matched = candidates
                    .iter()
                    .find_map(|process| {
                        self.recognized_match_cached(process, &recognized_paths)
                            .map(|executable| (process, executable.as_path().to_owned()))
                    })
                    .or_else(|| {
                        candidates
                            .iter()
                            .filter(|process| i64::from(process.pid) == pane.tpgid)
                            .find_map(|process| {
                                process
                                    .executable
                                    .clone()
                                    .map(|executable| (process, executable))
                            })
                    });
                let Some((matched_process, matched)) = matched else {
                    return Ok(None);
                };
                let Some(current) = self.read_process(pane.pid)? else {
                    return Ok(None);
                };
                let Some(current_match) = self.read_process(matched_process.pid)? else {
                    return Ok(None);
                };
                Ok((same_foreground_snapshot(&pane, &current)
                    && same_process_evidence(matched_process, &current_match))
                .then_some(matched))
            })
            .collect()
    }

    fn recognized_match_cached<'a>(
        &'a self,
        process: &ProcessEvidence,
        recognized_paths: &[PathBuf],
    ) -> Option<&'a CodexExecutable> {
        process
            .executable
            .as_deref()
            .and_then(|path| self.configured_match(path, recognized_paths))
            .or_else(|| {
                process
                    .executable
                    .as_deref()
                    .filter(|path| is_trusted_interpreter(path))
                    .and_then(|path| interpreted_script_argument(path, &process.arguments))
                    .map(Path::new)
                    .and_then(|path| self.configured_match(path, recognized_paths))
            })
    }

    fn exact_recognized_match_cached<'a>(
        &'a self,
        process: &ProcessEvidence,
        recognized_paths: &[PathBuf],
    ) -> Option<&'a CodexExecutable> {
        process
            .executable
            .as_deref()
            .and_then(|path| self.configured_match(path, recognized_paths))
    }

    fn configured_match<'a>(
        &'a self,
        candidate: &Path,
        recognized_paths: &[PathBuf],
    ) -> Option<&'a CodexExecutable> {
        let canonical = candidate.canonicalize().ok();
        self.recognized
            .iter()
            .zip(recognized_paths)
            .find(|(executable, configured)| {
                candidate == executable.as_path()
                    || canonical
                        .as_deref()
                        .is_some_and(|candidate| candidate == configured.as_path())
            })
            .map(|(executable, _)| executable)
    }

    fn process_snapshot(&self) -> std::io::Result<Vec<ProcessEvidence>> {
        let mut processes = Vec::new();
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
            if let Ok(Some(process)) = self.read_process(pid) {
                processes.push(process);
            }
        }
        processes.sort_by_key(|process| process.pid);
        Ok(processes)
    }

    fn recognized_match(&self, process: &ProcessEvidence) -> Option<&CodexExecutable> {
        self.recognized.iter().find(|executable| {
            process
                .executable
                .as_deref()
                .is_some_and(|path| same_file(path, executable.as_path()))
                || (process
                    .executable
                    .as_deref()
                    .is_some_and(is_trusted_interpreter)
                    && process
                        .executable
                        .as_deref()
                        .and_then(|path| interpreted_script_argument(path, &process.arguments))
                        .is_some_and(|argument| {
                            same_file(Path::new(argument), executable.as_path())
                        }))
        })
    }

    fn exact_recognized_match(&self, process: &ProcessEvidence) -> Option<&CodexExecutable> {
        let actual = process.executable.as_deref()?;
        self.recognized
            .iter()
            .find(|executable| same_file(actual, executable.as_path()))
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

    fn foreground_executables(&self, pane_pids: &[u32]) -> Vec<Result<Option<PathBuf>>> {
        self.inspect_batch(pane_pids)
            .into_iter()
            .map(|result| {
                result.map_err(|source| MuxError::Filesystem {
                    path: self.proc_root.clone(),
                    source,
                })
            })
            .collect()
    }
}

fn clone_io_error(error: &std::io::Error) -> std::io::Error {
    std::io::Error::new(error.kind(), error.to_string())
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

fn same_process_evidence(initial: &ProcessEvidence, current: &ProcessEvidence) -> bool {
    initial.pid == current.pid
        && initial.pgrp == current.pgrp
        && initial.tty_nr == current.tty_nr
        && initial.tpgid == current.tpgid
        && initial.start_time == current.start_time
        && initial.executable == current.executable
        && initial.arguments == current.arguments
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

fn is_trusted_interpreter(executable: &Path) -> bool {
    let Some(name) = executable.file_name() else {
        return false;
    };
    let known_name = [
        "env", "sh", "bash", "dash", "zsh", "node", "nodejs", "bun", "deno",
    ]
    .iter()
    .any(|wrapper| name == *wrapper);
    if !known_name {
        return false;
    }
    ["/usr/bin", "/bin"]
        .into_iter()
        .map(|directory| Path::new(directory).join(name))
        .chain(
            env::var_os("PATH")
                .into_iter()
                .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
                .map(|directory| directory.join(name)),
        )
        .any(|candidate| candidate.is_file() && same_file(&candidate, executable))
}

fn interpreted_script_argument<'a>(
    executable: &Path,
    arguments: &'a [OsString],
) -> Option<&'a OsString> {
    let name = executable.file_name()?.to_str()?;
    let mut candidates = arguments.iter().skip(1);
    if name == "env" {
        return candidates.find(|argument| {
            argument
                .to_str()
                .is_some_and(|value| !value.starts_with('-') && !value.contains('='))
        });
    }
    candidates.find(|argument| {
        argument
            .to_str()
            .is_some_and(|value| !value.starts_with('-'))
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
