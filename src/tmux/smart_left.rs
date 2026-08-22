//! Fail-through Smart Left probe for the Codex composer.

use std::{ffi::OsString, path::Path, thread, time::Duration};

use regex::Regex;

use crate::{
    MuxError, Result,
    domain::{
        CodexExecutable, CommandOutput, InvocationContext, PaneProcess, ProcessMatchIdentity,
        TmuxCommandRunner,
    },
    linux_process::LinuxProcessInspector,
    smart_naming::NamingDiagnostics,
};

const FIELD_SEPARATOR: char = '\u{1f}';
const ESCAPED_FIELD_SEPARATOR: &str = "\\037";
const SHELL_SETTLE_INTERVAL: Duration = Duration::from_millis(30);
const CODEX_REDRAW_SETTLE_INTERVAL: Duration = Duration::from_millis(30);
const CODEX_SNAPSHOT_RETRY_INTERVAL: Duration = Duration::from_millis(10);
const CODEX_SNAPSHOT_ATTEMPTS: usize = 3;

/// Observable result of one Smart Left gesture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmartLeftOutcome {
    /// Left was delivered and no popup was opened.
    Forwarded,
    /// Left was delivered at the composer boundary and the mux popup was opened.
    Opened,
}

/// Exact foreground-process identity required before prefixless interception.
pub trait DirectCodexInspector {
    /// Returns true only when the pane includes a configured Codex process.
    fn is_direct_codex(&self, pane: &PaneProcess) -> Result<bool>;

    /// Returns the stable identity of the configured process matched in the pane.
    ///
    /// The default preserves the pre-0.10 implementation contract. The Linux
    /// runtime overrides it with the exact matched descendant PID/start time.
    fn direct_codex_identity(&self, pane: &PaneProcess) -> Result<Option<ProcessMatchIdentity>> {
        self.is_direct_codex(pane).map(|matched| {
            matched.then_some(ProcessMatchIdentity {
                pid: pane.pid,
                start_time: 0,
            })
        })
    }

    /// Returns true only for an interactive Bash or Zsh process that is the
    /// pane's exact foreground process.
    fn is_direct_shell(&self, pid: u32, command: &str) -> Result<bool>;
}

impl DirectCodexInspector for LinuxProcessInspector {
    fn is_direct_codex(&self, pane: &PaneProcess) -> Result<bool> {
        self.pane_process_match_identity(pane)
            .map(|identity| identity.is_some())
    }

    fn direct_codex_identity(&self, pane: &PaneProcess) -> Result<Option<ProcessMatchIdentity>> {
        self.pane_process_match_identity(pane)
    }

    fn is_direct_shell(&self, pid: u32, command: &str) -> Result<bool> {
        self.foreground_process_is_shell(pid, command)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SmartLeftTarget {
    Codex,
    Shell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ComposerBoundary {
    Exact,
    RowRejected,
    CursorRejected,
    SnapshotUnstable,
}

/// Injectable delay boundary for deterministic probe tests.
pub trait ProbeSleeper {
    /// Waits between rendered-cursor observations.
    fn sleep(&self, duration: Duration);

    /// Returns a blocking tmux-side delay when the probe can batch observations.
    ///
    /// Test sleepers use the default host-side wait so fake runners remain
    /// deterministic; the runtime sleeper moves the delay into tmux's command
    /// queue to avoid launching one tmux client per observation.
    fn tmux_sleep_command(&self, duration: Duration) -> Option<OsString> {
        self.sleep(duration);
        None
    }
}

/// Host thread sleeper used by the runtime.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemSleeper;

impl ProbeSleeper for SystemSleeper {
    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }

    fn tmux_sleep_command(&self, duration: Duration) -> Option<OsString> {
        Some(OsString::from(format!(
            "sleep {}.{:03}",
            duration.as_secs(),
            duration.subsec_millis()
        )))
    }
}

/// Runs the bounded Smart Left decision against one exact tmux pane and client.
pub struct SmartLeftProbe<'a, Runner, Inspector, Sleeper> {
    runner: &'a Runner,
    inspector: &'a Inspector,
    sleeper: &'a Sleeper,
    mux: &'a Path,
    codex: &'a CodexExecutable,
    pane_commands: &'a [String],
    match_executables: &'a [CodexExecutable],
    pane_command_regexes: &'a [String],
    match_scope: &'a str,
    match_command_regexes: &'a [String],
    diagnostics: Option<NamingDiagnostics>,
}

/// Shared launch and detection values propagated by the Smart Left probe.
pub struct SmartLeftMatcher<'a> {
    /// Exact pane-command prefilters.
    pub pane_commands: &'a [String],
    /// Exact executable/script identities.
    pub match_executables: &'a [CodexExecutable],
    /// Regex pane-command prefilters.
    pub pane_command_regexes: &'a [String],
    /// Candidate process scope.
    pub match_scope: &'a str,
    /// Regex fallbacks for normalized process argv.
    pub match_command_regexes: &'a [String],
}

