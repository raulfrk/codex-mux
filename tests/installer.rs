use std::{
    fs,
    io::Write,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
    time::{SystemTime, UNIX_EPOCH},
};

use codex_mux::install::{
    BEGIN_MARKER, DiscoveryContext, END_MARKER, ExecutablePaths, NoRunningServer, ServerEvidence,
    TmuxReloader, discover_config, install, install_with_options, status, uninstall,
};

fn scratch(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("codex-mux-installer-{name}-{nonce}"));
    fs::create_dir_all(&root).unwrap();
    root
}

fn executables() -> ExecutablePaths {
    ExecutablePaths::new(
        PathBuf::from("/opt/codex mux/bin/codex-mux"),
        PathBuf::from("/custom/codex's/bin/codex"),
    )
    .unwrap()
}

#[derive(Default)]
struct Reloader {
    running: bool,
    calls: Vec<PathBuf>,
    unbound: Vec<String>,
    root_bound: bool,
    root_unbound: usize,
    root_expected: Vec<PathBuf>,
    failure: Option<String>,
    root_failure: Option<String>,
    unbind_failure: Option<String>,
}

impl TmuxReloader for Reloader {
    fn is_running(&self) -> bool {
        self.running
    }

    fn unbind(&mut self, key: &str) -> Result<(), String> {
        self.unbound.push(key.to_owned());
        self.unbind_failure.clone().map_or(Ok(()), Err)
    }

    fn root_left_bound(&mut self) -> Result<bool, String> {
        Ok(self.root_bound)
    }

    fn unbind_root_left(&mut self, expected_mux: &Path) -> Result<(), String> {
        self.root_unbound += 1;
        self.root_expected.push(expected_mux.to_owned());
        if let Some(error) = &self.root_failure {
            return Err(error.clone());
        }
        self.root_bound = false;
        Ok(())
    }

    fn reload(&mut self, path: &Path) -> Result<(), String> {
        self.calls.push(path.to_owned());
        self.failure.clone().map_or(Ok(()), Err)
    }
}

#[test]
fn install_is_idempotent_and_uninstall_restores_original_bytes() {
    let root = scratch("round-trip");
    let config = root.join("tmux.conf");
    let original = b"set -g mouse on\n# host bytes stay exact\n";
    fs::write(&config, original).unwrap();
    let mut no_server = NoRunningServer;

    let first = install(&config, "a", &executables(), &mut no_server).unwrap();
    assert!(first.changed);
    assert_eq!(fs::read(first.backup.unwrap()).unwrap(), original);
    let installed = fs::read(&config).unwrap();
    assert!(installed.starts_with(original));
    assert_eq!(
        installed
            .windows(BEGIN_MARKER.len())
            .filter(|w| *w == BEGIN_MARKER.as_bytes())
            .count(),
        1
    );

    let second = install(&config, "a", &executables(), &mut no_server).unwrap();
    assert!(!second.changed);
    assert_eq!(fs::read(&config).unwrap(), installed);

    assert!(uninstall(&config, &mut no_server).unwrap());
    assert_eq!(fs::read(&config).unwrap(), original);
    assert!(!uninstall(&config, &mut no_server).unwrap());
}

#[test]
fn install_preserves_a_missing_final_newline_across_uninstall() {
    let root = scratch("no-newline");
    let config = root.join(".tmux.conf");
    fs::write(&config, b"set -g status off").unwrap();
    let mut no_server = NoRunningServer;
    install(&config, "g", &executables(), &mut no_server).unwrap();
    uninstall(&config, &mut no_server).unwrap();
    assert_eq!(fs::read(&config).unwrap(), b"set -g status off");
}

