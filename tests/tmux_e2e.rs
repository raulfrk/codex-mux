mod support;

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

use support::{PtyProcess, Scratch, TmuxServer, assert_success, tools_available};

#[test]
fn installer_cli_loads_a_real_prefix_binding_with_responsive_geometry() {
    if !tools_available() {
        eprintln!("tmux or util-linux script is unavailable; skipping tmux E2E test");
        return;
    }

    let scratch = Scratch::new("installer");
    let config = scratch.join("tmux.conf");
    fs::write(&config, "set -g status off\nset -g prefix C-b\n").unwrap();
    let codex = fake_codex(&scratch);
    let server = TmuxServer::start(&config, "origin", scratch.path());
    let tmux_environment = server.tmux_environment();

    let installed = binary()
        .current_dir(scratch.path())
        .env_remove("HOME")
        .env("TMUX", &tmux_environment)
        .args([
            "--codex",
            path(&codex),
            "tmux",
            "install",
            "--key",
            "a",
            "--config",
            "tmux.conf",
        ])
        .output()
        .unwrap();
    assert_success(&installed, "install binding through packaged binary");
    let stdout = String::from_utf8(installed.stdout).unwrap();
    assert!(stdout.contains("installed codex-mux binding"));
    assert!(stdout.contains("reloaded running tmux server"));
    assert!(
        config.with_extension("codex-mux.bak").is_file(),
        "relative explicit install did not create its first backup"
    );

    let status = binary()
        .current_dir(scratch.path())
        .env_remove("HOME")
        .env("TMUX", &tmux_environment)
        .args([
            "--codex",
            path(&codex),
            "tmux",
            "status",
            "--config",
            "tmux.conf",
        ])
        .output()
        .unwrap();
    assert_success(&status, "inspect installed binding through packaged binary");
    let status = String::from_utf8(status.stdout).unwrap();
    assert!(status.contains("installed:"));
    assert!(status.contains("key: a"));
    assert!(status.contains("drift: none"));

    let binding = server.checked(&["list-keys", "-T", "prefix", "a"]);
    for literal in [
        "client_width",
        "client_height",
        "run-shell -C",
        "display-popup -E",
        "100%",
        "80%",
        "70%",
        "--client",
        "--invoking-pane",
        "--invoking-session",
        "--invoking-path",
    ] {
        assert!(
            binding.contains(literal),
            "binding omitted {literal:?}: {binding}"
        );
    }

    let uninstalled = binary()
        .current_dir(scratch.path())
        .env_remove("HOME")
        .env("TMUX", &tmux_environment)
        .args(["tmux", "uninstall", "--config", "tmux.conf"])
        .output()
        .unwrap();
    assert_success(&uninstalled, "uninstall binding through packaged binary");
    assert!(
        !fs::read_to_string(&config)
            .unwrap()
            .contains("codex-mux >>>")
    );
    let missing = server.run(&["list-keys", "-T", "prefix", "a"]);
    assert!(
        !missing.status.success(),
        "uninstall left the owned key binding active"
    );
}

#[test]
fn installer_refuses_to_write_when_tmux_config_inspection_fails() {
    let scratch = Scratch::new("inspection-error");
    let config = scratch.join("tmux.conf");
    let original = b"set -g status off\n";
    fs::write(&config, original).unwrap();
    let codex = fake_codex(&scratch);
    let tmux = scratch.join("tmux");
    fs::write(
        &tmux,
        "#!/bin/sh\nprintf 'inspection exploded\\n' >&2\nexit 42\n",
    )
    .unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o755)).unwrap();

    let output = binary()
        .current_dir(scratch.path())
        .env_remove("HOME")
        .env("PATH", scratch.path())
        .args([
            "--codex",
            path(&codex),
            "tmux",
            "install",
            "--config",
            "tmux.conf",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("could not inspect tmux configuration files: inspection exploded")
    );
    assert_eq!(fs::read(&config).unwrap(), original);
    assert!(!config.with_extension("codex-mux.bak").exists());
}

