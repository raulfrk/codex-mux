mod support;

use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

use codex_mux::{
    Result,
    domain::{CodexExecutable, CommandOutput, Pane, PaneId, SessionId, TmuxCommandRunner},
    smart_naming::GeneratedName,
    tmux::{actions::TmuxActions, owned_names::OwnedTmuxNames},
};

use support::{PtyProcess, Scratch, TmuxServer, assert_success, serial_tmux_test, tools_available};

#[derive(Clone)]
struct SocketRunner(String);

impl TmuxCommandRunner for SocketRunner {
    fn run(&self, arguments: &[OsString]) -> Result<CommandOutput> {
        let output = Command::new("tmux")
            .args(["-L", &self.0])
            .args(arguments)
            .output()
            .unwrap();
        Ok(CommandOutput {
            stdout: output.stdout,
            stderr: output.stderr,
            status: output.status.code(),
        })
    }
}

#[test]
fn smart_naming_targets_a_background_pane_format_context() {
    let _serial = serial_tmux_test();
    if !tools_available() {
        return;
    }
    let scratch = Scratch::new("owned-background");
    let config = scratch.join("tmux.conf");
    fs::write(&config, "set -g status off\nset -g automatic-rename on\n").unwrap();
    let server = TmuxServer::start(&config, "target", scratch.path());
    let pane = server
        .checked(&["display-message", "-p", "-t", "target", "#{pane_id}"])
        .trim()
        .to_owned();
    let thread_id = "12345678-1234-1234-1234-123456789abc";
    server.checked(&[
        "respawn-pane",
        "-k",
        "-t",
        &pane,
        "-c",
        path(scratch.path()),
        "sleep 60",
    ]);
    server.checked(&["select-pane", "-t", &pane, "-T", thread_id]);
    let source_title = server
        .checked(&["display-message", "-p", "-t", &pane, "#{pane_title}"])
        .trim()
        .to_owned();
    let original_window_name = server
        .checked(&["display-message", "-p", "-t", &pane, "#{window_name}"])
        .trim()
        .to_owned();
    server.checked(&[
        "new-session",
        "-d",
        "-s",
        "foreground",
        "-c",
        path(scratch.path()),
    ]);

    let names = HashMap::from([(
        PaneId::new(&pane).unwrap(),
        GeneratedName {
            thread_id: thread_id.to_owned(),
            source_title,
            source_cwd: scratch.path().to_owned(),
            name: "Background naming works".to_owned(),
            generated_at_unix: 1_700_000_000,
        },
    )]);
    let owned = OwnedTmuxNames::new(SocketRunner(server.socket().to_owned()));
    owned.reconcile(&names);

    assert_eq!(
        server
            .checked(&["display-message", "-p", "-t", "target", "#{window_name}"])
            .trim(),
        original_window_name
    );
    assert_eq!(
        server
            .checked(&[
                "display-message",
                "-p",
                "-t",
                "target",
                "#{automatic-rename}"
            ])
            .trim(),
        "1"
    );
    assert_ne!(
        server
            .checked(&[
                "display-message",
                "-p",
                "-t",
                "foreground",
                "#{window_name}"
            ])
            .trim(),
        "Background naming works"
    );
    assert_eq!(
        server
            .checked(&[
                "display-message",
                "-p",
                "-t",
                "foreground",
                "#{automatic-rename}"
            ])
            .trim(),
        "1"
    );

    server.checked(&["rename-window", "-t", &pane, "Manual override"]);
    owned.reconcile(&names);
    for (option, expected) in [
        ("@codex_mux_generated_thread", thread_id),
        ("@codex_mux_generated_name", "Background naming works"),
        ("@codex_mux_generated_source_title", thread_id),
        ("@codex_mux_generated_source_cwd", path(scratch.path())),
        ("@codex_mux_generated_at", "1700000000"),
    ] {
        assert_eq!(
            server
                .checked(&["show-options", "-pv", "-t", &pane, option])
                .trim(),
            expected,
            "pane-local metadata mismatch for {option}"
        );
    }
}