#[test]
fn paths_are_quoted_and_status_reports_drift_without_writing() {
    let root = scratch("status");
    let config = root.join("tmux.conf");
    fs::write(&config, b"# host\n").unwrap();
    let mut no_server = NoRunningServer;
    install(&config, "C-a", &executables(), &mut no_server).unwrap();
    let before = fs::read(&config).unwrap();
    let rendered = String::from_utf8(before.clone()).unwrap();
    assert!(rendered.contains("'/opt/codex mux/bin/codex-mux'"));
    assert!(rendered.contains("/custom/codex"));
    assert!(rendered.contains("s/bin/codex"));

    let expected =
        ExecutablePaths::new(PathBuf::from("/new/codex-mux"), PathBuf::from("/new/codex")).unwrap();
    let report = status(&config, &expected).unwrap();
    assert!(report.installed);
    assert_eq!(report.key.as_deref(), Some("C-a"));
    assert_eq!(report.drift.len(), 2);
    assert!(!report.smart_left);
    assert_eq!(fs::read(&config).unwrap(), before);
}

#[test]
fn smart_left_is_opt_in_rendered_and_reported() {
    let root = scratch("smart-left-render");
    let config = root.join("tmux.conf");
    fs::write(&config, b"set -g status off\n").unwrap();
    let mut no_server = NoRunningServer;

    install_with_options(&config, "a", true, &executables(), &mut no_server).unwrap();
    let rendered = String::from_utf8(fs::read(&config).unwrap()).unwrap();
    assert!(rendered.contains("# codex-mux-smart-left: true"));
    assert!(rendered.contains("bind-key -T root Left"));
    assert!(rendered.contains("smart-left"));
    assert!(rendered.contains("@codex_mux_smart_left_active"));
    assert!(status(&config, &executables()).unwrap().smart_left);
}

#[test]
fn smart_left_refuses_file_and_live_root_binding_conflicts_before_mutation() {
    for host_binding in [
        "bind-key -n Left select-pane -L\n",
        "bind -T root Left resize-pane -L\n",
        "bind-key -N \"host left\" -T root Left select-pane -L\n",
    ] {
        let root = scratch("smart-left-file-conflict");
        let config = root.join("tmux.conf");
        fs::write(&config, host_binding.as_bytes()).unwrap();
        let before = fs::read(&config).unwrap();
        let mut no_server = NoRunningServer;
        assert!(install_with_options(&config, "a", true, &executables(), &mut no_server).is_err());
        assert_eq!(fs::read(&config).unwrap(), before);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
    }

    let root = scratch("smart-left-live-conflict");
    let config = root.join("tmux.conf");
    fs::write(&config, b"host\n").unwrap();
    let mut reloader = Reloader {
        running: true,
        root_bound: true,
        ..Reloader::default()
    };
    assert!(install_with_options(&config, "a", true, &executables(), &mut reloader).is_err());
    assert_eq!(fs::read(&config).unwrap(), b"host\n");
    assert!(reloader.calls.is_empty());
}

#[test]
fn smart_left_allows_unambiguous_non_left_bindings() {
    let root = scratch("smart-left-literal-dash");
    let config = root.join("tmux.conf");
    fs::write(
        &config,
        b"bind - split-window -v\nbind W run-shell 'capture-pane' \\\ndisplay 'captured'\n",
    )
    .unwrap();
    let mut no_server = NoRunningServer;

    install_with_options(&config, "a", true, &executables(), &mut no_server).unwrap();

    let rendered = String::from_utf8(fs::read(&config).unwrap()).unwrap();
    assert!(rendered.starts_with("bind - split-window -v\nbind W run-shell"));
    assert!(status(&config, &executables()).unwrap().smart_left);
}

#[test]
fn smart_left_rejects_an_incomplete_continued_binding() {
    for (name, contents) in [
        (
            "before-key",
            b"bind \\\n-T root Left select-pane -L\n".as_slice(),
        ),
        ("at-eof", b"bind W run-shell 'capture-pane' \\".as_slice()),
    ] {
        let root = scratch(&format!("smart-left-incomplete-continuation-{name}"));
        let config = root.join("tmux.conf");
        fs::write(&config, contents).unwrap();
        let before = fs::read(&config).unwrap();
        let mut no_server = NoRunningServer;

        assert!(install_with_options(&config, "a", true, &executables(), &mut no_server).is_err());
        assert_eq!(fs::read(&config).unwrap(), before);
    }
}

