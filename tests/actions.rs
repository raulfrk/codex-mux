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
        let mut output = self.outputs.borrow_mut().pop_front().unwrap_or_else(ok);
        if output.stdout == b"<operation-token>\n" {
            let token = self
                .commands
                .borrow()
                .iter()
                .rev()
                .flat_map(|call| call.iter().rev())
                .find_map(|argument| {
                    let value = argument.to_string_lossy();
                    let start = value.find("op-")?;
                    Some(
                        value[start..]
                            .chars()
                            .take_while(|character| {
                                character.is_ascii_alphanumeric() || *character == '-'
                            })
                            .collect::<String>(),
                    )
                })
                .expect("guarded mutation omitted operation token");
            output.stdout = format!("{token}\n").into_bytes();
        }
        Ok(output)
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

fn operation_marker() -> CommandOutput {
    stdout("<operation-token>\n")
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
        generated_at_unix: None,
        immediate_naming: false,
        manual_name: false,

        manual_name_source: None,

        manual_name_pid: None,

        manual_name_session: None,

        pane_pid: 100,
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
            args(&[
                "select-window",
                "-t",
                "%73",
                ";",
                "select-pane",
                "-Z",
                "-t",
                "%73",
                ";",
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
        runner.commands()[1],
        args(&[
            "select-window",
            "-t",
            "%73",
            ";",
            "select-pane",
            "-Z",
            "-t",
            "%73",
            ";",
            "switch-client",
            "-Z",
            "-c",
            "/dev/pts/42; display-message hacked",
            "-t",
            "%73",
            ";",
            "resize-pane",
            "-Z",
            "-t",
            "%73",
        ])
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
            args(&[
                "select-window",
                "-t",
                "%91",
                ";",
                "select-pane",
                "-Z",
                "-t",
                "%91",
                ";",
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
    assert_eq!(
        runner.commands().last().unwrap(),
        &args(&["set-option", "-p", "-t", "%92", "@codex_mux_name_now", "1",])
    );
}

#[test]
fn yolo_resume_profile_uses_custom_executable_and_global_permission_flag() {
    let runner = RecordingRunner::with_outputs([stdout("%95\n"), ok()]);
    let configured = CodexExecutable::new("/configured/codex").unwrap();
    let custom = CodexExecutable::new("/opt/custom codex").unwrap();
    let actions = TmuxActions::new(&runner, &configured);
    let context = context("/fallback");
    let selected = pane("%73", "/selected");

    assert_eq!(
        actions
            .resume_all_with_profile(&context, Some(&selected), &custom, true)
            .unwrap(),
        PaneId::new("%95").unwrap()
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
                "/selected",
                "--",
                "/opt/custom codex",
                "-c",
                TERMINAL_TITLE_CONFIG,
                "--yolo",
                "resume",
                "--all",
            ]),
            args(&[
                "select-window",
                "-t",
                "%95",
                ";",
                "select-pane",
                "-Z",
                "-t",
                "%95",
                ";",
                "switch-client",
                "-Z",
                "-c",
                "/dev/pts/42; display-message hacked",
                "-t",
                "%95",
            ]),
            args(&["set-option", "-p", "-t", "%95", "@codex_mux_name_now", "1",]),
        ]
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
fn manual_rename_clears_generated_metadata_and_passes_title_as_literal_argument() {
    let runner = RecordingRunner::with_outputs([ok(), operation_marker(), ok()]);
    let executable = CodexExecutable::new("/opt/codex/bin/codex").unwrap();
    let actions = TmuxActions::new(&runner, &executable);
    let selected = pane("%104", "/work/project");
    let title = "Manual; $(not a shell command)";

    actions.rename_pane(&selected, title).unwrap();

    let commands = runner.commands();
    assert_eq!(commands.len(), 3);
    assert!(
        commands[0]
            .last()
            .unwrap()
            .to_string_lossy()
            .contains(title)
    );
    assert!(
        commands[0]
            .last()
            .unwrap()
            .to_string_lossy()
            .contains("@codex_mux_manual_name")
    );
    for option in [
        "@codex_mux_generated_thread",
        "@codex_mux_manual_name_source",
        "@codex_mux_unpin_ready",
    ] {
        assert!(
            commands[0]
                .last()
                .unwrap()
                .to_string_lossy()
                .contains(option)
        );
    }
    assert!(
        !commands[0]
            .last()
            .unwrap()
            .to_string_lossy()
            .contains("pane_title"),
        "a process-owned pane title must not reject an otherwise live pane"
    );
}

#[test]
fn manual_rename_escapes_tmux_format_interpolation() {
    let runner = RecordingRunner::with_outputs([ok(), operation_marker(), ok()]);
    let executable = CodexExecutable::new("/opt/codex/bin/codex").unwrap();
    let actions = TmuxActions::new(&runner, &executable);

    actions
        .rename_pane(
            &pane("%105", "/work/project"),
            "a#{pane_id}b #[fg=red] ##[bg=blue] ###[none] plain#hash #(hostname) ##",
        )
        .unwrap();

    assert!(
        runner.commands()[0]
            .last()
            .unwrap()
            .to_string_lossy()
            .contains(
                "a##{pane_id}b #[fg=red] ##[bg=blue] ###[none] plain##hash ##(hostname) ####"
            )
    );
}

#[test]
fn manual_rename_preserves_a_title_that_is_exactly_a_semicolon() {
    let runner = RecordingRunner::with_outputs([ok(), operation_marker(), ok()]);
    let executable = CodexExecutable::new("/opt/codex/bin/codex").unwrap();
    TmuxActions::new(&runner, &executable)
        .rename_pane(&pane("%110", "/work/project"), ";")
        .unwrap();
    let commands = runner.commands();
    let mutation = commands[0].last().unwrap().to_string_lossy();
    assert!(mutation.contains("'-T' ';' ; 'set-option'"));
}

#[test]
fn unpin_restores_retained_thread_and_requests_immediate_naming() {
    let runner =
        RecordingRunner::with_outputs([ok(), operation_marker(), ok(), operation_marker(), ok()]);
    let executable = CodexExecutable::new("/opt/codex/bin/codex").unwrap();
    let actions = TmuxActions::new(&runner, &executable);
    let mut selected = pane("%106", "/work/project");
    selected.manual_name = true;
    selected.manual_name_source = Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_owned());
    selected.manual_name_pid = Some(selected.pane_pid);
    selected.manual_name_session = Some(selected.session_id.clone());
    actions.unpin_pane(&selected).unwrap();
    let commands = runner.commands();
    assert_eq!(commands.len(), 5);
    assert!(
        commands[0]
            .iter()
            .any(|arg| arg.to_string_lossy().contains("select-pane"))
    );
    assert!(
        commands[2]
            .iter()
            .any(|arg| arg.to_string_lossy().contains("@codex_mux_name_now"))
    );
}

#[test]
fn legacy_manual_uuid_is_never_inferred_as_a_retained_thread() {
    let runner = RecordingRunner::with_outputs([ok(), operation_marker(), ok()]);
    let executable = CodexExecutable::new("/opt/codex/bin/codex").unwrap();
    let actions = TmuxActions::new(&runner, &executable);
    let mut selected = pane("%107", "/work/project");
    selected.manual_name = true;
    selected.title = Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_owned());
    actions.rename_pane(&selected, "Still manual").unwrap();
    assert_eq!(
        runner.commands()[0]
            .last()
            .unwrap()
            .to_string_lossy()
            .matches("@codex_mux_manual_name_source")
            .count(),
        1
    );
}

#[test]
fn guarded_rename_and_unpin_surface_stale_pane_races() {
    let executable = CodexExecutable::new("/opt/codex/bin/codex").unwrap();
    let rename_runner = RecordingRunner::with_outputs([ok(), stdout("\n")]);
    let error = TmuxActions::new(&rename_runner, &executable)
        .rename_pane(&pane("%108", "/work/project"), "Manual")
        .unwrap_err();
    assert!(error.to_string().contains("pane changed"));

    let unpin_runner =
        RecordingRunner::with_outputs([ok(), operation_marker(), ok(), stdout("1\n")]);
    let mut selected = pane("%109", "/work/project");
    selected.manual_name = true;
    selected.title = Some("Pinned".to_owned());
    selected.manual_name_source = Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_owned());
    selected.manual_name_pid = Some(selected.pane_pid);
    selected.manual_name_session = Some(selected.session_id.clone());
    let error = TmuxActions::new(&unpin_runner, &executable)
        .unpin_pane(&selected)
        .unwrap_err();
    assert!(error.to_string().contains("while its manual name"));
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
