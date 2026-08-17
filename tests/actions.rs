use std::{cell::RefCell, collections::VecDeque, ffi::OsString, path::PathBuf};

use codex_mux::{
    Result,
    domain::{
        ClientId, CodexExecutable, CommandOutput, InvocationContext, Pane, PaneId, SessionId,
        TmuxCommandRunner,
    },
    launch::TERMINAL_TITLE_CONFIG,
    tmux::actions::TmuxActions,
};

#[derive(Default)]
struct RecordingRunner {
    commands: RefCell<Vec<Vec<OsString>>>,
    outputs: RefCell<VecDeque<CommandOutput>>,
}

impl RecordingRunner {
    fn with_outputs(outputs: impl IntoIterator<Item = CommandOutput>) -> Self {
        Self {
            commands: RefCell::default(),
            outputs: RefCell::new(outputs.into_iter().collect()),
        }
    }

    fn commands(&self) -> Vec<Vec<OsString>> {
        self.commands.borrow().clone()
    }
}

impl TmuxCommandRunner for RecordingRunner {
    fn run(&self, arguments: &[OsString]) -> Result<CommandOutput> {
        self.commands.borrow_mut().push(arguments.to_vec());
        Ok(self.outputs.borrow_mut().pop_front().unwrap_or_else(ok))
    }
}

fn ok() -> CommandOutput {
    CommandOutput {
        stdout: Vec::new(),
        stderr: Vec::new(),
        status: Some(0),
    }
}

fn stdout(value: &str) -> CommandOutput {
    CommandOutput {
        stdout: value.as_bytes().to_vec(),
        ..ok()
    }
}

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn context(path: &str) -> InvocationContext {
    InvocationContext {
        client_id: ClientId::new("/dev/pts/42; display-message hacked").unwrap(),
        pane_id: PaneId::new("%1").unwrap(),
        session_id: SessionId::new("$7").unwrap(),
        current_path: PathBuf::from(path),
    }
}

fn pane(id: &str, path: &str) -> Pane {
    Pane {
        id: PaneId::new(id).unwrap(),
        session_id: SessionId::new("$9").unwrap(),
        title: Some("thread".to_owned()),
        generated_title: None,
        current_path: PathBuf::from(path),
    }
}

#[test]
fn switch_selects_exact_window_and_pane_then_targets_the_invoking_client() {
    let runner = RecordingRunner::with_outputs([stdout("1\n"), ok()]);
    let executable = CodexExecutable::new("/opt/codex/bin/codex").unwrap();
    let actions = TmuxActions::new(&runner, &executable);
    let context = context("/fallback");
    let selected = pane("%73", "/selected");

    actions.switch_and_zoom(&context, &selected).unwrap();

    assert_eq!(
        runner.commands(),
        vec![
            args(&[
                "display-message",
                "-p",
                "-t",
                "%73",
                "#{window_zoomed_flag}",
            ]),
            args(&["select-window", "-t", "%73",]),
            args(&["select-pane", "-Z", "-t", "%73",]),
            args(&[
                "switch-client",
                "-Z",
                "-c",
                "/dev/pts/42; display-message hacked",
                "-t",
                "%73",
            ]),
        ]
    );
}

#[test]
fn switch_zooms_selected_pane_only_when_needed() {
    let runner = RecordingRunner::with_outputs([stdout("0"), ok(), ok()]);
    let executable = CodexExecutable::new("/opt/codex/bin/codex").unwrap();
    let actions = TmuxActions::new(&runner, &executable);
    let context = context("/fallback");
    let selected = pane("%73", "/selected");

    actions.switch_and_zoom(&context, &selected).unwrap();

    assert_eq!(
        runner.commands()[4],
        args(&["resize-pane", "-Z", "-t", "%73"])
    );
}

#[test]
fn new_session_uses_selected_cwd_and_direct_custom_executable_arguments() {
    let runner = RecordingRunner::with_outputs([stdout("%91\n"), ok()]);
    let executable = CodexExecutable::new("/opt/Codex tools/codex'; touch /tmp/pwned").unwrap();
    let actions = TmuxActions::new(&runner, &executable);
    let context = context("/fallback path");
    let selected = pane("%73", "/work/quote' ; $(touch nope)");

    assert_eq!(
        actions.new_session(&context, Some(&selected)).unwrap(),
        PaneId::new("%91").unwrap()
    );

    assert_eq!(
        runner.commands(),
        vec![
            args(&[
                "new-window",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                "$7",
                "-c",
                "/work/quote' ; $(touch nope)",
                "--",
                "/opt/Codex tools/codex'; touch /tmp/pwned",
                "-c",
                TERMINAL_TITLE_CONFIG,
            ]),
            args(&["select-window", "-t", "%91",]),
            args(&["select-pane", "-Z", "-t", "%91",]),
            args(&[
                "switch-client",
                "-Z",
                "-c",
                "/dev/pts/42; display-message hacked",
                "-t",
                "%91",
            ]),
        ]
    );
}

#[test]
fn yolo_profile_uses_custom_executable_and_exact_direct_arguments() {
    let runner = RecordingRunner::with_outputs([stdout("%94\n"), ok()]);
    let configured = CodexExecutable::new("/configured/codex").unwrap();
    let custom = CodexExecutable::new("/opt/custom codex").unwrap();
    let actions = TmuxActions::new(&runner, &configured);
    let context = context("/fallback");

    actions
        .new_session_with_profile(&context, None, &custom, true)
        .unwrap();

    assert_eq!(
        runner.commands()[0],
        args(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "$7",
            "-c",
            "/fallback",
            "--",
            "/opt/custom codex",
            "-c",
            TERMINAL_TITLE_CONFIG,
            "--yolo",
        ])
    );
}

#[test]
fn resume_uses_invoking_cwd_fallback_and_resume_all_exactly() {
    let runner = RecordingRunner::with_outputs([stdout("%92"), ok()]);
    let executable = CodexExecutable::new("/custom/bin/codex").unwrap();
    let actions = TmuxActions::new(&runner, &executable);
    let context = context("/fallback dir/with $dollar");

    actions.resume_all(&context, None).unwrap();

    assert_eq!(
        runner.commands()[0],
        args(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "$7",
            "-c",
            "/fallback dir/with $dollar",
            "--",
            "/custom/bin/codex",
            "-c",
            TERMINAL_TITLE_CONFIG,
            "resume",
            "--all",
        ])
    );
}

#[test]
fn close_kills_only_the_explicit_selected_pane() {
    let runner = RecordingRunner::default();
    let executable = CodexExecutable::new("/opt/codex/bin/codex").unwrap();
    let actions = TmuxActions::new(&runner, &executable);

    actions.close_pane(&pane("%104", "/work/project")).unwrap();

    assert_eq!(runner.commands(), vec![args(&["kill-pane", "-t", "%104"])]);
}

#[test]
fn invalid_tmux_zoom_or_created_pane_output_fails_closed() {
    let executable = CodexExecutable::new("/opt/codex/bin/codex").unwrap();
    let selected = pane("%73", "/selected");
    let context = context("/fallback");

    let bad_zoom = RecordingRunner::with_outputs([stdout("maybe")]);
    assert!(
        TmuxActions::new(&bad_zoom, &executable)
            .switch_and_zoom(&context, &selected)
            .is_err()
    );
    assert_eq!(bad_zoom.commands().len(), 1);

    let bad_pane = RecordingRunner::with_outputs([stdout("session:window.0")]);
    assert!(
        TmuxActions::new(&bad_pane, &executable)
            .new_session(&context, Some(&selected))
            .is_err()
    );
    assert_eq!(bad_pane.commands().len(), 1);
}