#[test]
fn live_unbind_failures_do_not_leave_a_partially_removed_binding_pair() {
    for operation in ["update", "uninstall"] {
        let root = scratch(&format!("smart-left-root-unbind-failure-{operation}"));
        let config = root.join("tmux.conf");
        fs::write(&config, b"host\n").unwrap();
        let mut reloader = Reloader {
            running: true,
            ..Reloader::default()
        };
        install_with_options(&config, "a", true, &executables(), &mut reloader).unwrap();
        let installed = fs::read(&config).unwrap();
        reloader.calls.clear();
        reloader.unbound.clear();
        reloader.root_failure = Some("root unbind exploded".to_owned());

        let result = if operation == "update" {
            install_with_options(&config, "g", false, &executables(), &mut reloader).map(|_| ())
        } else {
            uninstall(&config, &mut reloader).map(|_| ())
        };
        assert!(result.is_err());
        assert_eq!(fs::read(&config).unwrap(), installed);
        assert!(reloader.unbound.is_empty());
        assert!(reloader.calls.is_empty());
    }

    for operation in ["update", "uninstall"] {
        let root = scratch(&format!("smart-left-prefix-unbind-failure-{operation}"));
        let config = root.join("tmux.conf");
        fs::write(&config, b"host\n").unwrap();
        let mut reloader = Reloader {
            running: true,
            ..Reloader::default()
        };
        install_with_options(&config, "a", true, &executables(), &mut reloader).unwrap();
        let installed = fs::read(&config).unwrap();
        reloader.calls.clear();
        reloader.unbound.clear();
        reloader.unbind_failure = Some("prefix unbind exploded".to_owned());

        let result = if operation == "update" {
            install_with_options(&config, "g", false, &executables(), &mut reloader).map(|_| ())
        } else {
            uninstall(&config, &mut reloader).map(|_| ())
        };
        assert!(result.is_err());
        assert_eq!(fs::read(&config).unwrap(), installed);
        assert_eq!(reloader.unbound, vec!["a"]);
        assert_eq!(reloader.calls, vec![config]);
    }
}

#[test]
fn disabling_and_uninstalling_smart_left_remove_only_owned_live_root_binding() {
    let root = scratch("smart-left-disable");
    let config = root.join("tmux.conf");
    fs::write(&config, b"host\n").unwrap();
    let mut reloader = Reloader {
        running: true,
        ..Reloader::default()
    };
    install_with_options(&config, "a", true, &executables(), &mut reloader).unwrap();
    reloader.root_bound = true;
    install_with_options(&config, "a", false, &executables(), &mut reloader).unwrap();
    assert_eq!(reloader.root_unbound, 1);
    assert!(!status(&config, &executables()).unwrap().smart_left);

    install_with_options(&config, "a", true, &executables(), &mut reloader).unwrap();
    reloader.root_bound = true;
    assert!(uninstall(&config, &mut reloader).unwrap());
    assert_eq!(reloader.root_unbound, 2);
}

#[test]
fn disabling_after_a_binary_move_authenticates_the_recorded_old_binding() {
    let root = scratch("smart-left-moved-binary");
    let config = root.join("tmux.conf");
    fs::write(&config, b"host\n").unwrap();
    let old = ExecutablePaths::new(
        PathBuf::from("/old/codex-mux"),
        PathBuf::from("/custom/codex"),
    )
    .unwrap();
    let new = ExecutablePaths::new(
        PathBuf::from("/new/codex-mux"),
        PathBuf::from("/custom/codex"),
    )
    .unwrap();
    let mut reloader = Reloader {
        running: true,
        ..Reloader::default()
    };

    install_with_options(&config, "a", true, &old, &mut reloader).unwrap();
    install_with_options(&config, "a", false, &new, &mut reloader).unwrap();

    assert_eq!(
        reloader.root_expected,
        vec![PathBuf::from("/old/codex-mux")]
    );
    let report = status(&config, &new).unwrap();
    assert!(!report.smart_left);
    assert!(report.drift.is_empty());
}

