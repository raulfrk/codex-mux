//! Fail-through Smart Left probe for the Codex composer.

use std::{ffi::OsString, path::Path, thread, time::Duration};

use crate::{
    MuxError, Result,
    domain::{CodexExecutable, CommandOutput, InvocationContext, TmuxCommandRunner},
    linux_process::LinuxProcessInspector,
};

const FIELD_SEPARATOR: char = '\u{1f}';
const ESCAPED_FIELD_SEPARATOR: &str = "\\037";
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const POLL_ATTEMPTS: usize = 12;

/// Observable result of one Smart Left gesture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmartLeftOutcome {
    /// Left was delivered and no popup was opened.
    Forwarded,
    /// Left was delivered at the composer boundary and the mux popup was opened.
    Opened,
}

/// Exact-process identity required before prefixless interception is attempted.
pub trait DirectCodexInspector {
    /// Returns true only when `pid` itself is the configured Codex executable.
    fn is_direct_codex(&self, pid: u32) -> Result<bool>;
}

impl DirectCodexInspector for LinuxProcessInspector {
    fn is_direct_codex(&self, pid: u32) -> Result<bool> {
        self.process_is_exact(pid)
    }
}

/// Injectable delay boundary for deterministic probe tests.
pub trait ProbeSleeper {
    /// Waits between rendered-cursor observations.
    fn sleep(&self, duration: Duration);
}

/// Host thread sleeper used by the runtime.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemSleeper;

