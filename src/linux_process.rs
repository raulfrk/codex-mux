//! Linux `/proc` foreground-process inspection.

use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fs,
    io::{ErrorKind, Read},
    os::unix::{ffi::OsStringExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

/// Matches one live process against exact executable or trusted interpreted-script
/// file identities. Rollout recovery uses this same wrapper-aware identity rule.
pub(crate) fn pid_matches_file_identities(
    pid: u32,
    recognized: &[(u64, u64)],
    max_cmdline_bytes: u64,
) -> (bool, u64) {
    let directory = PathBuf::from(format!("/proc/{pid}"));
    let executable_link = directory.join("exe");
    let executable = match fs::read_link(directory.join("exe")) {
        Ok(path) => path,
        Err(_) => return (false, 0),
    };
    let direct = fs::metadata(&executable_link)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()));
    if direct.is_some_and(|identity| recognized.contains(&identity)) {
        let current = fs::metadata(&executable_link)
            .ok()
            .map(|metadata| (metadata.dev(), metadata.ino()));
        return (current == direct, 0);
    }
    if !is_trusted_interpreter(&executable) {
        return (false, 0);
    }
    let read_cmdline = || {
        let mut bytes = Vec::new();
        let read = fs::File::open(directory.join("cmdline"))
            .and_then(|file| {
                file.take(max_cmdline_bytes.saturating_add(1))
                    .read_to_end(&mut bytes)
            })
            .is_ok();
        let consumed = bytes.len() as u64;
        (
            (read && consumed <= max_cmdline_bytes).then_some(bytes),
            consumed,
        )
    };
    let (first, first_bytes) = read_cmdline();
    let Some(cmdline) = first else {
        return (false, first_bytes);
    };
    let arguments = cmdline
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(|argument| OsString::from_vec(argument.to_vec()))
        .collect::<Vec<_>>();
    let Some(script) = interpreted_script_argument(&executable, &arguments) else {
        return (false, cmdline.len() as u64);
    };
    let identity = fs::metadata(Path::new(script))
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()));
    let (second, second_bytes) = read_cmdline();
    (
        identity.is_some_and(|identity| recognized.contains(&identity))
            && second.as_deref() == Some(cmdline.as_slice())
            && fs::metadata(&executable_link)
                .ok()
                .map(|metadata| (metadata.dev(), metadata.ino()))
                == direct,
        first_bytes + second_bytes,
    )
}

use regex::Regex;

use crate::{
    MuxError, Result,
    config::MatchScope,
    domain::{CodexExecutable, PaneProcess, ProcessInspector, ProcessMatchIdentity},
};