#[test]
fn legacy_block_without_smart_left_metadata_migrates_as_disabled() {
    let root = scratch("smart-left-legacy");
    let config = root.join("tmux.conf");
    let legacy = format!(
        "{BEGIN_MARKER}\n# codex-mux-owned-leading-newline: false\n# codex-mux-key: a\n# codex-mux-binary: /opt/codex mux/bin/codex-mux\n# codex-executable: /custom/codex's/bin/codex\nbind-key a display-message legacy\n{END_MARKER}\n"
    );
    fs::write(&config, legacy).unwrap();
    assert!(!status(&config, &executables()).unwrap().smart_left);
    let mut no_server = NoRunningServer;
    install(&config, "a", &executables(), &mut no_server).unwrap();
    assert!(
        String::from_utf8(fs::read(&config).unwrap())
            .unwrap()
            .contains("# codex-mux-smart-left: false")
    );
}

#[test]
fn malformed_duplicate_partial_and_nested_markers_are_refused_without_mutation() {
    let cases = [
        format!("{BEGIN_MARKER}\n"),
        format!("{END_MARKER}\n"),
        format!("{BEGIN_MARKER}\n{BEGIN_MARKER}\n{END_MARKER}\n"),
        format!("{BEGIN_MARKER}\n{END_MARKER}\n{END_MARKER}\n"),
        format!("{END_MARKER}\n{BEGIN_MARKER}\n"),
    ];
    for (index, contents) in cases.into_iter().enumerate() {
        let root = scratch(&format!("markers-{index}"));
        let config = root.join("tmux.conf");
        fs::write(&config, contents.as_bytes()).unwrap();
        let before = fs::read(&config).unwrap();
        let mut no_server = NoRunningServer;
        assert!(install(&config, "a", &executables(), &mut no_server).is_err());
        assert_eq!(fs::read(&config).unwrap(), before);
    }
}

#[test]
fn discovery_covers_explicit_running_custom_and_standard_branches() {
    let root = scratch("discovery");
    let home = root.join("home");
    fs::create_dir_all(home.join(".config/tmux")).unwrap();
    let custom = home.join("custom.conf");
    fs::write(&custom, b"").unwrap();
    let explicit = DiscoveryContext {
        explicit: Some(custom.clone()),
        server: ServerEvidence::NotRunning,
        home: home.clone(),
        xdg_config_home: None,
    };
    assert_eq!(discover_config(&explicit).unwrap(), custom);

    let system = root.join("etc/tmux.conf");
    fs::create_dir_all(system.parent().unwrap()).unwrap();
    fs::write(&system, b"").unwrap();
    let running = DiscoveryContext {
        explicit: None,
        server: ServerEvidence::Running(vec![system, custom.clone(), custom.clone()]),
        home: home.clone(),
        xdg_config_home: None,
    };
    assert_eq!(discover_config(&running).unwrap(), custom);

    fs::remove_file(&custom).unwrap();
    let standard = home.join(".tmux.conf");
    fs::write(&standard, b"").unwrap();
    let stopped = DiscoveryContext {
        explicit: None,
        server: ServerEvidence::NotRunning,
        home: home.clone(),
        xdg_config_home: None,
    };
    assert_eq!(discover_config(&stopped).unwrap(), standard);
    let xdg = home.join(".config/tmux/tmux.conf");
    fs::write(&xdg, b"").unwrap();
    assert!(discover_config(&stopped).is_err());
}

#[test]
fn unsafe_paths_and_invalid_keys_are_refused() {
    let root = scratch("unsafe");
    let real = root.join("real.conf");
    let link = root.join("link.conf");
    fs::write(&real, b"host\n").unwrap();
    symlink(&real, &link).unwrap();
    let context = DiscoveryContext {
        explicit: Some(link),
        server: ServerEvidence::NotRunning,
        home: root.clone(),
        xdg_config_home: None,
    };
    assert!(discover_config(&context).is_err());

    let before = fs::read(&real).unwrap();
    let mut no_server = NoRunningServer;
    assert!(install(&real, "a; kill-server", &executables(), &mut no_server).is_err());
    assert_eq!(fs::read(&real).unwrap(), before);

    let mut permissions = fs::metadata(&real).unwrap().permissions();
    permissions.set_mode(0o400);
    fs::set_permissions(&real, permissions).unwrap();
    assert!(status(&real, &executables()).is_err());
}