impl<'a, Runner, Inspector, Sleeper> SmartLeftProbe<'a, Runner, Inspector, Sleeper>
where
    Runner: TmuxCommandRunner,
    Inspector: DirectCodexInspector,
    Sleeper: ProbeSleeper,
{
    /// Creates a probe with explicit, injectable host boundaries.
    #[must_use]
    pub const fn new(
        runner: &'a Runner,
        inspector: &'a Inspector,
        sleeper: &'a Sleeper,
        mux: &'a Path,
        codex: &'a CodexExecutable,
    ) -> Self {
        Self {
            runner,
            inspector,
            sleeper,
            mux,
            codex,
            pane_commands: &[],
            match_executables: &[],
            pane_command_regexes: &[],
            match_scope: "foreground",
            match_command_regexes: &[],
            diagnostics: None,
        }
    }

    /// Creates a probe with the exact configured tmux command prefilter values.
    #[must_use]
    pub const fn with_pane_commands(
        runner: &'a Runner,
        inspector: &'a Inspector,
        sleeper: &'a Sleeper,
        mux: &'a Path,
        codex: &'a CodexExecutable,
        pane_commands: &'a [String],
        match_executables: &'a [CodexExecutable],
    ) -> Self {
        Self {
            runner,
            inspector,
            sleeper,
            mux,
            codex,
            pane_commands,
            match_executables,
            pane_command_regexes: &[],
            match_scope: "foreground",
            match_command_regexes: &[],
            diagnostics: None,
        }
    }

    /// Adds regex pane-command prefilters to the shared process probe.
    #[must_use]
    pub const fn with_process_matcher(
        runner: &'a Runner,
        inspector: &'a Inspector,
        sleeper: &'a Sleeper,
        mux: &'a Path,
        codex: &'a CodexExecutable,
        matcher: SmartLeftMatcher<'a>,
    ) -> Self {
        Self {
            runner,
            inspector,
            sleeper,
            mux,
            codex,
            pane_commands: matcher.pane_commands,
            match_executables: matcher.match_executables,
            pane_command_regexes: matcher.pane_command_regexes,
            match_scope: matcher.match_scope,
            match_command_regexes: matcher.match_command_regexes,
            diagnostics: None,
        }
    }

    /// Enables privacy-safe decision reason logging for the runtime probe.
    #[must_use]
    pub fn with_diagnostics(mut self, diagnostics: NamingDiagnostics) -> Self {
        self.diagnostics = Some(diagnostics);
        self
    }

    fn event(&self, code: &'static str) {
        if let Some(diagnostics) = &self.diagnostics {
            diagnostics.event(code);
        }
    }

    /// Forwards Left exactly once and opens immediately at a proven boundary.
    pub fn run(&self, context: &InvocationContext) -> Result<SmartLeftOutcome> {
        let initial = match self.read_state(context) {
            Ok(state) if state.cursor_visible && !state.pane_in_mode => {
                let pane_command_allowed = self.pane_commands.is_empty()
                    || self
                        .pane_commands
                        .iter()
                        .any(|command| command == &state.pane_command)
                    || self.pane_command_regexes.iter().any(|expression| {
                        Regex::new(expression)
                            .is_ok_and(|regex| regex.is_match(&state.pane_command))
                    });
                let codex_identity = pane_command_allowed
                    .then(|| {
                        self.inspector.direct_codex_identity(&PaneProcess {
                            pid: state.pane_pid,
                            tty: state.pane_tty.clone(),
                        })
                    })
                    .transpose()
                    .ok()
                    .flatten()
                    .flatten();
                let target = if codex_identity.is_some() {
                    Some((SmartLeftTarget::Codex, codex_identity))
                } else if state.shell_prompt
                    && self
                        .inspector
                        .is_direct_shell(state.pane_pid, &state.pane_command)
                        .unwrap_or(false)
                {
                    Some((SmartLeftTarget::Shell, None))
                } else {
                    None
                };
                if target.is_none() {
                    self.event(if pane_command_allowed {
                        "process_rejected"
                    } else {
                        "pane_command_rejected"
                    });
                }
                target.map(|(target, identity)| (state, target, identity))
            }
            Ok(state) => {
                self.event(if state.pane_in_mode {
                    "pane_mode_active"
                } else {
                    "cursor_hidden"
                });
                None
            }
            Err(_) => {
                self.event("state_read_failed");
                None
            }
        };

        let Some((mut initial, target, initial_process_identity)) = initial else {
            self.send_left(context)?;
            return Ok(SmartLeftOutcome::Forwarded);
        };

        let boundary_is_exact = match target {
            SmartLeftTarget::Codex => match self.composer_boundary(context, &initial) {
                (ComposerBoundary::Exact, stable) => {
                    initial = stable;
                    true
                }
                (ComposerBoundary::RowRejected, _) => {
                    self.event("composer_row_rejected");
                    false
                }
                (ComposerBoundary::CursorRejected, _) => {
                    self.event("composer_cursor_rejected");
                    false
                }
                (ComposerBoundary::SnapshotUnstable, _) => {
                    self.event("composer_snapshot_unstable");
                    false
                }
            },
            // Shell prompts may occupy any number of columns. An unchanged
            // immediate post-key observation proves the editing boundary.
            SmartLeftTarget::Shell => true,
        };
        self.send_left(context)?;
        if !boundary_is_exact {
            return Ok(SmartLeftOutcome::Forwarded);
        }

        // tmux can finish writing to a PTY before the application consumes and
        // redraws the key. Shells always need a bounded settle; Codex gets one
        // only after an observed state change so its stable fast path stays fast.
        if target == SmartLeftTarget::Shell {
            self.sleeper.sleep(SHELL_SETTLE_INTERVAL);
        }

        let Ok(mut current) = self.read_state(context) else {
            self.event("state_recheck_failed");
            return Ok(SmartLeftOutcome::Forwarded);
        };
        if target == SmartLeftTarget::Codex && !state_is_unchanged(&initial, &current) {
            self.sleeper.sleep(CODEX_REDRAW_SETTLE_INTERVAL);
            let Ok(settled) = self.read_state(context) else {
                self.event("state_recheck_failed");
                return Ok(SmartLeftOutcome::Forwarded);
            };
            current = settled;
        }
        if !state_is_unchanged(&initial, &current) {
            self.event("state_changed_after_left");
            return Ok(SmartLeftOutcome::Forwarded);
        }

        let still_exact = match target {
            SmartLeftTarget::Codex => self
                .inspector
                .direct_codex_identity(&PaneProcess {
                    pid: current.pane_pid,
                    tty: current.pane_tty.clone(),
                })
                .map(|identity| identity == initial_process_identity),
            SmartLeftTarget::Shell => self
                .inspector
                .is_direct_shell(current.pane_pid, &current.pane_command),
        }
        .unwrap_or(false);
        if !still_exact {
            self.event("process_changed_after_left");
            return Ok(SmartLeftOutcome::Forwarded);
        }
        if self.open_popup(context).is_err() {
            self.event("client_context_changed");
            return Ok(SmartLeftOutcome::Forwarded);
        }
        self.event("popup_opened");
        Ok(SmartLeftOutcome::Opened)
    }

    fn read_state(&self, context: &InvocationContext) -> Result<PaneState> {
        let format = self.state_format();
        let output = self.run_checked(vec![
            OsString::from("display-message"),
            OsString::from("-p"),
            OsString::from("-t"),
            OsString::from(context.pane_id.as_str()),
            OsString::from(format),
        ])?;
        PaneState::parse(&output.stdout)
    }

    fn state_format(&self) -> String {
        format!(
            "#{{pane_pid}}{FIELD_SEPARATOR}#{{pane_tty}}{FIELD_SEPARATOR}#{{cursor_x}}{FIELD_SEPARATOR}#{{cursor_y}}{FIELD_SEPARATOR}#{{cursor_flag}}{FIELD_SEPARATOR}#{{pane_in_mode}}{FIELD_SEPARATOR}#{{pane_current_command}}{FIELD_SEPARATOR}#{{@codex_mux_shell_prompt}}{FIELD_SEPARATOR}#{{pane_width}}{FIELD_SEPARATOR}#{{pane_height}}{FIELD_SEPARATOR}#{{session_id}}{FIELD_SEPARATOR}#{{window_id}}"
        )
    }

    fn send_left(&self, context: &InvocationContext) -> Result<()> {
        self.run_checked(os_strings([
            "send-keys",
            "-t",
            context.pane_id.as_str(),
            "Left",
        ]))?;
        Ok(())
    }

    fn composer_boundary(
        &self,
        context: &InvocationContext,
        initial: &PaneState,
    ) -> (ComposerBoundary, PaneState) {
        // Cursor metadata and captured cells are separate tmux observations.
        // Reasoning-mode redraws can move the composer between them, so accept
        // a row only when the same pane/cursor evidence brackets its capture.
        let mut before = initial.clone();
        for attempt in 0..CODEX_SNAPSHOT_ATTEMPTS {
            let boundary =
                self.capture_composer_boundary(context, before.cursor_x, before.cursor_y);
            let Ok(after) = self.read_state(context) else {
                return (ComposerBoundary::SnapshotUnstable, before);
            };
            if state_is_unchanged(&before, &after) {
                return (boundary, after);
            }
            if !same_pane_identity(&before, &after) || !after.cursor_visible || after.pane_in_mode {
                return (ComposerBoundary::SnapshotUnstable, after);
            }
            before = after;
            if attempt + 1 < CODEX_SNAPSHOT_ATTEMPTS {
                self.sleeper.sleep(CODEX_SNAPSHOT_RETRY_INTERVAL);
            }
        }
        (ComposerBoundary::SnapshotUnstable, before)
    }

    fn capture_composer_boundary(
        &self,
        context: &InvocationContext,
        cursor_x: u16,
        cursor_y: u16,
    ) -> ComposerBoundary {
        let output = self.run_checked(os_strings([
            "capture-pane",
            "-p",
            "-t",
            context.pane_id.as_str(),
        ]));
        let Ok(output) = output else {
            return ComposerBoundary::RowRejected;
        };
        let Ok(screen) = std::str::from_utf8(&output.stdout) else {
            return ComposerBoundary::RowRejected;
        };
        screen
            .lines()
            .nth(usize::from(cursor_y))
            .map_or(ComposerBoundary::RowRejected, |line| {
                let indentation = line.len() - line.trim_start_matches(' ').len();
                let line = &line[indentation..];
                let prompt_width = if line == "›" {
                    1
                } else if line.starts_with("› ") {
                    2
                } else {
                    return ComposerBoundary::RowRejected;
                };
                let cursor_x = usize::from(cursor_x);
                if cursor_x == indentation
                    || cursor_x == indentation + 1
                    || cursor_x == indentation + prompt_width
                {
                    ComposerBoundary::Exact
                } else {
                    ComposerBoundary::CursorRejected
                }
            })
    }

    /// Opens the same responsive popup used after a successful Smart Left probe.
    pub fn open_popup(&self, context: &InvocationContext) -> Result<()> {
        let dimensions = self.run_checked(vec![
            OsString::from("list-clients"),
            OsString::from("-F"),
            OsString::from(format!(
                "#{{client_tty}}{FIELD_SEPARATOR}#{{client_width}}{FIELD_SEPARATOR}#{{client_height}}{FIELD_SEPARATOR}#{{session_id}}{FIELD_SEPARATOR}#{{window_id}}{FIELD_SEPARATOR}#{{pane_id}}"
            )),
        ])?;
        let (width, height) = dimensions
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .find_map(|line| {
                let fields = split_fields(line).ok()?;
                (fields.len() == 6
                    && fields[0] == context.client_id.as_str()
                    && fields[3] == context.session_id.as_str()
                    && fields[4] == context.window_id.as_str()
                    && fields[5] == context.pane_id.as_str())
                .then(|| {
                    Ok((
                        parse_u16(fields[1], "client width")?,
                        parse_u16(fields[2], "client height")?,
                    ))
                })
            })
            .transpose()?
            .ok_or_else(|| {
                MuxError::Command("tmux did not return the exact invoking client".to_owned())
            })?;
        let compact = width < 90 || height < 28;
        let popup_width = if compact { "100%" } else { "80%" };
        let popup_height = if compact { "100%" } else { "70%" };
        let mut command = vec![shell_literal(
            self.mux.as_os_str().to_string_lossy().as_ref(),
        )];
        if self.match_executables.is_empty() {
            command.extend([
                "--codex".to_owned(),
                shell_literal(self.codex.as_path().as_os_str().to_string_lossy().as_ref()),
            ]);
        } else {
            command.extend([
                "--launch-executable".to_owned(),
                shell_literal(self.codex.as_path().as_os_str().to_string_lossy().as_ref()),
            ]);
            for executable in self.match_executables {
                command.push("--match-executable".to_owned());
                command.push(shell_literal(
                    executable.as_path().as_os_str().to_string_lossy().as_ref(),
                ));
            }
            for pane_command in self.pane_commands {
                command.push("--pane-command".to_owned());
                command.push(shell_literal(pane_command));
            }
            command.push("--match-scope".to_owned());
            command.push(shell_literal(self.match_scope));
            for expression in self.match_command_regexes {
                command.push("--match-command-regex".to_owned());
                command.push(shell_literal(expression));
            }
            for expression in self.pane_command_regexes {
                command.push("--pane-command-regex".to_owned());
                command.push(shell_literal(expression));
            }
        }
        command.extend([
            "--client".to_owned(),
            shell_literal(context.client_id.as_str()),
            "--invoking-pane".to_owned(),
            shell_literal(context.pane_id.as_str()),
            "--invoking-session".to_owned(),
            shell_literal(context.session_id.as_str()),
            "--invoking-window".to_owned(),
            shell_literal(context.window_id.as_str()),
            "--invoking-path".to_owned(),
            shell_literal(context.current_path.as_os_str().to_string_lossy().as_ref()),
        ]);
        let command = command.join(" ");
        self.run_checked(vec![
            OsString::from("display-popup"),
            OsString::from("-E"),
            OsString::from("-c"),
            OsString::from(context.client_id.as_str()),
            // Target both objects explicitly. tmux resolves them independently,
            // so the popup child performs the authoritative association preflight
            // before initializing the mux.
            OsString::from("-t"),
            OsString::from(context.pane_id.as_str()),
            OsString::from("-d"),
            context.current_path.as_os_str().to_owned(),
            OsString::from("-w"),
            OsString::from(popup_width),
            OsString::from("-h"),
            OsString::from(popup_height),
            OsString::from(command),
        ])?;
        Ok(())
    }

    fn run_checked(&self, arguments: Vec<OsString>) -> Result<CommandOutput> {
        let output = self.runner.run(&arguments)?;
        if output.status == Some(0) {
            return Ok(output);
        }
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(MuxError::Command(if detail.is_empty() {
            "tmux command failed".to_owned()
        } else {
            detail
        }))
    }
}