#[test]
fn manual_pane_rename_relinquishes_smart_naming_ownership_in_real_tmux() {
    let _serial = serial_tmux_test();
    if !tools_available() {
        return;
    }
    let scratch = Scratch::new("manual-pane-rename");
    let config = scratch.join("tmux.conf");
    fs::write(&config, "set -g status off\n").unwrap();
    let server = TmuxServer::start(&config, "target", scratch.path());
    let pane_id = server
        .checked(&["display-message", "-p", "-t", "target", "#{pane_id}"])
        .trim()
        .to_owned();
    let thread_id = "12345678-1234-1234-1234-123456789abc";
    let pane_pid = server
        .checked(&["display-message", "-p", "-t", &pane_id, "#{pane_pid}"])
        .trim()
        .parse()
        .unwrap();
    let session_id = server
        .checked(&["display-message", "-p", "-t", &pane_id, "#{session_id}"])
        .trim()
        .to_owned();
    server.checked(&["select-pane", "-t", &pane_id, "-T", thread_id]);
    let pane = Pane {
        id: PaneId::new(&pane_id).unwrap(),
        session_id: SessionId::new(session_id).unwrap(),
        title: Some(thread_id.to_owned()),
        generated_title: Some("Generated name".to_owned()),
        generated_at_unix: Some(1_700_000_000),
        immediate_naming: true,
        manual_name: false,

        manual_name_source: None,

        manual_name_pid: None,

        manual_name_session: None,

        pane_pid,
        current_path: scratch.path().to_owned(),
    };
    let names = HashMap::from([(
        pane.id.clone(),
        GeneratedName {
            thread_id: thread_id.to_owned(),
            source_title: thread_id.to_owned(),
            source_cwd: scratch.path().to_owned(),
            name: "Generated name".to_owned(),
            generated_at_unix: 1_700_000_000,
        },
    )]);
    let runner = SocketRunner(server.socket().to_owned());
    let owned = OwnedTmuxNames::new(runner.clone());
    owned.reconcile(&names);

    let executable = CodexExecutable::new("/bin/true").unwrap();
    TmuxActions::new(&runner, &executable)
        .rename_pane(&pane, "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
        .unwrap();
    owned.reconcile(&names);

    assert_eq!(
        server
            .checked(&["display-message", "-p", "-t", &pane_id, "#{pane_title}"])
            .trim(),
        "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
    );
    for option in [
        "@codex_mux_generated_thread",
        "@codex_mux_generated_name",
        "@codex_mux_generated_source_title",
        "@codex_mux_generated_source_cwd",
        "@codex_mux_generated_at",
        "@codex_mux_name_now",
    ] {
        assert!(
            !server
                .run(&["show-options", "-pv", "-t", &pane_id, option])
                .status
                .success(),
            "manual rename retained generated marker {option}"
        );
    }
    assert_eq!(
        server
            .checked(&[
                "show-options",
                "-pv",
                "-t",
                &pane_id,
                "@codex_mux_manual_name",
            ])
            .trim(),
        "1"
    );
    assert_eq!(
        server
            .checked(&[
                "show-options",
                "-pv",
                "-t",
                &pane_id,
                "@codex_mux_manual_name_source"
            ])
            .trim(),
        thread_id
    );
    let mut pinned = pane.clone();
    pinned.manual_name = true;
    pinned.title = Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_owned());
    pinned.manual_name_source = Some(thread_id.to_owned());
    pinned.manual_name_pid = Some(pane_pid);
    pinned.manual_name_session = Some(pinned.session_id.clone());
    TmuxActions::new(&runner, &executable)
        .unpin_pane(&pinned)
        .unwrap();
    assert_eq!(
        server
            .checked(&["display-message", "-p", "-t", &pane_id, "#{pane_title}"])
            .trim(),
        thread_id
    );
    assert_eq!(
        server
            .checked(&["show-options", "-pv", "-t", &pane_id, "@codex_mux_name_now"])
            .trim(),
        "1"
    );
    assert!(
        !server
            .run(&[
                "show-options",
                "-pv",
                "-t",
                &pane_id,
                "@codex_mux_manual_name"
            ])
            .status
            .success()
    );
}

