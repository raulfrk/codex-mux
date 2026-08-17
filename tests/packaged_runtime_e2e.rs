mod packaged_support;

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

use packaged_support::{Pty, Scratch, Server, assert_success, require_prerequisites, wait};

#[test]
fn packaged_binary_renders_server_wide_rows_rebuilds_and_handles_navigation_sizes() {
    let Some(binary) = require_prerequisites() else {
        return;
    };
    let fixture = Fixture::new("render");
    let alpha_dir = fixture.scratch.join("alpha-project");
    let beta_dir = fixture.scratch.join("beta-project");
    fs::create_dir_all(&alpha_dir).unwrap();
    fs::create_dir_all(&beta_dir).unwrap();
    let alpha = fixture.agent("alpha", &alpha_dir, "alpha");
    fixture.title(&alpha, "Alpha thread");
    let beta = fixture.agent("beta", &beta_dir, "beta");
    fixture.title(&beta, "Beta thread");
    let (mut client, tty) = fixture.client("origin", (120, 40), "render-client");

    let capture = fixture.scratch.join("render.log");
    let mut popup = fixture.popup(&binary, &tty, (120, 40), &capture, None);
    let rendered = popup.wait_text("beta-project");
    for visible in [
        "Alpha thread",
        "Beta",
        "thread",
        "alpha-project",
        "beta-project",
    ] {
        assert!(
            rendered.contains(visible),
            "missing row text {visible:?}: {rendered:?}"
        );
    }
    for pane in [&alpha, &beta] {
        assert!(
            !rendered.contains(pane),
            "internal pane ID leaked into UI: {rendered:?}"
        );
    }
    let before_navigation = popup.capture_len();
    popup.send(b"j");
    popup.wait_growth(before_navigation);
    fixture.server.checked(&["kill-pane", "-t", &alpha]);
    let gamma_dir = fixture.scratch.join("gamma-project");
    fs::create_dir(&gamma_dir).unwrap();
    let gamma = fixture.agent("gamma", &gamma_dir, "gamma");
    fixture.title(&gamma, "Gamma reconnected");
    popup.wait_text("gamma-project");
    popup.send(b"\r");
    assert!(popup.wait_exit().success());
    assert_eq!(client_fields(&fixture.server, &tty).1, beta);
    fixture
        .server
        .checked(&["switch-client", "-c", &tty, "-t", "origin"]);

    for (index, (width, height, expected, absent)) in [
        (120, 40, "Commands", "Enter open"),
        (89, 35, "Enter switch", "Commands"),
        (62, 35, "Enter open", "Commands"),
        (32, 10, "n r x t c q", "Commands"),
    ]
    .into_iter()
    .enumerate()
    {
        let size = (width, height);
        let capture = fixture.scratch.join(format!("size-{index}.log"));
        let before = fixture.scratch.join(format!("stty-before-{index}"));
        let after = fixture.scratch.join(format!("stty-after-{index}"));
        let mut sized = fixture.popup(&binary, &tty, size, &capture, Some((&before, &after)));
        let screen = sized.wait_text(expected);
        assert!(
            !screen.contains(absent),
            "{size:?} rendered forbidden layout marker {absent:?}: {screen:?}"
        );
        assert!(
            screen.contains("Beta"),
            "{size:?} omitted selected session title: {screen:?}"
        );
        sized.send(b"q");
        assert!(sized.wait_exit().success());
        assert_eq!(
            fs::read_to_string(before).unwrap(),
            fs::read_to_string(after).unwrap()
        );
    }
    client.send(b"\x02d");
}