#[test]
fn line_breaks_in_executable_metadata_are_refused_without_reload() {
    let root = scratch("metadata-injection");
    let config = root.join("tmux.conf");
    fs::write(&config, b"host\n").unwrap();
    let malicious = ExecutablePaths::new(
        PathBuf::from("/opt/codex-mux\nrun-shell 'touch /tmp/pwned'"),
        PathBuf::from("/opt/codex"),
    );
    assert!(malicious.is_err());
    for mux in ["/opt/mux#(touch format-pwned)", "/opt/mux$INJECTED_SUFFIX"] {
        assert!(ExecutablePaths::new(PathBuf::from(mux), PathBuf::from("/opt/codex")).is_err());
    }
    assert_eq!(fs::read(&config).unwrap(), b"host\n");
}

#[test]
fn successful_write_reloads_once_and_reload_failure_is_explicitly_recoverable() {
    let root = scratch("reload");
    let config = root.join("tmux.conf");
    fs::write(&config, b"host\n").unwrap();
    let mut reload = Reloader {
        running: true,
        ..Reloader::default()
    };
    let outcome = install(&config, "a", &executables(), &mut reload).unwrap();
    assert!(outcome.reloaded);
    assert_eq!(reload.calls, vec![config.clone()]);
    assert!(reload.unbound.is_empty());

    let mut failure = Reloader {
        running: true,
        failure: Some("source-file rejected line 3".to_owned()),
        ..Reloader::default()
    };
    let error = install(&config, "b", &executables(), &mut failure).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("could not synchronize running tmux")
    );
    assert_eq!(failure.unbound, vec!["a"]);
    assert!(
        String::from_utf8(fs::read(&config).unwrap())
            .unwrap()
            .contains("codex-mux-key: b")
    );
}

#[test]
fn key_change_and_uninstall_remove_only_the_previous_live_binding() {
    let root = scratch("live-key-cleanup");
    let config = root.join("tmux.conf");
    fs::write(&config, b"host\n").unwrap();
    let mut reload = Reloader {
        running: true,
        ..Reloader::default()
    };
    install(&config, "a", &executables(), &mut reload).unwrap();
    install(&config, "g", &executables(), &mut reload).unwrap();
    assert_eq!(reload.unbound, vec!["a"]);
    assert!(uninstall(&config, &mut reload).unwrap());
    assert_eq!(reload.unbound, vec!["a", "g"]);
    assert_eq!(reload.calls, vec![config.clone(), config.clone(), config]);
}

#[test]
fn retry_after_reload_failure_resynchronizes_an_unchanged_file() {
    let root = scratch("reload-retry");
    let config = root.join("tmux.conf");
    fs::write(&config, b"host\n").unwrap();
    let mut reload = Reloader {
        running: true,
        failure: Some("temporary source failure".to_owned()),
        ..Reloader::default()
    };
    assert!(install(&config, "a", &executables(), &mut reload).is_err());
    reload.failure = None;
    let retry = install(&config, "a", &executables(), &mut reload).unwrap();
    assert!(!retry.changed);
    assert!(retry.reloaded);
    assert_eq!(reload.calls, vec![config.clone(), config]);
}