#[test]
fn manual_pane_rename_keeps_tmux_format_syntax_literal_in_real_tmux() {
    let _serial = serial_tmux_test();
    if !tools_available() {
        return;
    }
    let scratch = Scratch::new("manual-pane-format-literal");
    let config = scratch.join("tmux.conf");
    fs::write(&config, "set -g status off\n").unwrap();
    let server = TmuxServer::start(&config, "target", scratch.path());
    let pane_id = server
        .checked(&["display-message", "-p", "-t", "target", "#{pane_id}"])
        .trim()
        .to_owned();
    server.checked(&["select-pane", "-t", &pane_id, "-T", "thread"]);
    let pane_pid = server
        .checked(&["display-message", "-p", "-t", &pane_id, "#{pane_pid}"])
        .trim()
        .parse()
        .unwrap();
    let session_id = server
        .checked(&["display-message", "-p", "-t", &pane_id, "#{session_id}"])
        .trim()
        .to_owned();
    let pane = Pane {
        id: PaneId::new(&pane_id).unwrap(),
        session_id: SessionId::new(session_id).unwrap(),
        title: Some("thread".to_owned()),
        generated_title: None,
        generated_at_unix: None,
        immediate_naming: false,
        manual_name: false,

        manual_name_source: None,

        manual_name_pid: None,

        manual_name_session: None,

        pane_pid,
        current_path: scratch.path().to_owned(),
    };
    let executable = CodexExecutable::new("/bin/true").unwrap();
    let title = "plain#hash a#{pane_id}b #[fg=red] ##[bg=blue] ###[none] #(hostname) ##";

    TmuxActions::new(&SocketRunner(server.socket().to_owned()), &executable)
        .rename_pane(&pane, title)
        .unwrap();

    assert_eq!(
        server
            .checked(&["display-message", "-p", "-t", &pane_id, "#{pane_title}"])
            .trim(),
        title
    );

    let literal_pane_id = server
        .checked(&[
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            &pane_id,
            "sleep",
            "30",
        ])
        .trim()
        .to_owned();
    server.checked(&["select-pane", "-t", &literal_pane_id, "-T", "thread"]);
    let literal_pid = server
        .checked(&[
            "display-message",
            "-p",
            "-t",
            &literal_pane_id,
            "#{pane_pid}",
        ])
        .trim()
        .parse()
        .unwrap();
    let literal_pane = Pane {
        id: PaneId::new(&literal_pane_id).unwrap(),
        session_id: pane.session_id.clone(),
        title: Some("thread".to_owned()),
        generated_title: None,
        generated_at_unix: None,
        immediate_naming: false,
        manual_name: false,
        manual_name_source: None,
        manual_name_pid: None,
        manual_name_session: None,
        pane_pid: literal_pid,
        current_path: scratch.path().to_owned(),
    };
    TmuxActions::new(&SocketRunner(server.socket().to_owned()), &executable)
        .rename_pane(&literal_pane, ";")
        .unwrap();
    assert_eq!(
        server
            .checked(&[
                "display-message",
                "-p",
                "-t",
                &literal_pane_id,
                "#{pane_title}"
            ])
            .trim(),
        ";"
    );
}

#[test]
fn smart_naming_migrates_a_legacy_owned_window_in_real_tmux() {
    let _serial = serial_tmux_test();
    if !tools_available() {
        return;
    }
    let scratch = Scratch::new("legacy-smart-name");
    let config = scratch.join("tmux.conf");
    fs::write(
        &config,
        "set -g status off\nset -g automatic-rename on\nset -g automatic-rename-format '#{pane_current_command}'\n",
    )
    .unwrap();
    let server = TmuxServer::start(&config, "target", scratch.path());
    let pane = server
        .checked(&["display-message", "-p", "-t", "target", "#{pane_id}"])
        .trim()
        .to_owned();
    let thread_id = "12345678-1234-1234-1234-123456789abc";
    server.checked(&[
        "respawn-pane",
        "-k",
        "-t",
        &pane,
        "-c",
        path(scratch.path()),
        "sleep 60",
    ]);
    server.checked(&["select-pane", "-t", &pane, "-T", thread_id]);
    for (scope, option, value) in [
        ("-p", "@codex_mux_generated_source_title", thread_id),
        (
            "-p",
            "@codex_mux_generated_source_cwd",
            path(scratch.path()),
        ),
        ("-w", "@codex_mux_generated_thread", thread_id),
        ("-w", "@codex_mux_generated_name", "Legacy smart title"),
    ] {
        server.checked(&["set-option", scope, "-t", &pane, option, value]);
    }
    server.checked(&["rename-window", "-t", &pane, "Legacy smart title"]);
    server.checked(&["set-option", "-w", "-t", &pane, "automatic-rename", "off"]);

    OwnedTmuxNames::new(SocketRunner(server.socket().to_owned()))
        .migrate_legacy_window_names(1_700_000_000);

    assert_eq!(
        server
            .checked(&["display-message", "-p", "-t", &pane, "#{automatic-rename}"])
            .trim(),
        "1"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let name = server
            .checked(&["display-message", "-p", "-t", &pane, "#{window_name}"])
            .trim()
            .to_owned();
        if name != "Legacy smart title" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "tmux did not resume automatic window naming"
        );
        thread::sleep(Duration::from_millis(25));
    }
    for option in ["@codex_mux_generated_thread", "@codex_mux_generated_name"] {
        assert!(
            !server
                .run(&["show-options", "-wv", "-t", &pane, option])
                .status
                .success(),
            "legacy window option remained: {option}"
        );
    }
    for (option, expected) in [
        ("@codex_mux_generated_thread", thread_id),
        ("@codex_mux_generated_name", "Legacy smart title"),
        ("@codex_mux_generated_at", "1700000000"),
    ] {
        assert_eq!(
            server
                .checked(&["show-options", "-pv", "-t", &pane, option])
                .trim(),
            expected
        );
    }
}