#[test]
fn interactive_cli_selects_full_screen_for_only_the_named_client() {
    if !tools_available() {
        eprintln!("tmux or util-linux script is unavailable; skipping tmux E2E test");
        return;
    }

    let fixture = RuntimeFixture::new("switch");
    let agent_dir = fixture.scratch.join("agent-project");
    fs::create_dir(&agent_dir).unwrap();
    let agent_pane = fixture.new_agent("remote", &agent_dir, "existing");
    fixture
        .server
        .checked(&["select-pane", "-t", &agent_pane, "-T", "thread-from-title"]);
    let sibling = fixture
        .server
        .checked(&[
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            &agent_pane,
        ])
        .trim()
        .to_owned();
    let observer_pane = fixture.new_shell("observer", fixture.scratch.path());

    let mut invoking_client = PtyProcess::attach(&fixture.server, "origin", 110, 36);
    let invoking_tty = wait_for_client(&fixture.server, "origin");
    let mut observer_client = PtyProcess::attach(&fixture.server, "observer", 90, 30);
    let observer_tty = wait_for_client(&fixture.server, "observer");
    assert_eq!(
        client_pane(&fixture.server, &invoking_tty),
        fixture.origin_pane
    );
    assert_eq!(client_pane(&fixture.server, &observer_tty), observer_pane);
    assert_eq!(client_session(&fixture.server, &invoking_tty), "origin");
    assert_eq!(client_session(&fixture.server, &observer_tty), "observer");

    let capture = fixture.scratch.join("switch-screen.log");
    let mut popup = fixture.interactive_captured(&invoking_tty, &capture);
    support::wait_for_file_text(&capture, "thread-from-title");
    popup.send(b"\r");
    popup.wait_for_exit();

    let selected = client_pane(&fixture.server, &invoking_tty);
    assert_eq!(selected, agent_pane);
    assert_eq!(
        fixture
            .server
            .checked(&[
                "display-message",
                "-p",
                "-t",
                &selected,
                "#{pane_current_command}"
            ])
            .trim(),
        "codex-e2e"
    );
    assert_eq!(client_pane(&fixture.server, &observer_tty), observer_pane);
    assert_eq!(
        fixture
            .server
            .checked(&["display-message", "-p", "-t", &selected, "#{window_panes}"])
            .trim(),
        "2"
    );
    assert_eq!(
        fixture
            .server
            .checked(&[
                "display-message",
                "-p",
                "-t",
                &selected,
                "#{window_zoomed_flag}"
            ])
            .trim(),
        "1"
    );
    assert!(pane_exists(&fixture.server, &sibling));

    invoking_client.send(b"\x02d");
    observer_client.send(b"\x02d");
}

#[test]
fn interactive_cli_launches_exact_new_and_resume_arguments_in_selected_cwd() {
    if !tools_available() {
        eprintln!("tmux or util-linux script is unavailable; skipping tmux E2E test");
        return;
    }

    for (key, expected_tail) in [(b'n', ""), (b'r', "arg2=resume\narg3=--all\n")] {
        let fixture = RuntimeFixture::new(if key == b'n' { "new" } else { "resume" });
        let selected_dir = fixture.scratch.join("selected-cwd");
        fs::create_dir(&selected_dir).unwrap();
        fixture.new_agent("remote", &selected_dir, "existing");
        fs::write(&fixture.log, "").unwrap();
        let mut client = PtyProcess::attach(&fixture.server, "origin", 120, 40);
        let client_tty = wait_for_client(&fixture.server, "origin");
        let mut shared_client = PtyProcess::attach(&fixture.server, "origin", 100, 32);
        let shared_tty = wait_for_other_client(&fixture.server, "origin", &client_tty);
        assert_eq!(
            client_pane(&fixture.server, &shared_tty),
            fixture.origin_pane
        );

        let mut popup = fixture.interactive(&client_tty);
        thread::sleep(Duration::from_millis(200));
        popup.send(&[key]);
        popup.wait_for_exit();
        let log = support::wait_for_file(&fixture.log);
        assert!(
            log.contains(&format!("cwd={}\n", selected_dir.display())),
            "{log}"
        );
        assert!(
            log.contains("arg0=-c\narg1=tui.terminal_title=[\"thread-id\"]\n"),
            "launch did not preserve direct config arguments: {log}"
        );
        assert!(
            log.contains(expected_tail),
            "launch omitted expected direct arguments {expected_tail:?}: {log}"
        );
        if key == b'n' {
            assert!(
                !log.contains("resume"),
                "new action unexpectedly resumed: {log}"
            );
        }
        assert_eq!(
            client_pane(&fixture.server, &shared_tty),
            client_pane(&fixture.server, &client_tty),
            "clients attached to one tmux session must reflect its shared active window"
        );
        client.send(b"\x02d");
        shared_client.send(b"\x02d");
    }
}

