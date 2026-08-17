//! Server-wide tmux pane discovery and conservative Codex identity matching.

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

use crate::{
    MuxError, Result,
    domain::{CodexExecutable, Pane, PaneId, ProcessInspector, SessionId, TmuxCommandRunner},
};

const FIELD_SEPARATOR: u8 = 0x1f;
const ESCAPED_FIELD_SEPARATOR: &[u8] = b"\\037";
const PANE_FORMAT: &str = "#{pane_id}\x1f#{session_id}\x1f#{window_id}\x1f#{window_name}\x1f#{pane_title}\x1f#{pane_current_path}\x1f#{pane_current_command}\x1f#{pane_pid}\x1f#{pane_tty}\x1f#{@codex_mux_generated_thread}\x1f#{@codex_mux_generated_name}\x1f#{@codex_mux_generated_at}";

/// Discovers Codex panes through injectable tmux and process boundaries.
pub struct PaneInventory<R, I> {
    runner: R,
    processes: I,
    codex_executables: Vec<CodexExecutable>,
}

impl<R, I> PaneInventory<R, I>
where
    R: TmuxCommandRunner,
    I: ProcessInspector,
{
    /// Creates an inventory for one configured Codex executable.
    #[must_use]
    pub fn new(runner: R, processes: I, codex: CodexExecutable) -> Self {
        Self {
            runner,
            processes,
            codex_executables: vec![codex],
        }
    }

    /// Creates an inventory that recognizes configured and profile-specific binaries.
    #[must_use]
    pub fn with_executables(
        runner: R,
        processes: I,
        codex_executables: Vec<CodexExecutable>,
    ) -> Self {
        Self {
            runner,
            processes,
            codex_executables,
        }
    }

    /// Lists every pane in the current server whose foreground process is Codex.
    ///
    /// Invalid rows and panes whose process has exited or cannot be inspected are
    /// omitted. A failed tmux command remains an error because no trustworthy
    /// server-wide inventory can be produced from it.
    pub fn discover(&self) -> Result<Vec<Pane>> {
        let arguments = [
            OsString::from("list-panes"),
            OsString::from("-a"),
            OsString::from("-F"),
            OsString::from(PANE_FORMAT),
        ];
        let output = self.runner.run(&arguments)?;
        if output.status != Some(0) {
            return Err(MuxError::Command(command_failure(
                &output.stderr,
                output.status,
            )));
        }

        let mut seen = HashSet::new();
        let mut records = Vec::new();
        for line in output.stdout.split(|byte| *byte == b'\n') {
            let Some(record) = TmuxPaneRecord::parse(line) else {
                continue;
            };
            if !seen.insert(record.pane_id.clone()) {
                continue;
            }
            records.push(record);
        }

        let pane_pids = records
            .iter()
            .map(|record| record.pane_pid)
            .collect::<Vec<_>>();
        let executables = self.processes.foreground_executables(&pane_pids);
        if executables.len() != records.len() {
            return Err(MuxError::Command(format!(
                "process inspector returned {} results for {} panes",
                executables.len(),
                records.len()
            )));
        }
        let mut panes = Vec::new();
        for (record, executable) in records.into_iter().zip(executables) {
            let Ok(Some(executable)) = executable else {
                continue;
            };
            if !self
                .codex_executables
                .iter()
                .any(|codex| matches_executable(&executable, codex.as_path(), &record.command))
            {
                continue;
            }

            let generated_title = record.generated_title();
            let generated_at_unix = record.generated_at();
            let Ok(id) = PaneId::new(record.pane_id) else {
                continue;
            };
            let Ok(session_id) = SessionId::new(record.session_id) else {
                continue;
            };
            panes.push(Pane {
                id,
                session_id,
                title: nonempty_title(record.title),
                generated_title,
                generated_at_unix,
                current_path: record.current_path,
            });
        }
        Ok(panes)
    }
}

fn command_failure(stderr: &[u8], status: Option<i32>) -> String {
    let detail = String::from_utf8_lossy(stderr);
    let detail = detail.trim();
    let status = status
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_owned());
    if detail.is_empty() {
        format!("tmux list-panes exited with {status}")
    } else {
        format!("tmux list-panes exited with {status}: {detail}")
    }
}

fn nonempty_title(title: String) -> Option<String> {
    if title.trim().is_empty() {
        None
    } else {
        Some(title)
    }
}

fn matches_executable(actual: &Path, configured: &Path, pane_command: &OsStr) -> bool {
    if actual == configured {
        return true;
    }

    let same_basename = actual.file_name().is_some()
        && actual.file_name() == configured.file_name()
        && pane_command == configured.file_name().unwrap_or_default();
    if !same_basename {
        return false;
    }

    // A basename is only a hint. Canonical file identity is mandatory so an
    // unrelated executable called `codex` cannot enter the inventory.
    match (actual.canonicalize(), configured.canonicalize()) {
        (Ok(actual), Ok(configured)) => actual == configured,
        _ => false,
    }
}

struct TmuxPaneRecord {
    pane_id: String,
    session_id: String,
    #[allow(dead_code)]
    window_id: String,
    #[allow(dead_code)]
    window_name: String,
    title: String,
    current_path: PathBuf,
    command: OsString,
    pane_pid: u32,
    #[allow(dead_code)]
    tty: OsString,
    generated_name: String,
    generated_thread: String,
    generated_at: String,
}