#[test]
fn smart_naming_does_not_migrate_a_pane_local_lookalike() {
    let _serial = serial_tmux_test();
    if !tools_available() {
        return;
    }
    let scratch = Scratch::new("pane-local-smart-name");
    let config = scratch.join("tmux.conf");
    fs::write(&config, "set -g status off\nset -g automatic-rename on\n").unwrap();
    let server = TmuxServer::start(&config, "target", scratch.path());
    let pane = server
        .checked(&["display-message", "-p", "-t", "target", "#{pane_id}"])
        .trim()
        .to_owned();
    let thread_id = "12345678-1234-1234-1234-123456789abc";
    server.checked(&["select-pane", "-t", &pane, "-T", thread_id]);
    for (option, value) in [
        ("@codex_mux_generated_thread", thread_id),
        ("@codex_mux_generated_name", "Intentional window title"),
        ("@codex_mux_generated_source_title", thread_id),
        ("@codex_mux_generated_source_cwd", path(scratch.path())),
        ("@codex_mux_generated_at", "1700000000"),
    ] {
        server.checked(&["set-option", "-p", "-t", &pane, option, value]);
    }
    server.checked(&["rename-window", "-t", &pane, "Intentional window title"]);
    server.checked(&["set-option", "-w", "-t", &pane, "automatic-rename", "off"]);

    OwnedTmuxNames::new(SocketRunner(server.socket().to_owned()))
        .migrate_legacy_window_names(1_800_000_000);

    assert_eq!(
        server
            .checked(&["display-message", "-p", "-t", &pane, "#{window_name}"])
            .trim(),
        "Intentional window title"
    );
    assert_eq!(
        server
            .checked(&["display-message", "-p", "-t", &pane, "#{automatic-rename}"])
            .trim(),
        "0"
    );
    for option in ["@codex_mux_generated_thread", "@codex_mux_generated_name"] {
        assert!(
            !server
                .run(&["show-options", "-wv", "-t", &pane, option])
                .status
                .success(),
            "pane-local option leaked into window scope: {option}"
        );
    }
}