#[test]
fn rendered_binding_loads_in_a_disposable_tmux_server() {
    let root = scratch("tmux-parse");
    let config = root.join("tmux.conf");
    fs::write(&config, b"set -g status off\n").unwrap();
    let mut no_server = NoRunningServer;
    install_with_options(&config, "a", true, &executables(), &mut no_server).unwrap();

    let socket = format!(
        "codex-mux-installer-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let started = Command::new("tmux")
        .args(["-L", &socket, "-f"])
        .arg(&config)
        .args(["new-session", "-d"])
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "tmux rejected generated config: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let listed = Command::new("tmux")
        .args(["-L", &socket, "list-keys", "-T", "prefix", "a"])
        .output()
        .unwrap();
    let root_left = Command::new("tmux")
        .args(["-L", &socket, "list-keys", "-T", "root", "Left"])
        .output()
        .unwrap();
    let _ = Command::new("tmux")
        .args(["-L", &socket, "kill-server"])
        .status();
    assert!(listed.status.success());
    assert!(root_left.status.success());
    let binding = String::from_utf8(listed.stdout).unwrap();
    assert!(binding.contains("display-popup"));
    assert!(binding.contains("codex-mux"));
    assert!(binding.contains("client_width"));
    assert!(binding.contains("q:pane_current_path"));
    let smart_binding = String::from_utf8(root_left.stdout).unwrap();
    assert!(smart_binding.contains("smart-left"));
    assert!(smart_binding.contains("send-keys"));
}

#[test]
fn adversarial_cwd_cannot_inject_when_the_real_binding_is_pressed() {
    let root = scratch("binding-injection");
    let malicious_cwd = root.join("project'; touch pwned; echo '");
    fs::create_dir(&malicious_cwd).unwrap();
    let mux = root.join("mux executable'; semicolon");
    let codex = root.join("codex executable");
    let invoked = root.join("binding-invoked");
    fs::write(
        &mux,
        format!("#!/bin/sh\n: > '{}'\n", invoked.display()).as_bytes(),
    )
    .unwrap();
    let mut mux_permissions = fs::metadata(&mux).unwrap().permissions();
    mux_permissions.set_mode(0o700);
    fs::set_permissions(&mux, mux_permissions).unwrap();
    symlink("/bin/true", &codex).unwrap();
    let paths = ExecutablePaths::new(mux, codex).unwrap();
    let config = root.join("tmux.conf");
    fs::write(&config, b"set -g status off\n").unwrap();
    let mut no_server = NoRunningServer;
    install(&config, "a", &paths, &mut no_server).unwrap();

    let socket = format!(
        "codex-mux-binding-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let started = Command::new("tmux")
        .args(["-L", &socket, "-f"])
        .arg(&config)
        .args(["new-session", "-d", "-c"])
        .arg(&malicious_cwd)
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "tmux rejected generated config: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    let attach_command = format!("tmux -L {socket} attach-session");
    let mut client = Command::new("script")
        .args(["-qfec", &attach_command, "/dev/null"])
        .env_remove("TMUX")
        .env("TERM", "xterm-256color")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut attached_client = None;
    for _ in 0..40 {
        let listed = Command::new("tmux")
            .args(["-L", &socket, "list-clients", "-F", "#{client_tty}"])
            .output()
            .unwrap();
        if listed.status.success() && !listed.stdout.is_empty() {
            attached_client = Some(String::from_utf8(listed.stdout).unwrap().trim().to_owned());
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let attached_client = attached_client.expect("disposable tmux client did not attach");
    let prefix_table = Command::new("tmux")
        .args([
            "-L",
            &socket,
            "switch-client",
            "-c",
            &attached_client,
            "-T",
            "prefix",
        ])
        .output()
        .unwrap();
    assert!(prefix_table.status.success());
    let input = client.stdin.as_mut().unwrap();
    input.write_all(b"a").unwrap();
    input.flush().unwrap();
    for _ in 0..40 {
        if invoked.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    input.write_all(b"\x02d").unwrap();
    input.flush().unwrap();
    drop(client.stdin.take());
    for _ in 0..20 {
        if client.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if client.try_wait().unwrap().is_none() {
        client.kill().unwrap();
        let _ = client.wait();
    }
    let _ = Command::new("tmux")
        .args(["-L", &socket, "kill-server"])
        .status();

    assert!(
        !malicious_cwd.join("pwned").exists(),
        "generated popup command executed cwd text as shell syntax"
    );
    assert!(
        invoked.exists(),
        "the generated binding did not invoke codex-mux"
    );
}