#[test]
fn packaged_enter_switches_exact_client_cross_session_and_zooms() {
    let Some(binary) = require_prerequisites() else {
        return;
    };
    let fixture = Fixture::new("switch");
    let project = fixture.scratch.join("switch-project");
    fs::create_dir(&project).unwrap();
    let selected = fixture.agent("remote", &project, "selected");
    fixture.title(&selected, "Selected remote");
    let sibling = fixture
        .server
        .checked(&[
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            &selected,
        ])
        .trim()
        .to_owned();
    let observer_pane = fixture.shell("observer");
    let (mut invoking, invoking_tty) = fixture.client("origin", (120, 40), "invoking");
    let (mut observer, observer_tty) = fixture.client("observer", (100, 32), "observer");

    let capture = fixture.scratch.join("switch.log");
    let mut popup = fixture.popup(&binary, &invoking_tty, (120, 40), &capture, None);
    popup.wait_text("Selected remote");
    popup.send(b"\r");
    assert!(popup.wait_exit().success());
    assert_eq!(client_fields(&fixture.server, &invoking_tty).1, selected);
    assert_eq!(
        client_fields(&fixture.server, &observer_tty).1,
        observer_pane
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
    assert!(
        fixture
            .server
            .run(&["display-message", "-p", "-t", &sibling])
            .status
            .success()
    );
    invoking.send(b"\x02d");
    observer.send(b"\x02d");
}

#[test]
fn packaged_navigation_keys_activate_observable_rows_and_inverse_movements() {
    let Some(binary) = require_prerequisites() else {
        return;
    };
    let fixture = Fixture::new("navigation");
    let first_dir = fixture.scratch.join("first-project");
    let second_dir = fixture.scratch.join("second-project");
    fs::create_dir(&first_dir).unwrap();
    fs::create_dir(&second_dir).unwrap();
    let first = fixture.agent("first", &first_dir, "first");
    fixture.title(&first, "First row");
    let second = fixture.agent("second", &second_dir, "second");
    fixture.title(&second, "Second row");
    let (mut client, tty) = fixture.client("origin", (120, 40), "navigation-client");

    for (index, keys, expected) in [
        (0, b"j\r".as_slice(), &second),
        (1, b"jk\r".as_slice(), &first),
        (2, b"\x1b[B\r".as_slice(), &second),
        (3, b"\x1b[B\x1b[A\r".as_slice(), &first),
    ] {
        fixture
            .server
            .checked(&["switch-client", "-c", &tty, "-t", "origin"]);
        assert_eq!(client_fields(&fixture.server, &tty).1, fixture.origin_pane);
        let capture = fixture.scratch.join(format!("navigation-{index}.log"));
        let mut popup = fixture.popup(&binary, &tty, (120, 40), &capture, None);
        popup.wait_text("second-project");
        popup.send(keys);
        assert!(popup.wait_exit().success());
        assert_eq!(
            &client_fields(&fixture.server, &tty).1,
            expected,
            "navigation sequence {keys:?} selected the wrong pane"
        );
    }
    client.send(b"\x02d");
}

#[test]
fn packaged_detach_mutate_reconnect_rebuilds_and_switches_exact_new_client() {
    let Some(binary) = require_prerequisites() else {
        return;
    };
    let fixture = Fixture::new("reconnect");
    let before_dir = fixture.scratch.join("before-disconnect");
    fs::create_dir(&before_dir).unwrap();
    let old_pane = fixture.agent("old-agent", &before_dir, "old");
    fixture.title(&old_pane, "Before disconnect");
    fixture.install_binding(&binary, "a");
    let (mut old_client, old_tty) = fixture.client("origin", (120, 40), "old-client");
    old_client.send(b"\x02a");
    old_client.wait_text("Commands");
    wait(
        "installed popup process",
        || process_count(&binary) == 1,
        || format!("packaged process count={}", process_count(&binary)),
    );
    fixture
        .server
        .checked(&["detach-client", "-P", "-t", &old_tty]);
    let _ = old_client.wait_exit();
    wait(
        "popup process to die with disconnected client",
        || process_count(&binary) == 0,
        || format!("packaged process count={}", process_count(&binary)),
    );
    assert!(
        !fixture
            .server
            .checked(&["list-clients", "-F", "#{client_tty}"])
            .lines()
            .any(|tty| tty == old_tty)
    );

    fixture.server.checked(&["kill-pane", "-t", &old_pane]);
    let after_dir = fixture.scratch.join("after-reconnect");
    fs::create_dir(&after_dir).unwrap();
    let new_pane = fixture.agent("new-agent", &after_dir, "new");
    fixture.title(&new_pane, "After reconnect");

    let (mut new_client, new_tty) = fixture.client("origin", (62, 35), "new-client");
    new_client.send(b"\x02a");
    let screen = new_client.wait_text("After reconnect");
    assert!(
        screen.contains("After"),
        "rebuilt inventory omitted new title: {screen:?}"
    );
    assert!(!screen.contains("before-disconnect"));
    new_client.send(b"\r");
    wait(
        "reconnected client to switch to the rebuilt target",
        || client_fields(&fixture.server, &new_tty).1 == new_pane,
        || {
            format!(
                "expected pane {new_pane}; clients={}",
                fixture.server.checked(&[
                    "list-clients",
                    "-F",
                    "#{client_tty} #{client_session} #{pane_id}",
                ])
            )
        },
    );
    assert_eq!(client_fields(&fixture.server, &new_tty).1, new_pane);
    new_client.send(b"\x02d");
}

#[test]
fn installed_prefix_key_opens_the_extracted_binary_popup() {
    let Some(binary) = require_prerequisites() else {
        return;
    };
    let fixture = Fixture::new("prefix");
    let project = fixture.scratch.join("prefix-project");
    fs::create_dir(&project).unwrap();
    let pane = fixture.agent("prefix-agent", &project, "prefix");
    fixture.title(&pane, "Prefix activated");
    fixture.install_binding(&binary, "a");
    let binding = fixture.server.checked(&["list-keys", "-T", "prefix", "a"]);
    assert!(
        binding.contains("--client #{q:client_tty}") && !binding.contains("##{q:client_tty}"),
        "installed binding must retain the deferred runtime client format: {binding:?}"
    );

    let (mut client, _tty) = fixture.client("origin", (120, 40), "prefix-client");
    client.send(b"\x02a");
    let screen = client.wait_text("Commands");
    assert!(
        screen.contains("Prefix"),
        "prefix popup omitted target row: {screen:?}"
    );
    client.send(b"q");
    thread::sleep(Duration::from_millis(300));
    client.send(b"\x02d");
    assert!(client.wait_exit().success());
}

#[test]
fn installed_smart_left_moves_then_opens_the_extracted_binary_at_the_boundary() {
    let Some(binary) = require_prerequisites() else {
        return;
    };
    let fixture = Fixture::new("smart-left");
    fixture.install_smart_left(&binary);
    let output = fixture
        .server
        .command()
        .args(["respawn-pane", "-k", "-t", &fixture.origin_pane, "--"])
        .arg(&fixture.codex)
        .arg("smart-left")
        .output()
        .unwrap();
    assert_success(&output, "start packaged Smart Left composer fixture");
    fixture.server.wait("Smart Left composer cursor", || {
        pane_cursor_x(&fixture.server, &fixture.origin_pane) == 5
    });

    let capture = fixture.scratch.join("smart-left-client.log");
    let (mut client, _tty) = fixture.client("origin", (100, 32), "smart-left-client");
    client.send(b"\x1b[D");
    fixture.server.wait("ordinary packaged Left", || {
        pane_cursor_x(&fixture.server, &fixture.origin_pane) == 4
    });
    fixture.server.wait("ordinary packaged probe cleanup", || {
        smart_left_inactive(&fixture.server, &fixture.origin_pane)
    });
    assert!(!fs::read_to_string(&capture).unwrap().contains("sessions"));

    for expected_x in [3, 2] {
        client.send(b"\x1b[D");
        fixture
            .server
            .wait("packaged composer cursor movement", || {
                pane_cursor_x(&fixture.server, &fixture.origin_pane) == expected_x
            });
        fixture.server.wait("packaged movement probe cleanup", || {
            smart_left_inactive(&fixture.server, &fixture.origin_pane)
        });
    }
    client.send(b"\x1b[D");
    client.wait_text("sessions");
    assert_eq!(pane_cursor_x(&fixture.server, &fixture.origin_pane), 2);
    client.send(b"q");
    fixture.server.wait("packaged Smart Left cleanup", || {
        smart_left_inactive(&fixture.server, &fixture.origin_pane)
    });
    client.send(b"\x02d");
}

#[test]
fn packaged_setup_drives_prompt_aware_bash_and_zsh_then_remove_restores_files() {
    let Some(binary) = require_prerequisites() else {
        return;
    };
    for shell in ["bash", "zsh"] {
        let fixture = Fixture::new(&format!("shell-{shell}"));
        let bashrc = fixture.scratch.join(".bashrc");
        let zshrc = fixture.scratch.join(".zshrc");
        fs::write(&bashrc, "PS1='PROMPT> '\n").unwrap();
        fs::write(&zshrc, "PROMPT='PROMPT> '\n").unwrap();
        let setup = Command::new(&binary)
            .env("TMUX", fixture.server.environment())
            .env("HOME", fixture.scratch.path())
            .args(["--codex", text(&fixture.codex), "setup"])
            .arg("--tmux-config")
            .arg(&fixture.config)
            .arg("--bash-config")
            .arg(&bashrc)
            .arg("--zsh-config")
            .arg(&zshrc)
            .output()
            .unwrap();
        assert_success(&setup, "packaged aggregate setup");

        let respawn_shell = || {
            let mut respawn = fixture.server.command();
            respawn.args([
                "respawn-pane",
                "-k",
                "-t",
                &fixture.origin_pane,
                "--",
                "env",
            ]);
            respawn.arg(format!("HOME={}", fixture.scratch.path().display()));
            if shell == "bash" {
                respawn.args(["bash", "--noprofile", "--rcfile"]);
                respawn.arg(&bashrc);
                respawn.arg("-i");
            } else {
                respawn.arg(format!("ZDOTDIR={}", fixture.scratch.path().display()));
                respawn.args(["zsh", "-d", "-o", "interactive"]);
            }
            assert_success(
                &respawn.output().unwrap(),
                "start packaged configured shell",
            );
        };
        respawn_shell();
        fixture.server.wait("packaged shell prompt marker", || {
            fixture
                .server
                .checked(&[
                    "show-options",
                    "-pqv",
                    "-t",
                    &fixture.origin_pane,
                    "@codex_mux_shell_prompt",
                ])
                .trim()
                == "1"
        });
        let (mut client, _tty) = fixture.client("origin", (100, 32), "shell-client");
        fixture
            .server
            .checked(&["send-keys", "-t", &fixture.origin_pane, "echo '", "Enter"]);
        fixture
            .server
            .wait("packaged secondary prompt clears marker", || {
                fixture
                    .server
                    .run(&[
                        "show-options",
                        "-pqv",
                        "-t",
                        &fixture.origin_pane,
                        "@codex_mux_shell_prompt",
                    ])
                    .stdout
                    .is_empty()
            });
        client.send(b"\x1b[D");
        thread::sleep(Duration::from_millis(100));
        assert!(!client.wait_text("PROMPT>").contains("sessions"));
        respawn_shell();
        fixture.server.wait("packaged primary prompt returns", || {
            fixture
                .server
                .checked(&[
                    "show-options",
                    "-pqv",
                    "-t",
                    &fixture.origin_pane,
                    "@codex_mux_shell_prompt",
                ])
                .trim()
                == "1"
        });
        let boundary = pane_cursor_x(&fixture.server, &fixture.origin_pane);
        client.send(b"abc");
        fixture.server.wait("packaged shell input", || {
            pane_cursor_x(&fixture.server, &fixture.origin_pane) == boundary + 3
        });
        client.send(b"\x1b[D");
        fixture.server.wait("packaged shell ordinary Left", || {
            pane_cursor_x(&fixture.server, &fixture.origin_pane) == boundary + 2
        });
        client.send(b"\x01\x0b");
        fixture.server.wait("packaged shell cleared input", || {
            pane_cursor_x(&fixture.server, &fixture.origin_pane) == boundary
        });
        fixture
            .server
            .checked(&["send-keys", "-t", &fixture.origin_pane, "read -r", "Enter"]);
        fixture
            .server
            .wait("packaged shell read clears marker", || {
                fixture
                    .server
                    .run(&[
                        "show-options",
                        "-pqv",
                        "-t",
                        &fixture.origin_pane,
                        "@codex_mux_shell_prompt",
                    ])
                    .stdout
                    .is_empty()
            });
        client.send(b"\x1b[D");
        thread::sleep(Duration::from_millis(100));
        assert!(!client.wait_text("PROMPT>").contains("sessions"));
        respawn_shell();
        fixture.server.wait(
            "fresh packaged shell prompt after read safety check",
            || {
                fixture
                    .server
                    .checked(&[
                        "show-options",
                        "-pqv",
                        "-t",
                        &fixture.origin_pane,
                        "@codex_mux_shell_prompt",
                    ])
                    .trim()
                    == "1"
            },
        );
        client.send(b"\x1b[D");
        client.wait_text("sessions");
        client.send(b"q");
        fixture.server.wait("packaged shell probe cleanup", || {
            smart_left_inactive(&fixture.server, &fixture.origin_pane)
        });
        client.send(b"\x02d");

        let removed = Command::new(&binary)
            .env("TMUX", fixture.server.environment())
            .env("HOME", fixture.scratch.path())
            .arg("remove")
            .arg("--tmux-config")
            .arg(&fixture.config)
            .arg("--bash-config")
            .arg(&bashrc)
            .arg("--zsh-config")
            .arg(&zshrc)
            .output()
            .unwrap();
        assert_success(&removed, "packaged aggregate remove");
        assert_eq!(fs::read_to_string(&bashrc).unwrap(), "PS1='PROMPT> '\n");
        assert_eq!(fs::read_to_string(&zshrc).unwrap(), "PROMPT='PROMPT> '\n");
    }
}

#[test]
fn packaged_new_resume_fallback_and_confirmed_close_cross_process_boundaries() {
    let Some(binary) = require_prerequisites() else {
        return;
    };
    for (label, keys, expected) in [
        ("new", &b"ns"[..], None),
        ("resume", &b"r"[..], Some("arg2=resume\narg3=--all\n")),
    ] {
        let fixture = Fixture::new(label);
        let fallback = fixture.scratch.join("invoking-fallback");
        fs::create_dir(&fallback).unwrap();
        let (mut client, tty) = fixture.client("origin", (120, 40), "launch-client");
        fs::write(&fixture.log, "").unwrap();
        let capture = fixture.scratch.join("launch.log");
        let mut popup =
            fixture.popup_with_path(&binary, &tty, &fallback, (120, 40), &capture, None);
        popup.wait_text("codex-mux");
        popup.send(keys);
        assert!(popup.wait_exit().success());
        let log = wait_file(&fixture.log, "cwd=");
        assert!(
            log.contains(&format!("cwd={}\n", fallback.display())),
            "{log}"
        );
        assert!(
            log.contains("arg0=-c\narg1=tui.terminal_title=[\"thread-id\"]\n"),
            "{log}"
        );
        match expected {
            Some(arguments) => assert!(log.contains(arguments), "{log}"),
            None => assert!(!log.contains("resume"), "{log}"),
        }
        client.send(b"\x02d");
    }

    let fixture = Fixture::new("close");
    let pane = fixture.agent("agent", fixture.scratch.path(), "close");
    fixture.title(&pane, "Close target");
    let unrelated = fixture.shell("unrelated");
    let (mut client, tty) = fixture.client("origin", (120, 40), "close-client");
    let capture = fixture.scratch.join("close.log");
    let mut popup = fixture.popup(&binary, &tty, (120, 40), &capture, None);
    popup.wait_text("Close target");
    popup.send(b"xq");
    thread::sleep(Duration::from_millis(100));
    assert!(pane_exists(&fixture.server, &pane));
    popup.send(b"x");
    thread::sleep(Duration::from_millis(100));
    popup.send(b"x");
    fixture
        .server
        .wait("confirmed close", || !pane_exists(&fixture.server, &pane));
    popup.send(b"q");
    assert!(popup.wait_exit().success());
    assert!(pane_exists(&fixture.server, &unrelated));
    client.send(b"\x02d");
}

#[test]
fn packaged_action_failure_restores_terminal_and_does_not_escape_sandbox() {
    let Some(binary) = require_prerequisites() else {
        return;
    };
    let fixture = Fixture::new("failure");
    let pane = fixture.agent("agent", fixture.scratch.path(), "failure");
    fixture.title(&pane, "Failure target");
    let capture = fixture.scratch.join("failure.log");
    let before = fixture.scratch.join("failure-before");
    let after = fixture.scratch.join("failure-after");
    let mut popup = fixture.popup(
        &binary,
        "/dev/pts/definitely-missing",
        (120, 40),
        &capture,
        Some((&before, &after)),
    );
    popup.wait_text("Failure target");
    popup.send(b"\r");
    assert!(!popup.wait_exit().success());
    let output = fs::read_to_string(&capture).unwrap();
    assert!(
        output.contains("codex-mux:"),
        "missing bounded action error: {output:?}"
    );
    assert_eq!(
        fs::read_to_string(before).unwrap(),
        fs::read_to_string(after).unwrap()
    );
    assert!(pane_exists(&fixture.server, &pane));
}

struct Fixture {
    scratch: Scratch,
    config: PathBuf,
    server: Server,
    codex: PathBuf,
    log: PathBuf,
    origin_pane: String,
    origin_session: String,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let scratch = Scratch::new(label);
        let config = scratch.join("tmux.conf");
        fs::write(
            &config,
            "set -g status off\nset -g prefix C-b\nset -g default-shell /bin/sh\nset -g default-command /bin/sh\n",
        )
        .unwrap();
        let codex = fake_codex(&scratch);
        let log = scratch.join("codex.log");
        let server = Server::start(&config, scratch.path());
        server.checked(&["set-environment", "-g", "CODEX_MUX_E2E_LOG", text(&log)]);
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
            config,
            server,
            codex,
            log,
            origin_pane,
            origin_session,
        }
    }

    fn agent(&self, session: &str, cwd: &Path, marker: &str) -> String {
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
        assert_success(&output, "start fake Codex process");
        let pane = self
            .server
            .checked(&["display-message", "-p", "-t", session, "#{pane_id}"])
            .trim()
            .to_owned();
        self.server.wait("fake Codex foreground", || {
            self.server
                .checked(&[
                    "display-message",
                    "-p",
                    "-t",
                    &pane,
                    "#{pane_current_command}",
                ])
                .contains("codex-e2e")
        });
        pane
    }

    fn shell(&self, session: &str) -> String {
        let output = self
            .server
            .command()
            .args(["new-session", "-d", "-s", session, "-c"])
            .arg(self.scratch.path())
            .arg("/bin/sh")
            .output()
            .unwrap();
        assert_success(&output, "start unrelated session");
        self.server
            .checked(&["display-message", "-p", "-t", session, "#{pane_id}"])
            .trim()
            .to_owned()
    }

    fn title(&self, pane: &str, title: &str) {
        self.server
            .checked(&["select-pane", "-t", pane, "-T", title]);
    }

    fn install_binding(&self, binary: &Path, key: &str) {
        let output = Command::new(binary)
            .env("TMUX", self.server.environment())
            .args([
                "--codex",
                text(&self.codex),
                "tmux",
                "install",
                "--key",
                key,
                "--config",
            ])
            .arg(&self.config)
            .output()
            .unwrap();
        assert_success(&output, "install extracted binary prefix binding");
    }

    fn install_smart_left(&self, binary: &Path) {
        let output = Command::new(binary)
            .env("TMUX", self.server.environment())
            .args([
                "--codex",
                text(&self.codex),
                "tmux",
                "install",
                "--smart-left",
                "--config",
            ])
            .arg(&self.config)
            .output()
            .unwrap();
        assert_success(&output, "install extracted binary Smart Left binding");
    }

    fn client(&self, session: &str, size: (u16, u16), label: &str) -> (Pty, String) {
        let before = self
            .server
            .checked(&["list-clients", "-F", "#{client_tty}"]);
        let client = Pty::attach(
            &self.server,
            session,
            size,
            &self.scratch.join(format!("{label}.log")),
        );
        let mut tty = None;
        self.server.wait("new tmux client", || {
            tty = self
                .server
                .checked(&["list-clients", "-F", "#{client_tty}"])
                .lines()
                .find(|candidate| !before.lines().any(|old| old == *candidate))
                .map(str::to_owned);
            tty.is_some()
        });
        (client, tty.unwrap())
    }

    fn popup(
        &self,
        binary: &Path,
        tty: &str,
        size: (u16, u16),
        capture: &Path,
        stty: Option<(&Path, &Path)>,
    ) -> Pty {
        self.popup_with_path(binary, tty, self.scratch.path(), size, capture, stty)
    }

    fn popup_with_path(
        &self,
        binary: &Path,
        tty: &str,
        cwd: &Path,
        size: (u16, u16),
        capture: &Path,
        stty: Option<(&Path, &Path)>,
    ) -> Pty {
        let arguments = vec![
            "--codex".to_owned(),
            text(&self.codex).to_owned(),
            "--client".to_owned(),
            tty.to_owned(),
            "--invoking-pane".to_owned(),
            self.origin_pane.clone(),
            "--invoking-session".to_owned(),
            self.origin_session.clone(),
            "--invoking-path".to_owned(),
            text(cwd).to_owned(),
        ];
        let tmux = self.server.environment();
        let home = std::env::var("HOME").unwrap();
        let xdg = std::env::var("XDG_CONFIG_HOME").unwrap();
        Pty::binary(
            binary,
            &arguments,
            &[("TMUX", &tmux), ("HOME", &home), ("XDG_CONFIG_HOME", &xdg)],
            size,
            capture,
            stty,
        )
    }
}