#[test]
fn installer_cli_loads_a_real_prefix_binding_with_responsive_geometry() {
    let _serial = serial_tmux_test();
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
    let _serial = serial_tmux_test();
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
fn uninstall_refuses_a_foreign_live_root_left_and_restores_its_prefix_binding() {
    let _serial = serial_tmux_test();
    if !tools_available() {
        eprintln!("tmux or util-linux script is unavailable; skipping tmux E2E test");
        return;
    }

    let scratch = Scratch::new("foreign-smart-left");
    let config = scratch.join("tmux.conf");
    fs::write(&config, "set -g status off\n").unwrap();
    let codex = fake_codex(&scratch);
    let server = TmuxServer::start(&config, "origin", scratch.path());
    let tmux = server.tmux_environment();
    let installed = binary()
        .env("TMUX", &tmux)
        .args([
            "--codex",
            path(&codex),
            "tmux",
            "install",
            "--smart-left",
            "--config",
            path(&config),
        ])
        .output()
        .unwrap();
    assert_success(&installed, "install Smart Left before live drift");
    let before = fs::read(&config).unwrap();
    server.checked(&[
        "bind-key",
        "-T",
        "root",
        "Left",
        "display-message",
        "foreign-left",
    ]);

    let uninstalled = binary()
        .env("TMUX", &tmux)
        .args(["tmux", "uninstall", "--config", path(&config)])
        .output()
        .unwrap();
    assert!(!uninstalled.status.success());
    assert!(
        String::from_utf8_lossy(&uninstalled.stderr)
            .contains("no longer matches the codex-mux-owned binding")
    );
    assert_eq!(fs::read(&config).unwrap(), before);
    assert!(
        server
            .checked(&["list-keys", "-T", "root", "Left"])
            .contains("foreign-left")
    );
    assert!(
        server
            .run(&["list-keys", "-T", "prefix", "a"])
            .status
            .success()
    );
}

#[test]
fn smart_left_live_ownership_survives_tmux_escaping_in_the_mux_path() {
    let _serial = serial_tmux_test();
    if !tools_available() {
        eprintln!("tmux or util-linux script is unavailable; skipping tmux E2E test");
        return;
    }

    let scratch = Scratch::new("escaped-smart-left-owner");
    let config = scratch.join("tmux.conf");
    fs::write(&config, "set -g status off\n").unwrap();
    let codex = fake_codex(&scratch);
    let mux = scratch.join("mux executable'; semicolon");
    fs::copy(env!("CARGO_BIN_EXE_codex-mux"), &mux).unwrap();
    let server = TmuxServer::start(&config, "origin", scratch.path());
    let tmux = server.tmux_environment();

    let installed = Command::new(&mux)
        .env("TMUX", &tmux)
        .args([
            "--codex",
            path(&codex),
            "tmux",
            "install",
            "--smart-left",
            "--config",
            path(&config),
        ])
        .output()
        .unwrap();
    assert_success(&installed, "install Smart Left from escaped mux path");
    let binding = server.checked(&["list-keys", "-T", "root", "Left"]);
    assert!(binding.contains("owner="), "{binding}");

    let uninstalled = Command::new(&mux)
        .env("TMUX", &tmux)
        .args(["tmux", "uninstall", "--config", path(&config)])
        .output()
        .unwrap();
    assert_success(&uninstalled, "uninstall Smart Left from escaped mux path");
    assert!(
        !server
            .run(&["list-keys", "-T", "root", "Left"])
            .status
            .success()
    );
}

#[test]
fn physical_left_moves_inside_composer_and_opens_mux_only_at_its_boundary() {
    let _serial = serial_tmux_test();
    if !tools_available() {
        eprintln!("tmux or util-linux script is unavailable; skipping tmux E2E test");
        return;
    }

    let fixture = RuntimeFixture::new("smart-left");
    let installed = binary()
        .env("TMUX", fixture.server.tmux_environment())
        .args([
            "--codex",
            path(&fixture.codex),
            "tmux",
            "install",
            "--smart-left",
            "--config",
            path(&fixture.config),
        ])
        .output()
        .unwrap();
    assert_success(&installed, "install Smart Left binding");
    let respawned = fixture
        .server
        .command()
        .args(["respawn-pane", "-k", "-t", &fixture.origin_pane, "--"])
        .arg(&fixture.codex)
        .arg("smart-left")
        .output()
        .unwrap();
    assert_success(&respawned, "start deterministic composer fixture");
    fixture.server.wait_until("composer fixture cursor", || {
        fixture
            .server
            .checked(&[
                "display-message",
                "-p",
                "-t",
                &fixture.origin_pane,
                "#{pane_current_command} #{cursor_x} #{cursor_y}",
            ])
            .trim()
            == "codex-e2e 5 0"
    });

    let capture = fixture.scratch.join("smart-left-screen.log");
    let mut client = PtyProcess::attach_captured(&fixture.server, "origin", 100, 32, &capture);
    let _client_tty = wait_for_client(&fixture.server, "origin");

    client.send(b"\x1b[D");
    fixture
        .server
        .wait_until("ordinary Left cursor movement", || {
            pane_cursor_x(&fixture.server, &fixture.origin_pane) == 4
        });
    thread::sleep(Duration::from_millis(100));
    fixture
        .server
        .wait_until("ordinary Left probe cleanup", || {
            smart_left_inactive(&fixture.server, &fixture.origin_pane)
        });
    assert!(
        !fs::read_to_string(&capture)
            .unwrap_or_default()
            .contains("codex-mux"),
        "Smart Left opened before reaching the composer boundary"
    );

    for expected_x in [3, 2] {
        client.send(b"\x1b[D");
        fixture.server.wait_until("composer cursor movement", || {
            pane_cursor_x(&fixture.server, &fixture.origin_pane) == expected_x
        });
        fixture.server.wait_until("movement probe cleanup", || {
            smart_left_inactive(&fixture.server, &fixture.origin_pane)
        });
    }
    client.send(b"\x1b[D");
    fixture.server.wait_until("Smart Left popup render", || {
        fs::read_to_string(&capture).is_ok_and(|screen| screen.contains("sessions"))
    });
    assert_eq!(pane_cursor_x(&fixture.server, &fixture.origin_pane), 2);
    client.send(b"q");
    fixture
        .server
        .wait_until("Smart Left debounce cleanup", || {
            smart_left_inactive(&fixture.server, &fixture.origin_pane)
        });

    let popup_count = popup_command_count(&fixture.server);
    client.send(b"\x1b[D\x1b[D\x1b[D");
    fixture.server.wait_until("rapid Smart Left probe", || {
        !smart_left_inactive(&fixture.server, &fixture.origin_pane)
    });
    fixture.server.wait_until("rapid Smart Left popup", || {
        popup_command_count(&fixture.server) > popup_count
    });
    client.send(b"q");
    fixture.server.wait_until("rapid Smart Left cleanup", || {
        smart_left_inactive(&fixture.server, &fixture.origin_pane)
    });
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        popup_command_count(&fixture.server),
        popup_count + 1,
        "rapid Left created more than one popup probe"
    );
    let messages = fixture.server.checked(&["show-messages"]);
    assert!(!messages.contains("No such pane: #{pane_id}"), "{messages}");
    assert!(
        !messages.contains("Can't find pane: #{pane_id}"),
        "{messages}"
    );

    client.send(b"\x02d");
    let uninstalled = binary()
        .env("TMUX", fixture.server.tmux_environment())
        .args(["tmux", "uninstall", "--config", path(&fixture.config)])
        .output()
        .unwrap();
    assert_success(&uninstalled, "uninstall owned Smart Left binding");
    assert!(
        !fixture
            .server
            .run(&["list-keys", "-T", "root", "Left"])
            .status
            .success()
    );
}