fn state_is_unchanged(initial: &PaneState, observed: &PaneState) -> bool {
    observed.pane_pid == initial.pane_pid
        && observed.pane_tty == initial.pane_tty
        && observed.cursor_x == initial.cursor_x
        && observed.cursor_y == initial.cursor_y
        && observed.pane_width == initial.pane_width
        && observed.pane_height == initial.pane_height
        && observed.session_id == initial.session_id
        && observed.window_id == initial.window_id
        && observed.cursor_visible
        && !observed.pane_in_mode
        && observed.pane_command == initial.pane_command
        && observed.shell_prompt == initial.shell_prompt
}

fn same_pane_identity(initial: &PaneState, observed: &PaneState) -> bool {
    observed.pane_pid == initial.pane_pid
        && observed.pane_tty == initial.pane_tty
        && observed.pane_command == initial.pane_command
        && observed.shell_prompt == initial.shell_prompt
        && observed.session_id == initial.session_id
        && observed.window_id == initial.window_id
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PaneState {
    pane_pid: u32,
    pane_tty: std::path::PathBuf,
    cursor_x: u16,
    cursor_y: u16,
    cursor_visible: bool,
    pane_in_mode: bool,
    pane_command: String,
    shell_prompt: bool,
    pane_width: u16,
    pane_height: u16,
    session_id: String,
    window_id: String,
}

impl PaneState {
    fn parse(output: &[u8]) -> Result<Self> {
        let fields = split_fields(output)?;
        if fields.len() != 7 && fields.len() != 8 && fields.len() != 12 {
            return Err(MuxError::Command(
                "tmux returned malformed Smart Left pane state".to_owned(),
            ));
        }
        let offset = usize::from(fields.len() != 7);
        let extended = fields.len() == 12;
        Ok(Self {
            pane_pid: parse_u32(fields[0], "pane PID")?,
            pane_tty: if offset == 1 {
                std::path::PathBuf::from(fields[1])
            } else {
                std::path::PathBuf::new()
            },
            cursor_x: parse_u16(fields[1 + offset], "cursor x")?,
            cursor_y: parse_u16(fields[2 + offset], "cursor y")?,
            cursor_visible: parse_flag(fields[3 + offset], "cursor flag")?,
            pane_in_mode: parse_flag(fields[4 + offset], "pane mode")?,
            pane_command: fields[5 + offset].to_owned(),
            shell_prompt: match fields[6 + offset] {
                "" | "0" => false,
                "1" => true,
                value => {
                    return Err(MuxError::InvalidValue {
                        field: "shell prompt marker",
                        message: format!("tmux returned {value:?}"),
                    });
                }
            },
            pane_width: if extended {
                parse_u16(fields[8], "pane width")?
            } else {
                0
            },
            pane_height: if extended {
                parse_u16(fields[9], "pane height")?
            } else {
                0
            },
            session_id: if extended {
                fields[10].to_owned()
            } else {
                String::new()
            },
            window_id: if extended {
                fields[11].to_owned()
            } else {
                String::new()
            },
        })
    }
}

fn split_fields(output: &[u8]) -> Result<Vec<&str>> {
    let text = std::str::from_utf8(output)
        .map_err(|_| MuxError::Command("tmux returned non-UTF-8 probe state".to_owned()))?
        .trim_end_matches(['\r', '\n']);
    if text.contains(FIELD_SEPARATOR) {
        Ok(text.split(FIELD_SEPARATOR).collect())
    } else {
        Ok(text.split(ESCAPED_FIELD_SEPARATOR).collect())
    }
}

fn parse_u32(value: &str, field: &'static str) -> Result<u32> {
    value.parse().map_err(|_| MuxError::InvalidValue {
        field,
        message: format!("tmux returned {value:?}"),
    })
}

fn parse_u16(value: &str, field: &'static str) -> Result<u16> {
    value.parse().map_err(|_| MuxError::InvalidValue {
        field,
        message: format!("tmux returned {value:?}"),
    })
}

fn parse_flag(value: &str, field: &'static str) -> Result<bool> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(MuxError::InvalidValue {
            field,
            message: format!("tmux returned {value:?}"),
        }),
    }
}

