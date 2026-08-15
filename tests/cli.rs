use std::process::Command;

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_codex-mux"))
}

#[test]
fn root_help_exposes_interactive_context_and_tmux_group() {
    let output = binary().arg("--help").output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Switch between Codex sessions running in tmux"));
    assert!(stdout.contains("--codex <PATH>"));
    assert!(stdout.contains("--client <CLIENT>"));
    assert!(stdout.contains("tmux"));
}

#[test]
fn tmux_help_exposes_install_status_and_uninstall() {
    let output = binary().args(["tmux", "--help"]).output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    for command in ["install", "status", "uninstall"] {
        assert!(stdout.contains(command), "missing {command} in {stdout}");
    }
}

#[test]
fn interactive_runtime_requires_explicit_tmux_context() {
    let output = binary().output().unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("invoking path"));
    assert!(stderr.contains("required when opening the interactive popup"));
}