#[test]
fn prompt_aware_smart_left_works_in_real_bash_and_zsh() {
    let _serial = serial_tmux_test();
    if !tools_available()
        || !["bash", "zsh"]
            .into_iter()
            .all(|shell| Command::new(shell).arg("--version").output().is_ok())
    {
        eprintln!("tmux, script, Bash, or Zsh is unavailable; skipping shell Smart Left E2E");
        return;
    }

    for shell in ["bash", "zsh"] {
        let fixture = RuntimeFixture::new(&format!("smart-left-{shell}"));
        let bashrc = fixture.scratch.join(".bashrc");
        let zshrc = fixture.scratch.join(".zshrc");
        fs::write(&bashrc, "PS1='PROMPT> '\n").unwrap();
        fs::write(&zshrc, "PROMPT='PROMPT> '\n").unwrap();
        let setup = binary()
            .env("TMUX", fixture.server.tmux_environment())
            .env("HOME", fixture.scratch.path())
            .args(["--codex", path(&fixture.codex), "setup"])
            .arg("--tmux-config")
            .arg(&fixture.config)
            .arg("--bash-config")
            .arg(&bashrc)
            .arg("--zsh-config")
            .arg(&zshrc)
            .output()
            .unwrap();
        assert_success(&setup, "install aggregate shell Smart Left setup");

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
                "start configured interactive shell",
            );
        };
        respawn_shell();
        fixture
            .server
            .wait_until("shell prompt lifecycle marker", || {
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

        let capture = fixture.scratch.join(format!("{shell}-screen.log"));
        let mut client = PtyProcess::attach_captured(&fixture.server, "origin", 100, 32, &capture);
        let _tty = wait_for_client(&fixture.server, "origin");
        let secondary_popup_count = popup_command_count(&fixture.server);
        fixture
            .server
            .checked(&["send-keys", "-t", &fixture.origin_pane, "echo '", "Enter"]);
        fixture
            .server
            .wait_until("secondary prompt clears marker", || {
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
        assert_eq!(
            popup_command_count(&fixture.server),
            secondary_popup_count,
            "{shell} opened the mux at a secondary prompt"
        );
        respawn_shell();
        fixture
            .server
            .wait_until("primary prompt returns after secondary prompt", || {
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
        if shell == "zsh" {
            let sentinel = fixture.scratch.join("later-precmd-ran");
            fixture.server.checked(&[
                "send-keys",
                "-t",
                &fixture.origin_pane,
                &format!(
                    "__codex_mux_test_later() {{ print -r -- hit >> {}; }}; add-zsh-hook precmd __codex_mux_test_later; false",
                    sentinel.display()
                ),
                "Enter",
            ]);
            fixture
                .server
                .wait_until("later Zsh precmd hook after false", || sentinel.is_file());
        }
        let boundary = pane_cursor_x(&fixture.server, &fixture.origin_pane);
        client.send(b"abc");
        fixture.server.wait_until("shell input rendered", || {
            pane_cursor_x(&fixture.server, &fixture.origin_pane) == boundary + 3
        });
        client.send(b"\x1b[D");
        fixture
            .server
            .wait_until("Left moves within shell input", || {
                pane_cursor_x(&fixture.server, &fixture.origin_pane) == boundary + 2
            });
        assert!(
            !fs::read_to_string(&capture)
                .unwrap_or_default()
                .contains("sessions"),
            "{shell} opened the mux while Left could still move"
        );

        client.send(b"\x01\x0b");
        fixture.server.wait_until("shell input cleared", || {
            pane_cursor_x(&fixture.server, &fixture.origin_pane) == boundary
        });
        fixture
            .server
            .checked(&["send-keys", "-t", &fixture.origin_pane, "read -r", "Enter"]);
        fixture
            .server
            .wait_until("shell read builtin clears prompt marker", || {
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
        let read_popup_count = popup_command_count(&fixture.server);
        client.send(b"\x1b[D");
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            popup_command_count(&fixture.server),
            read_popup_count,
            "{shell} opened the mux while read was consuming input"
        );
        respawn_shell();
        fixture
            .server
            .wait_until("fresh shell prompt after read safety check", || {
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
        let lifecycle_count = shell_prompt_unset_count(&fixture.server);
        fixture
            .server
            .checked(&["send-keys", "-t", &fixture.origin_pane, "true", "Enter"]);
        fixture.server.wait_until(
            "shell command lifecycle clears and restores prompt marker",
            || {
                shell_prompt_unset_count(&fixture.server) > lifecycle_count
                    && fixture
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
        let before = popup_command_count(&fixture.server);
        client.send(b"\x1b[D");
        fixture.server.wait_until("shell boundary opens mux", || {
            fs::read_to_string(&capture).is_ok_and(|screen| screen.contains("sessions"))
        });
        assert_eq!(popup_command_count(&fixture.server), before + 1);
        client.send(b"q");
        fixture.server.wait_until("shell probe cleanup", || {
            smart_left_inactive(&fixture.server, &fixture.origin_pane)
        });
        if shell == "bash" {
            let disabled = fixture.scratch.join("promptvars-disabled");
            fixture.server.checked(&[
                "send-keys",
                "-t",
                &fixture.origin_pane,
                &format!(
                    "shopt -u promptvars; printf disabled > {}",
                    disabled.display()
                ),
                "Enter",
            ]);
            fixture
                .server
                .wait_until("Bash promptvars-disabled command completes", || {
                    disabled.is_file()
                        && fixture
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
            let disabled_count = popup_command_count(&fixture.server);
            client.send(b"\x1b[D");
            thread::sleep(Duration::from_millis(100));
            assert_eq!(popup_command_count(&fixture.server), disabled_count);

            let reading = fixture.scratch.join("promptvars-read");
            fixture.server.checked(&[
                "send-keys",
                "-t",
                &fixture.origin_pane,
                &format!("printf reading > {}; read -r", reading.display()),
                "Enter",
            ]);
            fixture
                .server
                .wait_until("Bash promptvars-disabled read starts", || reading.is_file());
            client.send(b"\x1b[D");
            thread::sleep(Duration::from_millis(100));
            assert_eq!(popup_command_count(&fixture.server), disabled_count);

            respawn_shell();
            fixture
                .server
                .wait_until("fresh Bash prompt before promptvars PS2 check", || {
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
            fixture.server.checked(&[
                "send-keys",
                "-t",
                &fixture.origin_pane,
                "shopt -u promptvars",
                "Enter",
            ]);
            fixture.server.wait_until(
                "Bash promptvars-disabled primary prompt fails closed",
                || {
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
                },
            );
            fixture
                .server
                .checked(&["send-keys", "-t", &fixture.origin_pane, "echo '", "Enter"]);
            thread::sleep(Duration::from_millis(100));
            client.send(b"\x1b[D");
            thread::sleep(Duration::from_millis(100));
            assert_eq!(popup_command_count(&fixture.server), disabled_count);
        }
        client.send(b"\x02d");

        let removed = binary()
            .env("TMUX", fixture.server.tmux_environment())
            .env("HOME", fixture.scratch.path())
            .args(["remove"])
            .arg("--tmux-config")
            .arg(&fixture.config)
            .arg("--bash-config")
            .arg(&bashrc)
            .arg("--zsh-config")
            .arg(&zshrc)
            .output()
            .unwrap();
        assert_success(&removed, "remove aggregate shell Smart Left setup");
        assert_eq!(fs::read_to_string(&bashrc).unwrap(), "PS1='PROMPT> '\n");
        assert_eq!(fs::read_to_string(&zshrc).unwrap(), "PROMPT='PROMPT> '\n");
        assert!(
            !fixture
                .server
                .run(&["list-keys", "-T", "root", "Left"])
                .status
                .success()
        );
    }
}

#[test]
fn interactive_cli_selects_full_screen_for_only_the_named_client() {
    let _serial = serial_tmux_test();
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
    let _serial = serial_tmux_test();
    if !tools_available() {
        eprintln!("tmux or util-linux script is unavailable; skipping tmux E2E test");
        return;
    }

    for (keys, label, expected_tail) in [
        (&b"ns"[..], "new", ""),
        (&b"ny"[..], "yolo", "arg2=--yolo\n"),
        (&b"rs"[..], "resume", "arg2=resume\narg3=--all\n"),
        (
            &b"ry"[..],
            "resume-yolo",
            "arg2=--yolo\narg3=resume\narg4=--all\n",
        ),
    ] {
        let fixture = RuntimeFixture::new(label);
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

        let capture = fixture.scratch.join(format!("{label}-popup.capture"));
        let mut popup = fixture.interactive_captured(&client_tty, &capture);
        support::wait_for_file_text(&capture, "selected-cwd");
        popup.send(keys);
        popup.wait_for_exit();
        let log = support::wait_for_file_text(&fixture.log, "---\n");
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
        if label == "new" {
            assert!(
                !log.contains("resume"),
                "new action unexpectedly resumed: {log}"
            );
        }
        let launched_pane = client_pane(&fixture.server, &client_tty);
        assert_ne!(
            launched_pane, fixture.origin_pane,
            "launch left the invoking client on its original pane"
        );
        assert_eq!(
            client_pane(&fixture.server, &shared_tty),
            launched_pane,
            "clients attached to one tmux session must reflect its shared active window"
        );
        let marker = fixture.server.checked(&[
            "display-message",
            "-p",
            "-t",
            &launched_pane,
            "#{@codex_mux_name_now}",
        ]);
        assert_eq!(
            marker.trim(),
            if label.starts_with("resume") { "1" } else { "" },
            "only the exact pane created by Resume receives an immediate naming marker"
        );
        client.send(b"\x02d");
        shared_client.send(b"\x02d");
    }
}

#[test]
fn interactive_close_requires_confirmation_and_targets_only_the_selected_pane() {
    let _serial = serial_tmux_test();
    if !tools_available() {
        eprintln!("tmux or util-linux script is unavailable; skipping tmux E2E test");
        return;
    }

    let fixture = RuntimeFixture::new("close");
    let agent = fixture.new_agent("remote", fixture.scratch.path(), "existing");
    let unrelated = fixture.new_shell("unrelated", fixture.scratch.path());
    let mut client = PtyProcess::attach(&fixture.server, "origin", 120, 40);
    let client_tty = wait_for_client(&fixture.server, "origin");

    let capture = fixture.scratch.join("close-popup.capture");
    let mut popup = fixture.interactive_captured(&client_tty, &capture);
    support::wait_for_file_text(&capture, "codex-mux-e2e-close-");
    popup.send(b"x");
    support::wait_for_file_text(&capture, "Close selected pane?");
    let confirmation_end = fs::metadata(&capture).unwrap().len() as usize;
    popup.send(b"qx");
    support::wait_for_file_text_after(&capture, confirmation_end, "Close selected pane?");
    assert!(
        pane_exists(&fixture.server, &agent),
        "canceling and reopening confirmation killed the selected pane"
    );

    popup.send(b"qq");
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
    let capture = fixture.scratch.join("close-popup.capture");
    let mut popup = fixture.interactive_captured(&client_tty, &capture);
    support::wait_for_file_text(&capture, "codex-mux-e2e-close-confirmed-");
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
    let _serial = serial_tmux_test();
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
    config: PathBuf,
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
            config,
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
use std::{env, fs::OpenOptions, io::{Read, Write}, process::Command, thread, time::Duration};

fn run_smart_left_fixture() {
    let status = Command::new("stty").args(["raw", "-echo"]).status().unwrap();
    assert!(status.success());
    let mut output = std::io::stdout().lock();
    let mut input = std::io::stdin().lock();
    let mut cursor = 3usize;
    write!(output, "\x1b[2J\x1b[H› abc\x1b[1;{}H", cursor + 3).unwrap();
    output.flush().unwrap();
    let mut byte = [0_u8; 1];
    loop {
        input.read_exact(&mut byte).unwrap();
        if byte[0] != 0x1b {
            continue;
        }
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
    let log = env::var_os("CODEX_MUX_E2E_LOG").expect("test log path");
    let mut output = OpenOptions::new().create(true).append(true).open(log).unwrap();
    writeln!(output, "cwd={}", env::current_dir().unwrap().display()).unwrap();
    for (index, argument) in env::args().skip(1).enumerate() {
        writeln!(output, "arg{index}={argument}").unwrap();
    }
    writeln!(output, "---").unwrap();
    if env::args().nth(1).as_deref() == Some("smart-left") {
        drop(output);
        run_smart_left_fixture();
    }
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

fn pane_cursor_x(server: &TmuxServer, pane: &str) -> u16 {
    server
        .checked(&["display-message", "-p", "-t", pane, "#{cursor_x}"])
        .trim()
        .parse()
        .unwrap()
}

fn smart_left_inactive(server: &TmuxServer, pane: &str) -> bool {
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

fn popup_command_count(server: &TmuxServer) -> usize {
    server
        .checked(&["show-messages"])
        .lines()
        .filter(|line| line.contains(" command: display-popup "))
        .count()
}

fn shell_prompt_unset_count(server: &TmuxServer) -> usize {
    server
        .checked(&["show-messages"])
        .lines()
        .filter(|line| {
            line.contains("command: set-option -pu") && line.contains("@codex_mux_shell_prompt")
        })
        .count()
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
        .replace("\\037", "\u{1f}")
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