fn shell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn os_strings<const N: usize>(values: [&str; N]) -> Vec<OsString> {
    values.into_iter().map(OsString::from).collect()
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        ffi::OsString,
        path::PathBuf,
        time::Duration,
    };

    use crate::{
        Result,
        domain::{
            ClientId, CodexExecutable, CommandOutput, InvocationContext, PaneId, PaneProcess,
            ProcessMatchIdentity, SessionId, TmuxCommandRunner,
        },
    };

    use super::{
        DirectCodexInspector, PaneState, ProbeSleeper, SmartLeftMatcher, SmartLeftOutcome,
        SmartLeftProbe, shell_literal, split_fields,
    };

    #[derive(Default)]
    struct Runner {
        outputs: RefCell<VecDeque<CommandOutput>>,
        calls: RefCell<Vec<Vec<OsString>>>,
    }

    impl Runner {
        fn with(outputs: impl IntoIterator<Item = CommandOutput>) -> Self {
            Self {
                outputs: RefCell::new(outputs.into_iter().collect()),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl TmuxCommandRunner for Runner {
        fn run(&self, arguments: &[OsString]) -> Result<CommandOutput> {
            self.calls.borrow_mut().push(arguments.to_vec());
            Ok(self.outputs.borrow_mut().pop_front().unwrap())
        }
    }

    struct Inspector {
        codex: bool,
        shell: bool,
    }

    impl DirectCodexInspector for Inspector {
        fn is_direct_codex(&self, _pane: &PaneProcess) -> Result<bool> {
            Ok(self.codex)
        }

        fn is_direct_shell(&self, _pid: u32, _command: &str) -> Result<bool> {
            Ok(self.shell)
        }
    }

    #[derive(Default)]
    struct Sleeper {
        calls: Cell<usize>,
        elapsed: Cell<Duration>,
    }

    impl ProbeSleeper for Sleeper {
        fn sleep(&self, duration: Duration) {
            self.calls.set(self.calls.get() + 1);
            self.elapsed.set(self.elapsed.get() + duration);
        }
    }

    fn output(stdout: impl Into<Vec<u8>>) -> CommandOutput {
        CommandOutput {
            stdout: stdout.into(),
            stderr: Vec::new(),
            status: Some(0),
        }
    }

    fn state(x: u16, y: u16, cursor: bool, mode: bool) -> CommandOutput {
        output(
            format!(
                "42\x1f{x}\x1f{y}\x1f{}\x1f{}\x1fcodex\x1f0\n",
                u8::from(cursor),
                u8::from(mode)
            )
            .into_bytes(),
        )
    }

    fn state_with_geometry(x: u16, y: u16, width: u16, height: u16) -> CommandOutput {
        output(
            format!(
                "42\x1f/dev/pts/7\x1f{x}\x1f{y}\x1f1\x1f0\x1fcodex\x1f0\x1f{width}\x1f{height}\x1f$2\x1f@3\n"
            )
            .into_bytes(),
        )
    }

    struct SequenceInspector {
        identities: RefCell<VecDeque<Option<ProcessMatchIdentity>>>,
    }

    impl DirectCodexInspector for SequenceInspector {
        fn is_direct_codex(&self, _pane: &PaneProcess) -> Result<bool> {
            Ok(true)
        }

        fn direct_codex_identity(
            &self,
            _pane: &PaneProcess,
        ) -> Result<Option<ProcessMatchIdentity>> {
            Ok(self.identities.borrow_mut().pop_front().unwrap())
        }

        fn is_direct_shell(&self, _pid: u32, _command: &str) -> Result<bool> {
            Ok(false)
        }
    }

    fn shell_state(x: u16, y: u16, prompt: bool) -> CommandOutput {
        output(
            format!(
                "42\x1f{x}\x1f{y}\x1f1\x1f0\x1fbash\x1f{}\n",
                u8::from(prompt)
            )
            .into_bytes(),
        )
    }

    fn context() -> InvocationContext {
        InvocationContext {
            client_id: ClientId::new("/dev/pts/7").unwrap(),
            pane_id: PaneId::new("%4").unwrap(),
            session_id: SessionId::new("$2").unwrap(),
            window_id: crate::domain::WindowId::new("@3").unwrap(),
            current_path: PathBuf::from("/work/project's"),
        }
    }

    fn probe<'a>(
        runner: &'a Runner,
        inspector: &'a Inspector,
        sleeper: &'a Sleeper,
        codex: &'a CodexExecutable,
    ) -> SmartLeftProbe<'a, Runner, Inspector, Sleeper> {
        SmartLeftProbe::new(
            runner,
            inspector,
            sleeper,
            std::path::Path::new("/opt/codex mux/codex-mux"),
            codex,
        )
    }

    #[test]
    fn parses_raw_and_tmux_34_escaped_state() {
        let raw = b"42\x1f2\x1f10\x1f1\x1f0\x1fcodex\x1f0\n";
        let escaped = b"42\\0372\\03710\\0371\\0370\\037codex\\0370\n";
        assert_eq!(
            PaneState::parse(raw).unwrap(),
            PaneState::parse(escaped).unwrap()
        );
    }

    #[test]
    fn malformed_state_fails_closed() {
        assert!(PaneState::parse(b"42\x1f2\n").is_err());
        assert!(split_fields(b"not utf8 \xff").is_err());
    }

    #[test]
    fn popup_shell_arguments_escape_single_quotes() {
        assert_eq!(shell_literal("project's"), "'project'\\''s'");
    }

    #[test]
    fn cursor_outside_exact_boundary_forwards_without_waiting() {
        let runner = Runner::with([
            state(5, 10, true, false),
            output(b"not a composer\n"),
            state(5, 10, true, false),
            output([]),
        ]);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        let result = probe(
            &runner,
            &Inspector {
                codex: true,
                shell: false,
            },
            &sleeper,
            &codex,
        )
        .run(&context())
        .unwrap();

        assert_eq!(result, SmartLeftOutcome::Forwarded);
        assert_eq!(sleeper.calls.get(), 0);
        assert_eq!(sleeper.elapsed.get(), Duration::ZERO);
        let calls = runner.calls.borrow();
        assert_eq!(
            calls.iter().filter(|call| call[0] == "send-keys").count(),
            1
        );
        assert!(!calls.iter().any(|call| call[0] == "display-popup"));
    }

    #[test]
    fn unchanged_composer_boundary_opens_exact_client_popup() {
        let mut screen = [""; 11];
        screen[10] = "› draft";
        let outputs = vec![
            state(2, 10, true, false),
            output(format!("{}\n", screen.join("\n")).into_bytes()),
            state(2, 10, true, false),
            output([]),
            state(2, 10, true, false),
            output(b"/dev/pts/8\x1f120\x1f40\x1f$9\x1f@9\x1f%9\n/dev/pts/7\x1f62\x1f35\x1f$2\x1f@3\x1f%4\n".to_vec()),
            output([]),
        ];
        let runner = Runner::with(outputs);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        let result = probe(
            &runner,
            &Inspector {
                codex: true,
                shell: false,
            },
            &sleeper,
            &codex,
        )
        .run(&context())
        .unwrap();

        assert_eq!(result, SmartLeftOutcome::Opened);
        assert_eq!(sleeper.calls.get(), 0);
        assert_eq!(sleeper.elapsed.get(), Duration::ZERO);
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 7);
        assert_eq!(
            calls.iter().filter(|call| call[0] == "send-keys").count(),
            1
        );
        let popup = calls
            .iter()
            .find(|call| call[0] == "display-popup")
            .unwrap();
        assert!(popup.windows(2).any(|pair| pair == ["-c", "/dev/pts/7"]));
        assert!(calls.iter().any(|call| call[0] == "list-clients"));
        assert!(popup.windows(2).any(|pair| pair == ["-w", "100%"]));
        let command = popup.last().unwrap().to_string_lossy();
        assert!(command.contains("'/work/project'\\''s'"));
    }

    #[test]
    fn composer_snapshot_retries_when_a_redraw_moves_the_cursor_row() {
        let mut first_screen = [""; 12];
        first_screen[10] = "not the composer";
        let mut settled_screen = [""; 12];
        settled_screen[11] = "› draft";
        let runner = Runner::with([
            state(2, 10, true, false),
            output(format!("{}\n", first_screen.join("\n")).into_bytes()),
            state(2, 11, true, false),
            output(format!("{}\n", settled_screen.join("\n")).into_bytes()),
            state(2, 11, true, false),
            output([]),
            state(2, 11, true, false),
            output(b"/dev/pts/7\x1f120\x1f40\x1f$2\x1f@3\x1f%4\n".to_vec()),
            output([]),
        ]);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        let outcome = probe(
            &runner,
            &Inspector {
                codex: true,
                shell: false,
            },
            &sleeper,
            &codex,
        )
        .run(&context())
        .unwrap();
        assert_eq!(
            outcome,
            SmartLeftOutcome::Opened,
            "{:?}",
            runner.calls.borrow()
        );
        assert_eq!(sleeper.calls.get(), 1);
        assert_eq!(sleeper.elapsed.get(), Duration::from_millis(10));
        let calls = runner.calls.borrow();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call[0] == "capture-pane")
                .count(),
            2
        );
        assert_eq!(
            calls.iter().filter(|call| call[0] == "send-keys").count(),
            1
        );
    }

    #[test]
    fn composer_snapshot_retries_when_geometry_changes_at_the_same_cursor() {
        let mut screen = [""; 11];
        screen[10] = "› draft";
        let runner = Runner::with([
            state_with_geometry(2, 10, 80, 24),
            output(format!("{}\n", screen.join("\n")).into_bytes()),
            state_with_geometry(2, 10, 60, 18),
            output(format!("{}\n", screen.join("\n")).into_bytes()),
            state_with_geometry(2, 10, 60, 18),
            output([]),
            state_with_geometry(2, 10, 60, 18),
            output(b"/dev/pts/7\x1f120\x1f40\x1f$2\x1f@3\x1f%4\n"),
            output([]),
        ]);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        let outcome = probe(
            &runner,
            &Inspector {
                codex: true,
                shell: false,
            },
            &sleeper,
            &codex,
        )
        .run(&context())
        .unwrap();
        assert_eq!(
            outcome,
            SmartLeftOutcome::Opened,
            "{:?}",
            runner.calls.borrow()
        );
        assert_eq!(sleeper.calls.get(), 1);
    }

    #[test]
    fn moved_client_fails_through_after_the_left_gesture() {
        let mut screen = [""; 11];
        screen[10] = "› draft";
        let runner = Runner::with([
            state(2, 10, true, false),
            output(format!("{}\n", screen.join("\n")).into_bytes()),
            state(2, 10, true, false),
            output([]),
            state(2, 10, true, false),
            output(b"/dev/pts/7\x1f120\x1f40\x1f$2\x1f@8\x1f%9\n"),
        ]);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        assert_eq!(
            probe(
                &runner,
                &Inspector {
                    codex: true,
                    shell: false,
                },
                &sleeper,
                &codex,
            )
            .run(&context())
            .unwrap(),
            SmartLeftOutcome::Forwarded
        );
        let calls = runner.calls.borrow();
        assert_eq!(
            calls.iter().filter(|call| call[0] == "send-keys").count(),
            1
        );
        assert!(!calls.iter().any(|call| call[0] == "display-popup"));
    }

    #[test]
    fn popup_launch_failure_after_validation_fails_through() {
        let mut screen = [""; 11];
        screen[10] = "› draft";
        let runner = Runner::with([
            state(2, 10, true, false),
            output(format!("{}\n", screen.join("\n")).into_bytes()),
            state(2, 10, true, false),
            output([]),
            state(2, 10, true, false),
            output(b"/dev/pts/7\x1f120\x1f40\x1f$2\x1f@3\x1f%4\n"),
            CommandOutput {
                stdout: Vec::new(),
                stderr: b"popup unavailable".to_vec(),
                status: Some(1),
            },
        ]);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        assert_eq!(
            probe(
                &runner,
                &Inspector {
                    codex: true,
                    shell: false,
                },
                &sleeper,
                &codex,
            )
            .run(&context())
            .unwrap(),
            SmartLeftOutcome::Forwarded
        );
        let calls = runner.calls.borrow();
        let popup = calls
            .iter()
            .find(|call| call[0] == "display-popup")
            .unwrap();
        assert!(popup.windows(2).any(|pair| pair == ["-c", "/dev/pts/7"]));
        assert!(popup.windows(2).any(|pair| pair == ["-t", "%4"]));
    }

    #[test]
    fn replaced_matched_descendant_cannot_authorize_the_popup() {
        let mut screen = [""; 11];
        screen[10] = "› draft";
        let runner = Runner::with([
            state(2, 10, true, false),
            output(format!("{}\n", screen.join("\n")).into_bytes()),
            state(2, 10, true, false),
            output([]),
            state(2, 10, true, false),
        ]);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();
        let inspector = SequenceInspector {
            identities: RefCell::new(VecDeque::from([
                Some(ProcessMatchIdentity {
                    pid: 100,
                    start_time: 1,
                }),
                Some(ProcessMatchIdentity {
                    pid: 101,
                    start_time: 2,
                }),
            ])),
        };
        let probe = SmartLeftProbe::new(
            &runner,
            &inspector,
            &sleeper,
            std::path::Path::new("/opt/codex-mux"),
            &codex,
        );

        assert_eq!(probe.run(&context()).unwrap(), SmartLeftOutcome::Forwarded);
        assert_eq!(
            runner
                .calls
                .borrow()
                .iter()
                .filter(|call| call[0] == "send-keys")
                .count(),
            1
        );
    }

    #[test]
    fn continuously_moving_composer_snapshot_fails_through_after_the_bound() {
        let screen = output(b"not the captured composer row\n".to_vec());
        let runner = Runner::with([
            state(2, 10, true, false),
            screen.clone(),
            state(2, 11, true, false),
            screen.clone(),
            state(2, 12, true, false),
            screen,
            state(2, 13, true, false),
            output([]),
        ]);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        assert_eq!(
            probe(
                &runner,
                &Inspector {
                    codex: true,
                    shell: false,
                },
                &sleeper,
                &codex,
            )
            .run(&context())
            .unwrap(),
            SmartLeftOutcome::Forwarded
        );
        assert_eq!(sleeper.calls.get(), 2);
        assert_eq!(sleeper.elapsed.get(), Duration::from_millis(20));
        let calls = runner.calls.borrow();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call[0] == "capture-pane")
                .count(),
            super::CODEX_SNAPSHOT_ATTEMPTS
        );
        assert_eq!(
            calls.iter().filter(|call| call[0] == "send-keys").count(),
            1
        );
        assert!(!calls.iter().any(|call| call[0] == "display-popup"));
    }

    #[test]
    fn indented_composer_boundary_used_by_reasoning_modes_opens_popup() {
        let mut screen = [""; 11];
        screen[10] = "    › draft";
        let runner = Runner::with([
            state(6, 10, true, false),
            output(format!("{}\n", screen.join("\n")).into_bytes()),
            state(6, 10, true, false),
            output([]),
            state(6, 10, true, false),
            output(b"/dev/pts/7\x1f120\x1f40\x1f$2\x1f@3\x1f%4\n"),
            output([]),
        ]);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();
        assert_eq!(
            probe(
                &runner,
                &Inspector {
                    codex: true,
                    shell: false
                },
                &sleeper,
                &codex
            )
            .run(&context())
            .unwrap(),
            SmartLeftOutcome::Opened
        );
    }

    #[test]
    fn indented_composer_prompt_glyph_boundary_opens_popup() {
        let mut screen = [""; 11];
        screen[10] = "    › draft";
        let runner = Runner::with([
            state(4, 10, true, false),
            output(format!("{}\n", screen.join("\n")).into_bytes()),
            state(4, 10, true, false),
            output([]),
            state(4, 10, true, false),
            output(b"/dev/pts/7\x1f120\x1f40\x1f$2\x1f@3\x1f%4\n"),
            output([]),
        ]);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        assert_eq!(
            probe(
                &runner,
                &Inspector {
                    codex: true,
                    shell: false
                },
                &sleeper,
                &codex
            )
            .run(&context())
            .unwrap(),
            SmartLeftOutcome::Opened
        );
    }

    #[test]
    fn indented_composer_post_glyph_boundary_used_by_ultra_and_max_opens_popup() {
        let mut screen = [""; 11];
        screen[10] = "    › draft";
        let runner = Runner::with([
            state(5, 10, true, false),
            output(format!("{}\n", screen.join("\n")).into_bytes()),
            state(5, 10, true, false),
            output([]),
            state(5, 10, true, false),
            output(b"/dev/pts/7\x1f120\x1f40\x1f$2\x1f@3\x1f%4\n"),
            output([]),
        ]);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        assert_eq!(
            probe(
                &runner,
                &Inspector {
                    codex: true,
                    shell: false
                },
                &sleeper,
                &codex
            )
            .run(&context())
            .unwrap(),
            SmartLeftOutcome::Opened
        );
    }

    #[test]
    fn indented_composer_row_at_the_wrong_cursor_column_forwards() {
        let mut screen = [""; 11];
        screen[10] = "    › draft";
        for cursor_x in [0, 3, 7] {
            let runner = Runner::with([
                state(cursor_x, 10, true, false),
                output(format!("{}\n", screen.join("\n")).into_bytes()),
                state(cursor_x, 10, true, false),
                output([]),
            ]);
            let sleeper = Sleeper::default();
            let codex = CodexExecutable::new("/opt/codex").unwrap();
            assert_eq!(
                probe(
                    &runner,
                    &Inspector {
                        codex: true,
                        shell: false
                    },
                    &sleeper,
                    &codex
                )
                .run(&context())
                .unwrap(),
                SmartLeftOutcome::Forwarded
            );
        }
    }

    #[test]
    fn unchanged_marked_shell_boundary_opens_without_composer_glyph() {
        let outputs = vec![
            shell_state(8, 4, true),
            output([]),
            shell_state(8, 4, true),
            output(b"/dev/pts/7\x1f120\x1f40\x1f$2\x1f@3\x1f%4\n".to_vec()),
            output([]),
        ];
        let runner = Runner::with(outputs);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        assert_eq!(
            probe(
                &runner,
                &Inspector {
                    codex: false,
                    shell: true,
                },
                &sleeper,
                &codex,
            )
            .run(&context())
            .unwrap(),
            SmartLeftOutcome::Opened
        );
        assert_eq!(sleeper.calls.get(), 1);
        assert_eq!(sleeper.elapsed.get(), Duration::from_millis(30));
        assert!(
            !runner
                .calls
                .borrow()
                .iter()
                .any(|call| call[0] == "capture-pane")
        );
    }

    #[test]
    fn unmarked_shell_always_forwards() {
        let runner = Runner::with([shell_state(0, 4, false), output([])]);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        assert_eq!(
            probe(
                &runner,
                &Inspector {
                    codex: false,
                    shell: true,
                },
                &sleeper,
                &codex,
            )
            .run(&context())
            .unwrap(),
            SmartLeftOutcome::Forwarded
        );
    }

    #[test]
    fn unchanged_cursor_outside_composer_fails_through() {
        let outputs = vec![
            state(2, 3, true, false),
            output(b"picker\nrow\nselected\nnot a composer\n".to_vec()),
            state(2, 3, true, false),
            output([]),
        ];
        let runner = Runner::with(outputs);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        assert_eq!(
            probe(
                &runner,
                &Inspector {
                    codex: true,
                    shell: false,
                },
                &sleeper,
                &codex,
            )
            .run(&context())
            .unwrap(),
            SmartLeftOutcome::Forwarded
        );
        assert!(
            !runner
                .calls
                .borrow()
                .iter()
                .any(|call| call[0] == "display-popup")
        );
    }

    #[test]
    fn exact_boundary_that_moves_during_immediate_recheck_fails_through() {
        let mut screen = [""; 11];
        screen[10] = "› draft";
        let runner = Runner::with([
            state(2, 10, true, false),
            output(format!("{}\n", screen.join("\n")).into_bytes()),
            state(2, 10, true, false),
            output([]),
            state(3, 10, true, false),
            state(3, 10, true, false),
        ]);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        assert_eq!(
            probe(
                &runner,
                &Inspector {
                    codex: true,
                    shell: false,
                },
                &sleeper,
                &codex,
            )
            .run(&context())
            .unwrap(),
            SmartLeftOutcome::Forwarded
        );
        assert_eq!(sleeper.calls.get(), 1);
        assert!(
            !runner
                .calls
                .borrow()
                .iter()
                .any(|call| call[0] == "display-popup")
        );
    }

    #[test]
    fn transient_ultra_redraw_settles_back_to_the_exact_boundary() {
        let mut screen = [""; 11];
        screen[10] = "› draft";
        let runner = Runner::with([
            state(2, 10, true, false),
            output(format!("{}\n", screen.join("\n")).into_bytes()),
            state(2, 10, true, false),
            output([]),
            state(54, 3, true, false),
            state(2, 10, true, false),
            output(b"/dev/pts/7\x1f120\x1f40\x1f$2\x1f@3\x1f%4\n".to_vec()),
            output([]),
        ]);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        assert_eq!(
            probe(
                &runner,
                &Inspector {
                    codex: true,
                    shell: false,
                },
                &sleeper,
                &codex,
            )
            .run(&context())
            .unwrap(),
            SmartLeftOutcome::Opened
        );
        assert_eq!(sleeper.calls.get(), 1);
        assert_eq!(sleeper.elapsed.get(), Duration::from_millis(30));
    }

    #[test]
    fn wrappers_hidden_cursor_and_copy_mode_only_forward() {
        for (direct, cursor, mode) in [
            (false, true, false),
            (true, false, false),
            (true, true, true),
        ] {
            let runner = Runner::with([state(2, 10, cursor, mode), output([])]);
            let sleeper = Sleeper::default();
            let codex = CodexExecutable::new("/opt/codex").unwrap();
            assert_eq!(
                probe(
                    &runner,
                    &Inspector {
                        codex: direct,
                        shell: false,
                    },
                    &sleeper,
                    &codex,
                )
                .run(&context())
                .unwrap(),
                SmartLeftOutcome::Forwarded
            );
            assert_eq!(runner.calls.borrow().len(), 2);
        }
    }

    #[test]
    fn configured_pane_command_prefilter_controls_wrapper_aware_probe() {
        let codex = CodexExecutable::new("/opt/codex").unwrap();
        for (pane_commands, expected) in [
            (vec!["other".to_owned()], SmartLeftOutcome::Forwarded),
            (vec!["codex".to_owned()], SmartLeftOutcome::Opened),
        ] {
            let mut screen = [""; 11];
            screen[10] = "› draft";
            let outputs = if expected == SmartLeftOutcome::Opened {
                vec![
                    state(2, 10, true, false),
                    output(format!("{}\n", screen.join("\n")).into_bytes()),
                    state(2, 10, true, false),
                    output([]),
                    state(2, 10, true, false),
                    output(b"/dev/pts/7\x1f120\x1f40\x1f$2\x1f@3\x1f%4\n".to_vec()),
                    output([]),
                ]
            } else {
                vec![state(2, 10, true, false), output([])]
            };
            let runner = Runner::with(outputs);
            let sleeper = Sleeper::default();
            let matches = vec![codex.clone()];
            let inspector = Inspector {
                codex: true,
                shell: false,
            };
            let probe = SmartLeftProbe::with_pane_commands(
                &runner,
                &inspector,
                &sleeper,
                std::path::Path::new("/opt/codex-mux"),
                &codex,
                &pane_commands,
                &matches,
            );
            assert_eq!(probe.run(&context()).unwrap(), expected);
        }
    }

    #[test]
    fn pane_command_regex_prefilter_accepts_versioned_supervisor() {
        let codex = CodexExecutable::new("/opt/codex").unwrap();
        let mut screen = [""; 11];
        screen[10] = "› draft";
        let runner = Runner::with([
            output(b"42\x1f2\x1f10\x1f1\x1f0\x1fsupervisor-v17\x1f0\n".to_vec()),
            output(format!("{}\n", screen.join("\n")).into_bytes()),
            output(b"42\x1f2\x1f10\x1f1\x1f0\x1fsupervisor-v17\x1f0\n".to_vec()),
            output([]),
            output(b"42\x1f2\x1f10\x1f1\x1f0\x1fsupervisor-v17\x1f0\n".to_vec()),
            output(b"/dev/pts/7\x1f120\x1f40\x1f$2\x1f@3\x1f%4\n".to_vec()),
            output([]),
        ]);
        let sleeper = Sleeper::default();
        let matches = vec![codex.clone()];
        let pane_regexes = vec![r"^supervisor-v[0-9]+$".to_owned()];
        let inspector = Inspector {
            codex: true,
            shell: false,
        };
        let probe = SmartLeftProbe::with_process_matcher(
            &runner,
            &inspector,
            &sleeper,
            std::path::Path::new("/opt/codex-mux"),
            &codex,
            SmartLeftMatcher {
                pane_commands: &[],
                match_executables: &matches,
                pane_command_regexes: &pane_regexes,
                match_scope: "pane-tree",
                match_command_regexes: &[],
            },
        );

        assert_eq!(probe.run(&context()).unwrap(), SmartLeftOutcome::Opened);
    }

    #[test]
    fn malformed_initial_state_still_forwards_exactly_once() {
        let runner = Runner::with([output(b"bad state\n".to_vec()), output([])]);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        assert_eq!(
            probe(
                &runner,
                &Inspector {
                    codex: true,
                    shell: false,
                },
                &sleeper,
                &codex,
            )
            .run(&context())
            .unwrap(),
            SmartLeftOutcome::Forwarded
        );
        assert_eq!(
            runner
                .calls
                .borrow()
                .iter()
                .filter(|call| call[0] == "send-keys")
                .count(),
            1
        );
    }
}