#[test]
fn interactive_close_requires_confirmation_and_targets_only_the_selected_pane() {
    if !tools_available() {
        eprintln!("tmux or util-linux script is unavailable; skipping tmux E2E test");
        return;
    }

    let fixture = RuntimeFixture::new("close");
    let agent = fixture.new_agent("remote", fixture.scratch.path(), "existing");
    let unrelated = fixture.new_shell("unrelated", fixture.scratch.path());
    let mut client = PtyProcess::attach(&fixture.server, "origin", 120, 40);
    let client_tty = wait_for_client(&fixture.server, "origin");

    let mut popup = fixture.interactive(&client_tty);
    thread::sleep(Duration::from_millis(700));
    popup.send(b"xq");
    thread::sleep(Duration::from_millis(100));
    assert!(
        pane_exists(&fixture.server, &agent),
        "one x killed the selected pane"
    );

    popup.send(b"q");
    popup.wait_for_exit();
    assert!(
        pane_exists(&fixture.server, &unrelated),
        "close cancellation damaged an unrelated pane"
    );
    client.send(b"\x02d");

    let fixture = RuntimeFixture::new("close-confirmed");
    let agent = fixture.new_agent("remote", fixture.scratch.path(), "existing");
    let unrelated = fixture.new_shell("unrelated", fixture.scratch.path());
    let mut client = PtyProcess::attach(&fixture.server, "origin", 120, 40);
    let client_tty = wait_for_client(&fixture.server, "origin");
    let mut popup = fixture.interactive(&client_tty);
    thread::sleep(Duration::from_millis(700));
    popup.send(b"x\rq");
    fixture.server.wait_until("confirmed pane close", || {
        !pane_exists(&fixture.server, &agent)
    });
    popup.wait_for_exit();

    assert!(
        pane_exists(&fixture.server, &unrelated),
        "close damaged an unrelated pane"
    );
    client.send(b"\x02d");
}

#[test]
fn interactive_errors_are_explicit_and_leave_tmux_untouched() {
    if !tools_available() {
        eprintln!("tmux or util-linux script is unavailable; skipping tmux E2E test");
        return;
    }

    let fixture = RuntimeFixture::new("error");
    let unrelated = fixture.new_shell("unrelated", fixture.scratch.path());
    let before = fixture
        .server
        .checked(&["list-panes", "-a", "-F", "#{pane_id}"]);
    let output = binary()
        .env("TMUX", fixture.server.tmux_environment())
        .args(["--codex", path(&fixture.codex)])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("is required when opening the interactive popup from tmux"),
        "unexpected error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        before,
        fixture
            .server
            .checked(&["list-panes", "-a", "-F", "#{pane_id}"])
    );
    assert!(pane_exists(&fixture.server, &unrelated));
}

struct RuntimeFixture {
    scratch: Scratch,
    codex: PathBuf,
    log: PathBuf,
    server: TmuxServer,
    origin_pane: String,
    origin_session: String,
}

impl RuntimeFixture {
    fn new(label: &str) -> Self {
        let scratch = Scratch::new(label);
        let config = scratch.join("tmux.conf");
        fs::write(&config, "set -g status off\nset -g prefix C-b\n").unwrap();
        let codex = fake_codex(&scratch);
        let log = scratch.join("codex.log");
        let server = TmuxServer::start(&config, "origin", scratch.path());
        server.checked(&["set-environment", "-g", "CODEX_MUX_E2E_LOG", path(&log)]);
        let origin_pane = server
            .checked(&["display-message", "-p", "-t", "origin", "#{pane_id}"])
            .trim()
            .to_owned();
        let origin_session = server
            .checked(&["display-message", "-p", "-t", "origin", "#{session_id}"])
            .trim()
            .to_owned();
        Self {
            scratch,
            codex,
            log,
            server,
            origin_pane,
            origin_session,
        }
    }

    fn new_agent(&self, session: &str, cwd: &Path, marker: &str) -> String {
        if !self
            .server
            .run(&["has-session", "-t", session])
            .status
            .success()
        {
            let output = self
                .server
                .command()
                .args(["new-session", "-d", "-s", session, "-c"])
                .arg(cwd)
                .arg("--")
                .arg(&self.codex)
                .arg(marker)
                .output()
                .unwrap();
            assert_success(&output, "create cross-session Codex pane");
        }
        let pane = self
            .server
            .checked(&["display-message", "-p", "-t", session, "#{pane_id}"])
            .trim()
            .to_owned();
        self.server.wait_until("fake Codex foreground process", || {
            self.server
                .checked(&[
                    "display-message",
                    "-p",
                    "-t",
                    &pane,
                    "#{pane_current_command}",
                ])
                .trim()
                != "sh"
        });
        pane
    }

    fn new_shell(&self, session: &str, cwd: &Path) -> String {
        let output = self
            .server
            .command()
            .args(["new-session", "-d", "-s", session, "-c"])
            .arg(cwd)
            .output()
            .unwrap();
        assert_success(&output, "create unrelated tmux session");
        self.server
            .checked(&["display-message", "-p", "-t", session, "#{pane_id}"])
            .trim()
            .to_owned()
    }