impl TmuxPaneRecord {
    fn parse(line: &[u8]) -> Option<Self> {
        if line.is_empty() || line.contains(&b'\r') {
            return None;
        }
        let fields = split_fields(line);
        if !matches!(fields.len(), 11 | 12) {
            return None;
        }

        let pane_id = utf8_nonempty(fields[0])?;
        let session_id = utf8_nonempty(fields[1])?;
        let window_id = utf8_nonempty(fields[2])?;
        let window_name = String::from_utf8_lossy(fields[3]).into_owned();
        let title = String::from_utf8_lossy(fields[4]).into_owned();
        let current_path = path_from_bytes(fields[5]);
        if !current_path.is_absolute() {
            return None;
        }
        let command = os_string_from_bytes(fields[6]);
        if command.is_empty() {
            return None;
        }
        let pane_pid = std::str::from_utf8(fields[7]).ok()?.parse().ok()?;
        if pane_pid == 0 || fields[8].is_empty() {
            return None;
        }

        Some(Self {
            pane_id,
            session_id,
            window_id,
            window_name,
            title,
            current_path,
            command,
            pane_pid,
            tty: os_string_from_bytes(fields[8]),
            generated_thread: String::from_utf8_lossy(fields[9]).into_owned(),
            generated_name: String::from_utf8_lossy(fields[10]).into_owned(),
            generated_at: fields.get(11).map_or_else(String::new, |field| {
                String::from_utf8_lossy(field).into_owned()
            }),
        })
    }

    fn generated_title(&self) -> Option<String> {
        if self.generated_at.parse::<u64>().is_ok()
            && thread_marker_matches_title(&self.generated_thread, &self.title)
            && !self.generated_name.trim().is_empty()
        {
            Some(self.generated_name.clone())
        } else {
            None
        }
    }

    fn generated_at(&self) -> Option<u64> {
        self.generated_title()?;
        self.generated_at.parse().ok()
    }
}

fn thread_marker_matches_title(thread: &str, title: &str) -> bool {
    if !looks_like_thread_id(thread) {
        return false;
    }
    if thread == title {
        return true;
    }
    title.strip_suffix("...").is_some_and(|prefix| {
        prefix.chars().filter(|character| *character != '-').count() >= 12
            && prefix.len() < 36
            && prefix
                .chars()
                .all(|character| character.is_ascii_hexdigit() || character == '-')
            && thread.starts_with(prefix)
    })
}

fn looks_like_thread_id(value: &str) -> bool {
    value.len() == 36
        && value.chars().enumerate().all(|(index, character)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                character == '-'
            } else {
                character.is_ascii_hexdigit()
            }
        })
}

fn split_fields(line: &[u8]) -> Vec<&[u8]> {
    if line.contains(&FIELD_SEPARATOR) {
        return line
            .split(|byte| *byte == FIELD_SEPARATOR)
            .collect::<Vec<_>>();
    }

    let mut fields = Vec::new();
    let mut start = 0;
    while let Some(offset) = line[start..]
        .windows(ESCAPED_FIELD_SEPARATOR.len())
        .position(|window| window == ESCAPED_FIELD_SEPARATOR)
    {
        let separator = start + offset;
        fields.push(&line[start..separator]);
        start = separator + ESCAPED_FIELD_SEPARATOR.len();
    }
    fields.push(&line[start..]);
    fields
}

fn utf8_nonempty(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    if text.trim().is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

#[cfg(unix)]
fn os_string_from_bytes(bytes: &[u8]) -> OsString {
    OsString::from_vec(bytes.to_vec())
}

#[cfg(not(unix))]
fn os_string_from_bytes(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(os_string_from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::{TmuxPaneRecord, matches_executable};
    use std::{ffi::OsStr, path::Path};

    #[test]
    fn parser_rejects_wrong_field_count_and_relative_paths() {
        assert!(TmuxPaneRecord::parse(b"%1\x1f$1").is_none());
        assert!(
            TmuxPaneRecord::parse(
                b"%1\x1f$1\x1f@1\x1fmain\x1ftitle\x1frelative\x1fcodex\x1f42\x1f/dev/pts/1\x1f\x1f"
            )
            .is_none()
        );
    }

    #[test]
    fn parser_accepts_tmux_34_octal_escaped_separators() {
        let record = TmuxPaneRecord::parse(
            b"%1\\037$1\\037@1\\037main\\037thread\\037/work/project\\037codex\\03742\\037/dev/pts/1\\037\\037",
        )
        .expect("tmux 3.4 record should parse");

        assert_eq!(record.pane_id, "%1");
        assert_eq!(record.session_id, "$1");
        assert_eq!(record.current_path, Path::new("/work/project"));
        assert_eq!(record.command, OsStr::new("codex"));
        assert_eq!(record.pane_pid, 42);
    }

    #[test]
    fn exact_executable_match_does_not_need_basename_fallback() {
        assert!(matches_executable(
            Path::new("/opt/codex/bin/renamed-agent"),
            Path::new("/opt/codex/bin/renamed-agent"),
            OsStr::new("anything"),
        ));
    }

    #[test]
    fn basename_collision_is_rejected() {
        assert!(!matches_executable(
            Path::new("/tmp/unrelated/codex"),
            Path::new("/opt/codex/bin/codex"),
            OsStr::new("codex"),
        ));
    }
}