impl ProbeSleeper for SystemSleeper {
    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

/// Runs the bounded Smart Left decision against one exact tmux pane and client.
pub struct SmartLeftProbe<'a, Runner, Inspector, Sleeper> {
    runner: &'a Runner,
    inspector: &'a Inspector,
    sleeper: &'a Sleeper,
    mux: &'a Path,
    codex: &'a CodexExecutable,
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
        }
    }

    /// Forwards Left exactly once and opens only after an unchanged guarded probe.
    pub fn run(&self, context: &InvocationContext) -> Result<SmartLeftOutcome> {
        let initial = match self.read_state(context) {
            Ok(state)
                if state.cursor_visible
                    && !state.pane_in_mode
                    && self
                        .inspector
                        .is_direct_codex(state.pane_pid)
                        .unwrap_or(false) =>
            {
                Some(state)
            }
            Ok(_) | Err(_) => None,
        };

        self.send_left(context)?;
        let Some(initial) = initial else {
            return Ok(SmartLeftOutcome::Forwarded);
        };

        let mut current = initial;
        for _ in 0..POLL_ATTEMPTS {
            self.sleeper.sleep(POLL_INTERVAL);
            let Ok(observed) = self.read_state(context) else {
                return Ok(SmartLeftOutcome::Forwarded);
            };
            if observed.pane_pid != initial.pane_pid
                || observed.cursor_x != initial.cursor_x
                || observed.cursor_y != initial.cursor_y
                || !observed.cursor_visible
                || observed.pane_in_mode
            {
                return Ok(SmartLeftOutcome::Forwarded);
            }
            current = observed;
        }

        if current.cursor_x != 2 || !self.cursor_is_on_composer_prompt(context, current.cursor_y) {
            return Ok(SmartLeftOutcome::Forwarded);
        }

        self.open_popup(context)?;
        Ok(SmartLeftOutcome::Opened)
    }

    fn read_state(&self, context: &InvocationContext) -> Result<PaneState> {
        let format = format!(
            "#{{pane_pid}}{FIELD_SEPARATOR}#{{cursor_x}}{FIELD_SEPARATOR}#{{cursor_y}}{FIELD_SEPARATOR}#{{cursor_flag}}{FIELD_SEPARATOR}#{{pane_in_mode}}"
        );
        let output = self.run_checked(vec![
            OsString::from("display-message"),
            OsString::from("-p"),
            OsString::from("-t"),
            OsString::from(context.pane_id.as_str()),
            OsString::from(format),
        ])?;
        PaneState::parse(&output.stdout)
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

    fn cursor_is_on_composer_prompt(&self, context: &InvocationContext, cursor_y: u16) -> bool {
        let output = self.run_checked(os_strings([
            "capture-pane",
            "-p",
            "-t",
            context.pane_id.as_str(),
        ]));
        let Ok(output) = output else {
            return false;
        };
        let Ok(screen) = std::str::from_utf8(&output.stdout) else {
            return false;
        };
        screen
            .lines()
            .nth(usize::from(cursor_y))
            .is_some_and(|line| line == "›" || line.starts_with("› "))
    }

    fn open_popup(&self, context: &InvocationContext) -> Result<()> {
        let dimensions = self.run_checked(vec![
            OsString::from("list-clients"),
            OsString::from("-F"),
            OsString::from(format!(
                "#{{client_tty}}{FIELD_SEPARATOR}#{{client_width}}{FIELD_SEPARATOR}#{{client_height}}"
            )),
        ])?;
        let (width, height) = dimensions
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .find_map(|line| {
                let fields = split_fields(line).ok()?;
                (fields.len() == 3 && fields[0] == context.client_id.as_str()).then(|| {
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
        let command = [
            shell_literal(self.mux.as_os_str().to_string_lossy().as_ref()),
            "--codex".to_owned(),
            shell_literal(self.codex.as_path().as_os_str().to_string_lossy().as_ref()),
            "--client".to_owned(),
            shell_literal(context.client_id.as_str()),
            "--invoking-pane".to_owned(),
            shell_literal(context.pane_id.as_str()),
            "--invoking-session".to_owned(),
            shell_literal(context.session_id.as_str()),
            "--invoking-path".to_owned(),
            shell_literal(context.current_path.as_os_str().to_string_lossy().as_ref()),
        ]
        .join(" ");
        self.run_checked(vec![
            OsString::from("display-popup"),
            OsString::from("-E"),
            OsString::from("-c"),
            OsString::from(context.client_id.as_str()),
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PaneState {
    pane_pid: u32,
    cursor_x: u16,
    cursor_y: u16,
    cursor_visible: bool,
    pane_in_mode: bool,
}

impl PaneState {
    fn parse(output: &[u8]) -> Result<Self> {
        let fields = split_fields(output)?;
        if fields.len() != 5 {
            return Err(MuxError::Command(
                "tmux returned malformed Smart Left pane state".to_owned(),
            ));
        }
        Ok(Self {
            pane_pid: parse_u32(fields[0], "pane PID")?,
            cursor_x: parse_u16(fields[1], "cursor x")?,
            cursor_y: parse_u16(fields[2], "cursor y")?,
            cursor_visible: parse_flag(fields[3], "cursor flag")?,
            pane_in_mode: parse_flag(fields[4], "pane mode")?,
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
            ClientId, CodexExecutable, CommandOutput, InvocationContext, PaneId, SessionId,
            TmuxCommandRunner,
        },
    };

    use super::{
        DirectCodexInspector, PaneState, ProbeSleeper, SmartLeftOutcome, SmartLeftProbe,
        shell_literal, split_fields,
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

    struct Inspector(bool);

    impl DirectCodexInspector for Inspector {
        fn is_direct_codex(&self, _pid: u32) -> Result<bool> {
            Ok(self.0)
        }
    }

    #[derive(Default)]
    struct Sleeper(Cell<usize>);

    impl ProbeSleeper for Sleeper {
        fn sleep(&self, _duration: Duration) {
            self.0.set(self.0.get() + 1);
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
                "42\x1f{x}\x1f{y}\x1f{}\x1f{}\n",
                u8::from(cursor),
                u8::from(mode)
            )
            .into_bytes(),
        )
    }

    fn context() -> InvocationContext {
        InvocationContext {
            client_id: ClientId::new("/dev/pts/7").unwrap(),
            pane_id: PaneId::new("%4").unwrap(),
            session_id: SessionId::new("$2").unwrap(),
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
        let raw = b"42\x1f2\x1f10\x1f1\x1f0\n";
        let escaped = b"42\\0372\\03710\\0371\\0370\n";
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
    fn moved_cursor_forwards_once_without_waiting_full_window() {
        let runner = Runner::with([
            state(5, 10, true, false),
            output([]),
            state(4, 10, true, false),
        ]);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        let result = probe(&runner, &Inspector(true), &sleeper, &codex)
            .run(&context())
            .unwrap();

        assert_eq!(result, SmartLeftOutcome::Forwarded);
        assert_eq!(sleeper.0.get(), 1);
        let calls = runner.calls.borrow();
        assert_eq!(
            calls.iter().filter(|call| call[0] == "send-keys").count(),
            1
        );
        assert!(!calls.iter().any(|call| call[0] == "display-popup"));
    }

    #[test]
    fn unchanged_composer_boundary_opens_exact_client_popup() {
        let mut outputs = vec![state(2, 10, true, false), output([])];
        outputs.extend((0..12).map(|_| state(2, 10, true, false)));
        let mut screen = [""; 11];
        screen[10] = "› draft";
        outputs.push(output(format!("{}\n", screen.join("\n")).into_bytes()));
        outputs.push(output(
            b"/dev/pts/8\x1f120\x1f40\n/dev/pts/7\x1f62\x1f35\n".to_vec(),
        ));
        outputs.push(output([]));
        let runner = Runner::with(outputs);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        let result = probe(&runner, &Inspector(true), &sleeper, &codex)
            .run(&context())
            .unwrap();

        assert_eq!(result, SmartLeftOutcome::Opened);
        assert_eq!(sleeper.0.get(), 12);
        let calls = runner.calls.borrow();
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
    fn unchanged_cursor_outside_composer_fails_through() {
        let mut outputs = vec![state(2, 3, true, false), output([])];
        outputs.extend((0..12).map(|_| state(2, 3, true, false)));
        outputs.push(output(b"picker\nrow\nselected\nnot a composer\n".to_vec()));
        let runner = Runner::with(outputs);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        assert_eq!(
            probe(&runner, &Inspector(true), &sleeper, &codex)
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
                probe(&runner, &Inspector(direct), &sleeper, &codex)
                    .run(&context())
                    .unwrap(),
                SmartLeftOutcome::Forwarded
            );
            assert_eq!(runner.calls.borrow().len(), 2);
        }
    }

    #[test]
    fn malformed_initial_state_still_forwards_exactly_once() {
        let runner = Runner::with([output(b"bad state\n".to_vec()), output([])]);
        let sleeper = Sleeper::default();
        let codex = CodexExecutable::new("/opt/codex").unwrap();

        assert_eq!(
            probe(&runner, &Inspector(true), &sleeper, &codex)
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