    fn interactive(&self, client_tty: &str) -> PtyProcess {
        let tmux = self.server.tmux_environment();
        PtyProcess::run_binary(
            &self.interactive_arguments(client_tty),
            &[
                ("TMUX", &tmux),
                ("XDG_CONFIG_HOME", path(self.scratch.path())),
            ],
        )
    }

    fn interactive_captured(&self, client_tty: &str, output: &Path) -> PtyProcess {
        let tmux = self.server.tmux_environment();
        PtyProcess::run_binary_captured(
            &self.interactive_arguments(client_tty),
            &[
                ("TMUX", &tmux),
                ("XDG_CONFIG_HOME", path(self.scratch.path())),
            ],
            output,
        )
    }

    fn interactive_arguments(&self, client_tty: &str) -> Vec<String> {
        vec![
            "--codex".to_owned(),
            path(&self.codex).to_owned(),
            "--client".to_owned(),
            client_tty.to_owned(),
            "--invoking-pane".to_owned(),
            self.origin_pane.clone(),
            "--invoking-session".to_owned(),
            self.origin_session.clone(),
            "--invoking-path".to_owned(),
            path(self.scratch.path()).to_owned(),
        ]
    }
}

fn fake_codex(scratch: &Scratch) -> PathBuf {
    let executable = scratch.join("codex-e2e");
    let source = scratch.join("fake_codex.rs");
    fs::write(
        &source,
        r#"
use std::{env, fs::OpenOptions, io::Write, thread, time::Duration};

fn main() {
    let log = env::var_os("CODEX_MUX_E2E_LOG").expect("test log path");
    let mut output = OpenOptions::new().create(true).append(true).open(log).unwrap();
    writeln!(output, "cwd={}", env::current_dir().unwrap().display()).unwrap();
    for (index, argument) in env::args().skip(1).enumerate() {
        writeln!(output, "arg{index}={argument}").unwrap();
    }
    writeln!(output, "---").unwrap();
    thread::sleep(Duration::from_secs(300));
}
"#,
    )
    .unwrap();
    let compiled = Command::new("rustc")
        .args(["--edition=2024", "-o"])
        .arg(&executable)
        .arg(&source)
        .output()
        .expect("compile deterministic fake Codex executable");
    assert_success(&compiled, "compile deterministic fake Codex executable");
    executable
}

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_codex-mux"))
}

fn wait_for_client(server: &TmuxServer, session: &str) -> String {
    let mut result = None;
    server.wait_until(&format!("client attached to {session}"), || {
        let clients = server.checked(&["list-clients", "-F", "#{client_tty} #{client_session}"]);
        result = clients
            .lines()
            .find_map(|line| line.strip_suffix(&format!(" {session}")).map(str::to_owned));
        result.is_some()
    });
    result.unwrap()
}

fn wait_for_other_client(server: &TmuxServer, session: &str, existing: &str) -> String {
    let mut result = None;
    server.wait_until(&format!("second client attached to {session}"), || {
        let clients = server.checked(&["list-clients", "-F", "#{client_tty} #{client_session}"]);
        result = clients.lines().find_map(|line| {
            line.strip_suffix(&format!(" {session}"))
                .filter(|tty| *tty != existing)
                .map(str::to_owned)
        });
        result.is_some()
    });
    result.unwrap()
}

fn client_pane(server: &TmuxServer, client: &str) -> String {
    client_fields(server, client).1
}

fn pane_exists(server: &TmuxServer, pane: &str) -> bool {
    server
        .checked(&["list-panes", "-a", "-F", "#{pane_id}"])
        .lines()
        .any(|candidate| candidate == pane)
}

fn client_session(server: &TmuxServer, client: &str) -> String {
    client_fields(server, client).0
}

fn client_fields(server: &TmuxServer, target: &str) -> (String, String) {
    server
        .checked(&[
            "list-clients",
            "-F",
            "#{client_tty}\u{1f}#{client_session}\u{1f}#{pane_id}",
        ])
        .lines()
        .find_map(|line| {
            let mut fields = line.split('\u{1f}');
            let tty = fields.next()?;
            let session = fields.next()?;
            let pane = fields.next()?;
            (tty == target).then(|| (session.to_owned(), pane.to_owned()))
        })
        .unwrap_or_else(|| panic!("tmux client {target:?} disappeared"))
}

fn path(path: &Path) -> &str {
    path.to_str().expect("E2E paths are valid UTF-8")
}