fn fake_codex(scratch: &Scratch) -> PathBuf {
    let source = scratch.join("fake.rs");
    let binary = scratch.join("codex-e2e");
    fs::write(
        &source,
        r#"
use std::{env, fs::OpenOptions, io::{Read, Write}, process::Command, thread, time::Duration};

fn run_smart_left_fixture() {
    assert!(Command::new("stty").args(["raw", "-echo"]).status().unwrap().success());
    let mut output = std::io::stdout().lock();
    let mut input = std::io::stdin().lock();
    let mut cursor = 3usize;
    write!(output, "\x1b[2J\x1b[H› abc\x1b[1;{}H", cursor + 3).unwrap();
    output.flush().unwrap();
    let mut byte = [0_u8; 1];
    loop {
        input.read_exact(&mut byte).unwrap();
        if byte[0] != 0x1b { continue; }
        let mut tail = [0_u8; 2];
        input.read_exact(&mut tail).unwrap();
        if tail == *b"[D" {
            cursor = cursor.saturating_sub(1);
            write!(output, "\x1b[1;{}H", cursor + 3).unwrap();
            output.flush().unwrap();
        }
    }
}

fn main() {
    let mut log = OpenOptions::new().create(true).append(true)
        .open(env::var_os("CODEX_MUX_E2E_LOG").unwrap()).unwrap();
    writeln!(log, "cwd={}", env::current_dir().unwrap().display()).unwrap();
    for (index, argument) in env::args().skip(1).enumerate() {
        writeln!(log, "arg{index}={argument}").unwrap();
    }
    writeln!(log, "---").unwrap();
    if env::args().nth(1).as_deref() == Some("smart-left") {
        drop(log);
        run_smart_left_fixture();
    }
    thread::sleep(Duration::from_secs(300));
}
"#,
    )
    .unwrap();
    let output = Command::new("rustc")
        .args(["--edition=2024", "-o"])
        .arg(&binary)
        .arg(source)
        .output()
        .unwrap();
    assert_success(&output, "compile fake Codex executable");
    binary
}

