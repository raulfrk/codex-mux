use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn scratch() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "codex-mux-naming-daemon-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn worker(root: &Path, codex: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_codex-mux"))
        .args(["--codex", codex.to_str().unwrap(), "smart-naming-worker"])
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env(
            "TMUX",
            format!("{}/tmux.sock,{},0", root.display(), std::process::id()),
        )
        .env("PATH", format!("{}:/usr/bin:/bin", root.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().unwrap().is_some() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_file(path: &Path, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(value) = fs::read_to_string(path) {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_pid_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Path::new(&format!("/proc/{pid}")).exists() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
    true
}

fn wait_for_lock(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        if rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn daemon_lock(root: &Path) -> PathBuf {
    fs::read_dir(root.join("runtime"))
        .unwrap()
        .find_map(|entry| {
            let path = entry.unwrap().path();
            path.file_name()?
                .to_string_lossy()
                .starts_with("codex-mux-namer-")
                .then_some(path)
        })
        .expect("daemon lock missing")
}

#[test]
fn worker_is_singleton_and_survives_its_launcher_until_persisted_disable() {
    let root = scratch();
    fs::create_dir_all(root.join("runtime")).unwrap();
    fs::write(root.join("tmux.sock"), b"fixture").unwrap();
    let config = root.join("config/codex-mux/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "smart_naming = true\n").unwrap();
    let codex = root.join("codex");
    executable(
        &codex,
        "#!/bin/sh\nIFS= read -r request || exit 1\nprintf '%s\\n' '{\"id\":1,\"result\":{}}'\nIFS= read -r initialized || exit 1\nwhile IFS= read -r request; do :; done\n",
    );
    executable(&root.join("tmux"), "#!/bin/sh\nexit 0\n");

    let mut first = worker(&root, &codex);
    thread::sleep(Duration::from_millis(200));
    assert!(
        first.try_wait().unwrap().is_none(),
        "daemon exited with launcher scope"
    );

    let mut duplicate = worker(&root, &codex);
    assert!(
        wait_for_exit(&mut duplicate, Duration::from_secs(1)),
        "duplicate did not yield singleton lock"
    );
    assert!(first.try_wait().unwrap().is_none());

    fs::write(&config, "smart_naming = false\n").unwrap();
    assert!(
        wait_for_exit(&mut first, Duration::from_secs(2)),
        "daemon did not join after disable"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn tmux_owned_launcher_cleans_provider_on_disable_and_server_death() {
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }
    let root = scratch();
    fs::create_dir_all(root.join("runtime")).unwrap();
    let config = root.join("config/codex-mux/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "smart_naming = true\n").unwrap();
    let provider_pid = root.join("provider.pid");
    let codex = root.join("codex");
    executable(
        &codex,
        "#!/bin/sh\nprintf '%s' \"$$\" > \"$CODEX_MUX_TEST_PROVIDER_PID\"\nIFS= read -r request || exit 1\nprintf '%s\\n' '{\"id\":1,\"result\":{}}'\nIFS= read -r initialized || exit 1\nwhile IFS= read -r request; do :; done\n",
    );
    let socket = format!("codex-mux-naming-{}", std::process::id());
    let started = Command::new("tmux")
        .args([
            "-L",
            &socket,
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "naming",
            "sleep",
            "30",
        ])
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("CODEX_MUX_TEST_PROVIDER_PID", &provider_pid)
        .status()
        .unwrap();
    assert!(started.success());
    let tmux_value = Command::new("tmux")
        .args([
            "-L",
            &socket,
            "display-message",
            "-p",
            "#{socket_path},#{pid},0",
        ])
        .output()
        .unwrap();
    let tmux_value = String::from_utf8(tmux_value.stdout)
        .unwrap()
        .trim()
        .to_owned();
    let launch = || {
        Command::new(env!("CARGO_BIN_EXE_codex-mux"))
            .args(["--codex", codex.to_str().unwrap(), "smart-naming-start"])
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("TMUX", &tmux_value)
            .output()
            .unwrap()
    };

    assert!(launch().status.success());
    let first_pid = wait_for_file(&provider_pid, Duration::from_secs(3))
        .expect("tmux-owned daemon did not start provider")
        .parse::<u32>()
        .unwrap();
    let lock = daemon_lock(&root);
    assert!(Path::new(&format!("/proc/{first_pid}")).exists());
    fs::write(&config, "smart_naming = false\n").unwrap();
    assert!(wait_for_pid_gone(first_pid, Duration::from_secs(3)));
    assert!(
        wait_for_lock(&lock, Duration::from_secs(3)),
        "daemon did not release singleton lock"
    );

    fs::remove_file(&provider_pid).unwrap();
    fs::write(&config, "smart_naming = true\n").unwrap();
    assert!(launch().status.success());
    let second_pid = wait_for_file(&provider_pid, Duration::from_secs(3))
        .expect("daemon did not restart provider")
        .parse::<u32>()
        .unwrap();
    assert_ne!(first_pid, second_pid);
    let _ = Command::new("tmux")
        .args(["-L", &socket, "kill-server"])
        .status();
    assert!(wait_for_pid_gone(second_pid, Duration::from_secs(3)));
    assert!(
        wait_for_lock(&lock, Duration::from_secs(3)),
        "daemon did not release singleton lock after tmux server exit"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn disable_interrupts_late_provider_retry_backoff() {
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }
    let root = scratch();
    fs::create_dir_all(root.join("runtime")).unwrap();
    let config = root.join("config/codex-mux/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "smart_naming = true\n").unwrap();
    let attempts = root.join("attempts");
    let codex = root.join("codex");
    executable(
        &codex,
        "#!/bin/sh\nprintf x >> \"$CODEX_MUX_TEST_ATTEMPTS\"\nexit 1\n",
    );
    let socket = format!("codex-mux-naming-retry-{}", std::process::id());
    assert!(
        Command::new("tmux")
            .args([
                "-L",
                &socket,
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "naming",
                "sleep",
                "30"
            ])
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("CODEX_MUX_TEST_ATTEMPTS", &attempts)
            .status()
            .unwrap()
            .success()
    );
    let tmux_value = String::from_utf8(
        Command::new("tmux")
            .args([
                "-L",
                &socket,
                "display-message",
                "-p",
                "#{socket_path},#{pid},0",
            ])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let launched = Command::new(env!("CARGO_BIN_EXE_codex-mux"))
        .args(["--codex", codex.to_str().unwrap(), "smart-naming-start"])
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("TMUX", &tmux_value)
        .output()
        .unwrap();
    assert!(launched.status.success());
    let deadline = Instant::now() + Duration::from_secs(4);
    while fs::read(&attempts).map_or(0, |bytes| bytes.len()) < 4 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        fs::read(&attempts).unwrap().len() >= 4,
        "provider did not reach exponential backoff"
    );
    let lock = daemon_lock(&root);
    assert!(
        Command::new("tmux")
            .args([
                "-L",
                &socket,
                "set-option",
                "-p",
                "-t",
                "naming:0.0",
                "@codex_mux_generated_name",
                "cached title",
            ])
            .status()
            .unwrap()
            .success()
    );
    fs::write(&config, "smart_naming = false\n").unwrap();
    assert!(
        wait_for_lock(&lock, Duration::from_secs(2)),
        "retry backoff delayed shutdown acknowledgement"
    );
    assert!(
        !Command::new("tmux")
            .args([
                "-L",
                &socket,
                "show-options",
                "-pv",
                "-t",
                "naming:0.0",
                "@codex_mux_generated_name",
            ])
            .status()
            .unwrap()
            .success(),
        "disable left pane-local generated metadata behind"
    );
    let _ = Command::new("tmux")
        .args(["-L", &socket, "kill-server"])
        .status();
    fs::remove_dir_all(root).unwrap();
}