/// Resolves foreground process groups from a configurable procfs root.
#[derive(Clone, Debug)]
pub struct LinuxProcessInspector {
    proc_root: PathBuf,
    recognized: Vec<CodexExecutable>,
    match_scope: MatchScope,
    match_command_regexes: Vec<Regex>,
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
            match_scope: MatchScope::Foreground,
            match_command_regexes: Vec::new(),
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
            match_scope: MatchScope::Foreground,
            match_command_regexes: Vec::new(),
        }
    }

    /// Creates an inspector with explicit candidate scope and normalized-argv regexes.
    pub fn with_matcher(
        recognized: Vec<CodexExecutable>,
        match_scope: MatchScope,
        match_command_regexes: &[String],
    ) -> Result<Self> {
        let primary = recognized
            .first()
            .cloned()
            .expect("validated non-empty matches");
        let regexes = match_command_regexes
            .iter()
            .map(|expression| {
                Regex::new(expression).map_err(|error| MuxError::InvalidValue {
                    field: "process match command regex",
                    message: format!("invalid regex {expression:?}: {error}"),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            proc_root: PathBuf::from("/proc"),
            recognized: if recognized.is_empty() {
                vec![primary]
            } else {
                recognized
            },
            match_scope,
            match_command_regexes: regexes,
        })
    }

    /// Creates a scoped matcher rooted at a hermetic procfs tree.
    pub fn with_proc_root_and_matcher(
        recognized: Vec<CodexExecutable>,
        match_scope: MatchScope,
        match_command_regexes: &[String],
        proc_root: impl Into<PathBuf>,
    ) -> Result<Self> {
        let mut inspector = Self::with_matcher(recognized, match_scope, match_command_regexes)?;
        inspector.proc_root = proc_root.into();
        Ok(inspector)
    }

    /// Returns whether the pane foreground matches any configured exact binary
    /// or interpreted-script identity using the same matcher as inventory.
    pub fn foreground_process_matches(&self, pane_pid: u32) -> Result<bool> {
        self.inspect(pane_pid)
            .map(|matched| {
                matched.is_some_and(|matched| {
                    self.recognized
                        .iter()
                        .any(|executable| same_file(&matched.path, executable.as_path()))
                })
            })
            .map_err(|source| MuxError::Filesystem {
                path: self.proc_root.clone(),
                source,
            })
    }

    /// Returns whether a pane matches using the configured scope and rules.
    pub fn pane_process_matches(&self, pane: &PaneProcess) -> Result<bool> {
        self.inspect_scoped(pane)
            .map(|matched| matched.is_some())
            .map_err(|source| MuxError::Filesystem {
                path: self.proc_root.clone(),
                source,
            })
    }

    /// Returns the stable identity of the process satisfying the pane matcher.
    pub fn pane_process_match_identity(
        &self,
        pane: &PaneProcess,
    ) -> Result<Option<ProcessMatchIdentity>> {
        self.inspect_scoped(pane)
            .map(|matched| {
                matched.and_then(|matched| matched.proven_match.then_some(matched.identity))
            })
            .map_err(|source| MuxError::Filesystem {
                path: self.proc_root.clone(),
                source,
            })
    }

    fn inspect_scoped(&self, pane: &PaneProcess) -> std::io::Result<Option<MatchedProcess>> {
        if self.match_scope == MatchScope::Foreground {
            return self.inspect(pane.pid);
        }
        let Some(initial) = self.read_process(pane.pid)? else {
            return Ok(None);
        };
        let snapshot = self.process_snapshot()?;
        let candidates = match self.match_scope {
            MatchScope::Foreground => unreachable!(),
            MatchScope::PaneTree => descendants(&snapshot, initial.pid),
            MatchScope::PaneTty => {
                let Some(tty_nr) = tty_number(&pane.tty) else {
                    return Ok(None);
                };
                if initial.tty_nr != tty_nr {
                    return Ok(None);
                }
                snapshot
                    .iter()
                    .filter(|candidate| candidate.tty_nr == tty_nr)
                    .collect()
            }
        };
        for allow_regex in [false, true] {
            for candidate in &candidates {
                let matched = if allow_regex {
                    self.command_regex_matches(candidate)
                        .then(|| self.recognized[0].as_path().to_owned())
                } else {
                    self.recognized_match(candidate)
                        .map(|path| path.as_path().to_owned())
                };
                let Some(path) = matched else { continue };
                if self.scoped_candidate_is_current(&initial, candidate)? {
                    return Ok(Some(MatchedProcess {
                        path,
                        identity: candidate.identity(),
                        proven_match: true,
                    }));
                }
            }
        }
        Ok(None)
    }

    fn scoped_candidate_is_current(
        &self,
        initial: &ProcessEvidence,
        candidate: &ProcessEvidence,
    ) -> std::io::Result<bool> {
        let current = self.process_snapshot()?;
        let Some(current_pane) = current.iter().find(|process| process.pid == initial.pid) else {
            return Ok(false);
        };
        let Some(current_candidate) = current.iter().find(|process| process.pid == candidate.pid)
        else {
            return Ok(false);
        };
        if !same_process_evidence(initial, current_pane)
            || !same_process_evidence(candidate, current_candidate)
        {
            return Ok(false);
        }
        Ok(match self.match_scope {
            MatchScope::Foreground => unreachable!(),
            MatchScope::PaneTree => descendants(&current, initial.pid)
                .iter()
                .any(|process| process.pid == candidate.pid),
            MatchScope::PaneTty => current_pane.tty_nr == initial.tty_nr,
        })
    }

    fn command_regex_matches(&self, process: &ProcessEvidence) -> bool {
        if self.match_command_regexes.is_empty() {
            return false;
        }
        let Some(command) = normalized_argv(&process.arguments) else {
            return false;
        };
        self.match_command_regexes
            .iter()
            .any(|regex| regex.is_match(&command))
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

    fn inspect(&self, pane_pid: u32) -> std::io::Result<Option<MatchedProcess>> {
        let pane = match self.read_process(pane_pid)? {
            Some(process) => process,
            None => return Ok(None),
        };
        if let Some(executable) = self.exact_recognized_match(&pane) {
            let Some(current) = self.read_process(pane_pid)? else {
                return Ok(None);
            };
            return Ok(
                same_process_evidence(&pane, &current).then(|| MatchedProcess {
                    path: executable.as_path().to_owned(),
                    identity: pane.identity(),
                    proven_match: true,
                }),
            );
        }
        if pane.tty_nr != 0
            && pane.tpgid > 0
            && pane.pgrp == pane.tpgid
            && (self.recognized_match(&pane).is_some() || self.command_regex_matches(&pane))
        {
            let Some(current) = self.read_process(pane_pid)? else {
                return Ok(None);
            };
            return Ok(
                same_process_evidence(&pane, &current).then(|| MatchedProcess {
                    path: self.recognized_match(&pane).map_or_else(
                        || self.recognized[0].as_path().to_owned(),
                        |executable| executable.as_path().to_owned(),
                    ),
                    identity: pane.identity(),
                    proven_match: true,
                }),
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
                .then(|| MatchedProcess {
                    path: executable.as_path().to_owned(),
                    identity: candidate.identity(),
                    proven_match: true,
                }));
            }
        }
        for candidate in &candidates {
            if self.command_regex_matches(candidate) {
                let Some(current_candidate) = self.read_process(candidate.pid)? else {
                    return Ok(None);
                };
                let Some(current_pane) = self.read_process(pane_pid)? else {
                    return Ok(None);
                };
                return Ok((same_process_evidence(candidate, &current_candidate)
                    && same_foreground_snapshot(&pane, &current_pane))
                .then(|| MatchedProcess {
                    path: self.recognized[0].as_path().to_owned(),
                    identity: candidate.identity(),
                    proven_match: true,
                }));
            }
        }

        // The process-group leader is the deterministic conservative fallback.
        // Inventory matching still rejects it unless canonical identity agrees.
        Ok(candidates
            .iter()
            .find(|process| i64::from(process.pid) == pane.tpgid)
            .and_then(|process| {
                process.executable.clone().map(|path| MatchedProcess {
                    path,
                    identity: process.identity(),
                    proven_match: false,
                })
            }))
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
                        candidates.iter().find_map(|process| {
                            self.command_regex_matches(process)
                                .then(|| (process, self.recognized[0].as_path().to_owned()))
                        })
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
        let Some((parent_pid, pgrp, tty_nr, tpgid, start_time)) = parse_stat(&stat) else {
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
        let Some((
            current_parent_pid,
            current_pgrp,
            current_tty_nr,
            current_tpgid,
            current_start_time,
        )) = fs::read(directory.join("stat"))
            .ok()
            .as_deref()
            .and_then(parse_stat)
        else {
            return Ok(None);
        };
        if (parent_pid, pgrp, tty_nr, tpgid, start_time)
            != (
                current_parent_pid,
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
            parent_pid,
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
            .map(|matched| matched.map(|matched| matched.path))
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

    fn pane_executable(&self, pane: &PaneProcess) -> Result<Option<PathBuf>> {
        self.inspect_scoped(pane)
            .map(|matched| matched.map(|matched| matched.path))
            .map_err(|source| MuxError::Filesystem {
                path: self.proc_root.clone(),
                source,
            })
    }

    fn pane_executables(&self, panes: &[PaneProcess]) -> Vec<Result<Option<PathBuf>>> {
        panes
            .iter()
            .map(|pane| self.pane_executable(pane))
            .collect()
    }
}

fn clone_io_error(error: &std::io::Error) -> std::io::Error {
    std::io::Error::new(error.kind(), error.to_string())
}

struct ProcessEvidence {
    pid: u32,
    parent_pid: u32,
    pgrp: i64,
    tty_nr: i64,
    tpgid: i64,
    start_time: u64,
    executable: Option<PathBuf>,
    arguments: Vec<OsString>,
}

impl ProcessEvidence {
    const fn identity(&self) -> ProcessMatchIdentity {
        ProcessMatchIdentity {
            pid: self.pid,
            start_time: self.start_time,
        }
    }
}

struct MatchedProcess {
    path: PathBuf,
    identity: ProcessMatchIdentity,
    proven_match: bool,
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
        && initial.parent_pid == current.parent_pid
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

fn parse_stat(stat: &[u8]) -> Option<(u32, i64, i64, i64, u64)> {
    // comm is parenthesized and may itself contain spaces or right parentheses;
    // the last `)` precedes the fixed-position numeric fields.
    let close = stat.iter().rposition(|byte| *byte == b')')?;
    let remainder = std::str::from_utf8(stat.get(close + 1..)?).ok()?;
    let fields = remainder.split_ascii_whitespace().collect::<Vec<_>>();
    if fields.len() < 20 {
        return None;
    }
    Some((
        fields[1].parse().ok()?,
        fields[2].parse().ok()?,
        fields[4].parse().ok()?,
        fields[5].parse().ok()?,
        fields[19].parse().ok()?,
    ))
}

fn descendants(processes: &[ProcessEvidence], root: u32) -> Vec<&ProcessEvidence> {
    let mut matched = vec![root];
    let mut cursor = 0;
    while cursor < matched.len() {
        let parent = matched[cursor];
        for process in processes {
            if process.parent_pid == parent && !matched.contains(&process.pid) {
                matched.push(process.pid);
            }
        }
        cursor += 1;
    }
    processes
        .iter()
        .filter(|process| matched.contains(&process.pid))
        .collect()
}

fn normalized_argv(arguments: &[OsString]) -> Option<String> {
    arguments
        .iter()
        .map(|argument| argument.to_str())
        .collect::<Option<Vec<_>>>()
        .map(|arguments| arguments.join(" "))
}

fn tty_number(path: &Path) -> Option<i64> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let metadata = fs::metadata(path).ok()?;
    if !path.is_absolute() || !metadata.file_type().is_char_device() {
        return None;
    }
    let rdev = metadata.rdev();
    let major = ((rdev >> 8) & 0x0fff) | ((rdev >> 32) & !0x0fff);
    let minor = (rdev & 0x00ff) | ((rdev >> 12) & !0x00ff);
    Some(
        (((major & 0x0fff) << 8)
            | (minor & 0x00ff)
            | ((minor & !0x00ff) << 12)
            | ((major & !0x0fff) << 32)) as i64,
    )
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
        "env", "sh", "bash", "dash", "zsh", "node", "nodejs", "bun", "deno", "python", "python3",
    ]
    .iter()
    .any(|wrapper| name == *wrapper)
        || name
            .to_str()
            .is_some_and(|name| name.starts_with("python3."));
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
    let candidates = arguments.get(1..)?;
    if name == "env" {
        for argument in candidates {
            let value = argument.to_str()?;
            if value.starts_with('-') {
                return None;
            }
            if !value.contains('=') {
                return Some(argument);
            }
        }
        return None;
    }
    let mut candidates = candidates.iter();
    let first = candidates.next()?;
    let first_value = first.to_str()?;
    if first_value == "--" {
        return candidates.next();
    }
    if name == "deno" {
        return (first_value == "run").then(|| candidates.next()).flatten();
    }
    // Interpreter flags have interpreter-specific operands and execution
    // modes (`-c`, `--rcfile`, `-X`, and similar). Treat all of them as
    // ambiguous rather than mistaking their data operand for a script path.
    (!first_value.starts_with('-')).then_some(first)
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::PathBuf,
        process::Command,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use super::{
        ProcessEvidence, descendants, parse_stat, pid_matches_file_identities,
        same_foreground_snapshot,
    };

    #[test]
    fn rollout_identity_accepts_an_actual_interpreted_launcher_script() {
        let script = std::env::temp_dir().join(format!(
            "codex-mux-interpreted-profile-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&script, b"#!/bin/sh\nsleep 5 &\nwait\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let metadata = fs::metadata(&script).unwrap();
        let mut child = Command::new(&script).spawn().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !pid_matches_file_identities(
            child.id(),
            &[(metadata.dev(), metadata.ino())],
            1024 * 1024,
        )
        .0 && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            pid_matches_file_identities(
                child.id(),
                &[(metadata.dev(), metadata.ino())],
                1024 * 1024,
            )
            .0
        );
        let _ = child.kill();
        let _ = child.wait();
        fs::remove_file(script).unwrap();
    }

    #[test]
    fn parses_stat_with_spaces_and_parentheses_in_comm() {
        let stat = b"42 (odd ) process) S 1 42 42 34816 42 0 0 0 0 0 0 0 0 0 0 0 0 0 1234";
        assert_eq!(parse_stat(stat), Some((1, 42, 34816, 42, 1234)));
    }

    #[test]
    fn malformed_stat_is_ignored() {
        assert_eq!(parse_stat(b"42 incomplete"), None);
    }

    #[test]
    fn foreground_snapshot_rejects_pid_reuse() {
        let evidence = |start_time| ProcessEvidence {
            pid: 42,
            parent_pid: 1,
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

    #[test]
    fn rebuilt_descendant_snapshot_rejects_a_reparented_chain() {
        let evidence = |pid: u32, parent_pid: u32| ProcessEvidence {
            pid,
            parent_pid,
            pgrp: pid.into(),
            tty_nr: 34816,
            tpgid: pid.into(),
            start_time: u64::from(pid),
            executable: Some(PathBuf::from("/opt/process")),
            arguments: vec![OsString::from("/opt/process")],
        };
        let original = vec![evidence(10, 1), evidence(20, 10), evidence(30, 20)];
        let reparented = vec![evidence(10, 1), evidence(20, 1), evidence(30, 20)];

        assert!(descendants(&original, 10).iter().any(|item| item.pid == 30));
        assert!(
            !descendants(&reparented, 10)
                .iter()
                .any(|item| item.pid == 30)
        );
    }
}