fn client_fields(server: &Server, target: &str) -> (String, String) {
    server
        .checked(&[
            "list-clients",
            "-F",
            "#{client_tty}\u{1f}#{client_session}\u{1f}#{pane_id}",
        ])
        .replace("\\037", "\u{1f}")
        .lines()
        .find_map(|line| {
            let mut fields = line.split('\u{1f}');
            let tty = fields.next()?;
            let session = fields.next()?;
            let pane = fields.next()?;
            (tty == target).then(|| (session.to_owned(), pane.to_owned()))
        })
        .unwrap_or_else(|| panic!("client {target:?} disappeared"))
}

fn pane_exists(server: &Server, pane: &str) -> bool {
    let output = server.run(&["list-panes", "-a", "-F", "#{pane_id}"]);
    output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|candidate| candidate == pane)
}

fn pane_cursor_x(server: &Server, pane: &str) -> u16 {
    server
        .checked(&["display-message", "-p", "-t", pane, "#{cursor_x}"])
        .trim()
        .parse()
        .unwrap()
}

fn smart_left_inactive(server: &Server, pane: &str) -> bool {
    server
        .run(&[
            "show-options",
            "-pqv",
            "-t",
            pane,
            "@codex_mux_smart_left_active",
        ])
        .stdout
        .is_empty()
}

fn process_count(executable: &Path) -> usize {
    let expected = executable.canonicalize().unwrap();
    fs::read_dir("/proc")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
                && fs::read_link(entry.path().join("exe")).is_ok_and(|path| path == expected)
        })
        .count()
}

fn wait_file(path: &Path, expected: &str) -> String {
    wait(
        &format!("{expected:?} in {}", path.display()),
        || fs::read_to_string(path).is_ok_and(|text| text.contains(expected)),
        || fs::read_to_string(path).unwrap_or_default(),
    );
    fs::read_to_string(path).unwrap()
}

fn text(path: &Path) -> &str {
    path.to_str().expect("packaged E2E paths must be UTF-8")
}
