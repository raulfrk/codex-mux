use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

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
    assert!(stdout.contains("setup"));
    assert!(stdout.contains("remove"));
}

fn scratch(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("codex-mux-cli-{name}-{nonce}"));
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn setup_and_remove_manage_all_three_marker_blocks() {
    let root = scratch("setup-remove");
    let tmux_tmp = root.join("tmux-tmp");
    fs::create_dir(&tmux_tmp).unwrap();
    let tmux = root.join("tmux.conf");
    let bash = root.join("bashrc");
    let zsh = root.join("zshrc");
    let codex = root.join("codex");
    fs::write(&tmux, b"set -g status off\n").unwrap();
    fs::write(&bash, b"host bash\n").unwrap();
    fs::write(&zsh, b"host zsh\n").unwrap();
    fs::write(&codex, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::create_dir(root.join(".codex")).unwrap();
    fs::write(
        root.join(".codex/config.toml"),
        "# user comment\n[tui]\nterminal_title = [\"thread-title\"]\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&codex).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&codex, permissions).unwrap();

    let setup = binary()
        .env("HOME", &root)
        .env("TMUX_TMPDIR", &tmux_tmp)
        .env_remove("TMUX")
        .arg("--codex")
        .arg(&codex)
        .arg("setup")
        .arg("--tmux-config")
        .arg(&tmux)
        .arg("--bash-config")
        .arg(&bash)
        .arg("--zsh-config")
        .arg(&zsh)
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "setup failed: {}",
        String::from_utf8_lossy(&setup.stderr)
    );
    assert!(
        fs::read_to_string(&tmux)
            .unwrap()
            .contains("codex-mux-smart-left: true")
    );
    assert!(
        fs::read_to_string(&bash)
            .unwrap()
            .contains("codex-mux bash")
    );
    assert!(fs::read_to_string(&zsh).unwrap().contains("codex-mux zsh"));
    let codex_config = fs::read_to_string(root.join(".codex/config.toml")).unwrap();
    assert!(codex_config.contains("# user comment"));
    assert!(codex_config.contains("terminal_title = [\"thread-id\"]"));
    assert!(
        root.join(".local/state/codex-mux/codex-terminal-title.toml")
            .exists()
    );

    let remove = binary()
        .env("HOME", &root)
        .env("TMUX_TMPDIR", &tmux_tmp)
        .env_remove("TMUX")
        .arg("remove")
        .arg("--tmux-config")
        .arg(&tmux)
        .arg("--bash-config")
        .arg(&bash)
        .arg("--zsh-config")
        .arg(&zsh)
        .output()
        .unwrap();
    assert!(
        remove.status.success(),
        "remove failed: {}",
        String::from_utf8_lossy(&remove.stderr)
    );
    assert_eq!(fs::read(&tmux).unwrap(), b"set -g status off\n");
    assert_eq!(fs::read(&bash).unwrap(), b"host bash\n");
    assert_eq!(fs::read(&zsh).unwrap(), b"host zsh\n");
    let codex_config = fs::read_to_string(root.join(".codex/config.toml")).unwrap();
    assert!(codex_config.contains("# user comment"));
    assert!(codex_config.contains("terminal_title = [\"thread-title\"]"));
    assert!(
        !root
            .join(".local/state/codex-mux/codex-terminal-title.toml")
            .exists()
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn explicit_process_configuration_is_embedded_and_reported() {
    let root = scratch("process-config");
    let tmux_tmp = root.join("tmux-tmp");
    fs::create_dir(&tmux_tmp).unwrap();
    let tmux = root.join("tmux.conf");
    let bash = root.join("bashrc");
    let zsh = root.join("zshrc");
    let launcher = root.join("codex-launcher");
    let underlying = root.join("real-codex");
    fs::write(&tmux, b"set -g status off\n").unwrap();
    fs::write(&bash, b"host bash\n").unwrap();
    fs::write(&zsh, b"host zsh\n").unwrap();
    for executable in [&launcher, &underlying] {
        fs::write(executable, b"#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(executable).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(executable, permissions).unwrap();
    }

    let install = binary()
        .env("HOME", &root)
        .env("TMUX_TMPDIR", &tmux_tmp)
        .env_remove("TMUX")
        .arg("--launch-executable")
        .arg(&launcher)
        .arg("--match-executable")
        .arg(&launcher)
        .arg("--match-executable")
        .arg(&underlying)
        .args(["--pane-command", "codex", "setup", "--tmux-config"])
        .arg(&tmux)
        .arg("--bash-config")
        .arg(&bash)
        .arg("--zsh-config")
        .arg(&zsh)
        .output()
        .unwrap();
    assert!(
        install.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );
    let rendered = fs::read_to_string(&tmux).unwrap();
    assert!(rendered.contains(&format!(
        "# codex-launch-executable: {}",
        launcher.display()
    )));
    assert!(rendered.contains(&format!(
        "# codex-match-executable: {}",
        underlying.display()
    )));
    assert!(rendered.contains("# codex-pane-command: codex"));
    assert!(rendered.contains("--launch-executable"));
    assert!(rendered.contains("--match-executable"));
    let saved = fs::read_to_string(root.join(".config/codex-mux/config.toml")).unwrap();
    assert!(saved.contains("[process]"));
    assert!(saved.contains(&format!("launch_executable = {:?}", launcher)));
    assert!(saved.contains("match_executables = ["));
    assert!(saved.contains(&format!("{:?}", launcher)));
    assert!(saved.contains(&format!("{:?}", underlying)));
    assert!(saved.contains("pane_commands = [\"codex\"]"));

    let status = binary()
        .env("HOME", &root)
        .env("TMUX_TMPDIR", &tmux_tmp)
        .env_remove("TMUX")
        .args(["tmux", "status", "--config"])
        .arg(&tmux)
        .output()
        .unwrap();
    assert!(status.status.success());
    let stdout = String::from_utf8(status.stdout).unwrap();
    assert!(stdout.contains(&format!("launch-executable: {}", launcher.display())));
    assert!(stdout.contains(&format!("match-executable: {}", underlying.display())));
    assert!(stdout.contains("pane-command: codex"));
    assert!(stdout.contains("drift: none"));

    fs::write(
        root.join(".codex/config.toml"),
        "[tui]\nterminal_title = [\"thread-title\"]\n",
    )
    .unwrap();
    let drifted = binary()
        .env("HOME", &root)
        .env("TMUX_TMPDIR", &tmux_tmp)
        .env_remove("TMUX")
        .args(["tmux", "status", "--config"])
        .arg(&tmux)
        .output()
        .unwrap();
    let stdout = String::from_utf8(drifted.stdout).unwrap();
    assert!(stdout.contains("codex-thread-id-title: drifted"));
    assert!(stdout.contains("drift: Codex terminal-title setting"));
    assert!(!stdout.contains("drift: none"));

    fs::write(&tmux, b"set -g status off\n").unwrap();
    let partial = binary()
        .env("HOME", &root)
        .env("TMUX_TMPDIR", &tmux_tmp)
        .env_remove("TMUX")
        .args(["tmux", "status", "--config"])
        .arg(&tmux)
        .output()
        .unwrap();
    let stdout = String::from_utf8(partial.stdout).unwrap();
    assert!(stdout.contains("not installed:"));
    assert!(stdout.contains("codex-thread-id-title: drifted"));
    assert!(stdout.contains("drift: Codex terminal-title setting"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn setup_rejects_a_scalar_tui_without_changing_host_files() {
    let root = scratch("scalar-tui");
    let tmux_tmp = root.join("tmux-tmp");
    fs::create_dir(&tmux_tmp).unwrap();
    fs::create_dir(root.join(".codex")).unwrap();
    let tmux = root.join("tmux.conf");
    let bash = root.join("bashrc");
    let zsh = root.join("zshrc");
    let codex = root.join("codex");
    fs::write(&tmux, b"host tmux\n").unwrap();
    fs::write(&bash, b"host bash\n").unwrap();
    fs::write(&zsh, b"host zsh\n").unwrap();
    fs::write(root.join(".codex/config.toml"), b"tui = \"custom\"\n").unwrap();
    fs::write(&codex, b"#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(&codex).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&codex, permissions).unwrap();

    let setup = binary()
        .env("HOME", &root)
        .env("TMUX_TMPDIR", &tmux_tmp)
        .env_remove("TMUX")
        .arg("--codex")
        .arg(&codex)
        .arg("setup")
        .arg("--tmux-config")
        .arg(&tmux)
        .arg("--bash-config")
        .arg(&bash)
        .arg("--zsh-config")
        .arg(&zsh)
        .output()
        .unwrap();
    assert!(!setup.status.success());
    assert!(String::from_utf8_lossy(&setup.stderr).contains("tui setting must be a table"));
    assert_eq!(fs::read(&tmux).unwrap(), b"host tmux\n");
    assert_eq!(fs::read(&bash).unwrap(), b"host bash\n");
    assert_eq!(fs::read(&zsh).unwrap(), b"host zsh\n");
    assert_eq!(
        fs::read(root.join(".codex/config.toml")).unwrap(),
        b"tui = \"custom\"\n"
    );
    assert!(
        !root
            .join(".local/state/codex-mux/codex-terminal-title.toml")
            .exists()
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn setup_rejects_a_symlinked_codex_config_without_changing_it() {
    let root = scratch("symlinked-codex-config");
    let tmux_tmp = root.join("tmux-tmp");
    fs::create_dir(&tmux_tmp).unwrap();
    fs::create_dir(root.join(".codex")).unwrap();
    let tmux = root.join("tmux.conf");
    let bash = root.join("bashrc");
    let zsh = root.join("zshrc");
    let codex = root.join("codex");
    let target = root.join("managed-config.toml");
    let config = root.join(".codex/config.toml");
    fs::write(&tmux, b"host tmux\n").unwrap();
    fs::write(&bash, b"host bash\n").unwrap();
    fs::write(&zsh, b"host zsh\n").unwrap();
    fs::write(&target, b"[tui]\nterminal_title = [\"thread-title\"]\n").unwrap();
    symlink(&target, &config).unwrap();
    fs::write(&codex, b"#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(&codex).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&codex, permissions).unwrap();

    let setup = binary()
        .env("HOME", &root)
        .env("TMUX_TMPDIR", &tmux_tmp)
        .env_remove("TMUX")
        .arg("--codex")
        .arg(&codex)
        .arg("setup")
        .arg("--tmux-config")
        .arg(&tmux)
        .arg("--bash-config")
        .arg(&bash)
        .arg("--zsh-config")
        .arg(&zsh)
        .output()
        .unwrap();
    assert!(!setup.status.success());
    assert!(String::from_utf8_lossy(&setup.stderr).contains("must not be a symlink"));
    assert!(
        fs::symlink_metadata(&config)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read(&target).unwrap(),
        b"[tui]\nterminal_title = [\"thread-title\"]\n"
    );
    assert_eq!(fs::read(&tmux).unwrap(), b"host tmux\n");
    assert_eq!(fs::read(&bash).unwrap(), b"host bash\n");
    assert_eq!(fs::read(&zsh).unwrap(), b"host zsh\n");
    assert!(!root.join(".config/codex-mux/config.toml").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn status_reports_managed_config_or_state_symlink_replacement_as_drift() {
    let root = scratch("status-symlink-drift");
    let tmux_tmp = root.join("tmux-tmp");
    fs::create_dir(&tmux_tmp).unwrap();
    fs::create_dir(root.join(".codex")).unwrap();
    let tmux = root.join("tmux.conf");
    let bash = root.join("bashrc");
    let zsh = root.join("zshrc");
    let codex = root.join("codex");
    fs::write(&tmux, b"host tmux\n").unwrap();
    fs::write(&bash, b"host bash\n").unwrap();
    fs::write(&zsh, b"host zsh\n").unwrap();
    fs::write(root.join(".codex/config.toml"), b"[tui]\n").unwrap();
    fs::write(&codex, b"#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(&codex).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&codex, permissions).unwrap();
    let setup = binary()
        .env("HOME", &root)
        .env("TMUX_TMPDIR", &tmux_tmp)
        .env_remove("TMUX")
        .arg("--codex")
        .arg(&codex)
        .args(["setup", "--tmux-config"])
        .arg(&tmux)
        .arg("--bash-config")
        .arg(&bash)
        .arg("--zsh-config")
        .arg(&zsh)
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );

    let status = || {
        binary()
            .env("HOME", &root)
            .env("TMUX_TMPDIR", &tmux_tmp)
            .env_remove("TMUX")
            .args(["tmux", "status", "--config"])
            .arg(&tmux)
            .output()
            .unwrap()
    };
    let config = root.join(".codex/config.toml");
    let config_target = root.join("managed-config-target.toml");
    fs::rename(&config, &config_target).unwrap();
    symlink(&config_target, &config).unwrap();
    let output = status();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("codex-thread-id-title: unreadable"),
        "{stdout}"
    );
    assert!(stdout.contains("drift: Codex terminal-title ownership is unreadable"));
    assert!(!stdout.contains("drift: none"));
    fs::remove_file(&config).unwrap();
    fs::rename(&config_target, &config).unwrap();

    let state = root.join(".local/state/codex-mux/codex-terminal-title.toml");
    let state_target = root.join("managed-state-target.toml");
    fs::rename(&state, &state_target).unwrap();
    symlink(&state_target, &state).unwrap();
    let output = status();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("codex-thread-id-title: unreadable"),
        "{stdout}"
    );
    assert!(stdout.contains("drift: Codex terminal-title ownership is unreadable"));
    assert!(!stdout.contains("drift: none"));
    assert!(
        fs::symlink_metadata(&state)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn zero_argument_setup_and_remove_use_safe_standard_defaults() {
    let root = scratch("zero-argument");
    let tmux_tmp = root.join("tmux-tmp");
    fs::create_dir(&tmux_tmp).unwrap();
    let tmux = root.join(".tmux.conf");
    let bash = root.join(".bashrc");
    let zsh = root.join(".zshrc");
    let codex = root.join("codex");
    fs::write(&tmux, b"set -g status off\n").unwrap();
    fs::write(&bash, b"host bash\n").unwrap();
    fs::write(&zsh, b"host zsh\n").unwrap();
    fs::write(&codex, b"#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(&codex).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&codex, permissions).unwrap();
    let tool_path = format!("{}:/usr/bin:/bin", root.display());

    let setup = binary()
        .env("HOME", &root)
        .env("PATH", &tool_path)
        .env("TMUX_TMPDIR", &tmux_tmp)
        .env_remove("TMUX")
        .env_remove("ZDOTDIR")
        .arg("setup")
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "zero-argument setup failed: {}",
        String::from_utf8_lossy(&setup.stderr)
    );
    assert!(fs::read_to_string(&tmux).unwrap().contains("codex-mux >>>"));
    assert!(
        fs::read_to_string(&bash)
            .unwrap()
            .contains("codex-mux bash")
    );
    assert!(fs::read_to_string(&zsh).unwrap().contains("codex-mux zsh"));

    let remove = binary()
        .env("HOME", &root)
        .env("PATH", &tool_path)
        .env("TMUX_TMPDIR", &tmux_tmp)
        .env_remove("TMUX")
        .env_remove("ZDOTDIR")
        .arg("remove")
        .output()
        .unwrap();
    assert!(
        remove.status.success(),
        "zero-argument remove failed: {}",
        String::from_utf8_lossy(&remove.stderr)
    );
    assert_eq!(fs::read(&tmux).unwrap(), b"set -g status off\n");
    assert_eq!(fs::read(&bash).unwrap(), b"host bash\n");
    assert_eq!(fs::read(&zsh).unwrap(), b"host zsh\n");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn setup_conflict_leaves_all_host_configuration_bytes_unchanged() {
    let root = scratch("setup-conflict");
    let tmux_tmp = root.join("tmux-tmp");
    fs::create_dir(&tmux_tmp).unwrap();
    let tmux = root.join("tmux.conf");
    let bash = root.join("bashrc");
    let zsh = root.join("zshrc");
    let codex = root.join("codex");
    let tmux_bytes = b"bind-key -T root Left select-pane -L\n";
    fs::write(&tmux, tmux_bytes).unwrap();
    fs::write(&bash, b"host bash\n").unwrap();
    fs::write(&zsh, b"host zsh\n").unwrap();
    fs::write(&codex, b"#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(&codex).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&codex, permissions).unwrap();

    let setup = binary()
        .env("HOME", &root)
        .env("TMUX_TMPDIR", &tmux_tmp)
        .env_remove("TMUX")
        .arg("--launch-executable")
        .arg(&codex)
        .arg("--match-executable")
        .arg(&codex)
        .args(["--pane-command", "codex"])
        .arg("setup")
        .arg("--tmux-config")
        .arg(&tmux)
        .arg("--bash-config")
        .arg(&bash)
        .arg("--zsh-config")
        .arg(&zsh)
        .output()
        .unwrap();
    assert!(!setup.status.success());
    assert_eq!(fs::read(&tmux).unwrap(), tmux_bytes);
    assert_eq!(fs::read(&bash).unwrap(), b"host bash\n");
    assert_eq!(fs::read(&zsh).unwrap(), b"host zsh\n");
    assert!(
        !root.join(".config/codex-mux/config.toml").exists(),
        "failed setup left persisted process configuration"
    );
    assert!(
        !root.join(".codex/config.toml").exists(),
        "failed setup left Codex terminal-title configuration"
    );
    assert!(
        !root
            .join(".local/state/codex-mux/codex-terminal-title.toml")
            .exists(),
        "failed setup left Codex terminal-title ownership state"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn setup_rejects_cross_role_exact_and_hard_link_aliases() {
    for hard_link in [false, true] {
        let root = scratch(if hard_link {
            "hard-link-alias"
        } else {
            "exact-alias"
        });
        let tmux_tmp = root.join("tmux-tmp");
        fs::create_dir(&tmux_tmp).unwrap();
        let tmux = root.join("tmux.conf");
        let bash = if hard_link {
            let bash = root.join("bashrc");
            fs::write(&tmux, b"set -g status off\n").unwrap();
            fs::hard_link(&tmux, &bash).unwrap();
            bash
        } else {
            fs::write(&tmux, b"set -g status off\n").unwrap();
            tmux.clone()
        };
        let zsh = root.join("zshrc");
        let codex = root.join("codex");
        fs::write(&zsh, b"host zsh\n").unwrap();
        fs::write(&codex, b"#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&codex).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&codex, permissions).unwrap();
        let before = fs::read(&tmux).unwrap();

        let setup = binary()
            .env("HOME", &root)
            .env("TMUX_TMPDIR", &tmux_tmp)
            .env_remove("TMUX")
            .arg("--codex")
            .arg(&codex)
            .arg("setup")
            .arg("--tmux-config")
            .arg(&tmux)
            .arg("--bash-config")
            .arg(&bash)
            .arg("--zsh-config")
            .arg(&zsh)
            .output()
            .unwrap();
        assert!(!setup.status.success());
        assert!(String::from_utf8_lossy(&setup.stderr).contains("must be distinct files"));
        assert_eq!(fs::read(&tmux).unwrap(), before);
        assert_eq!(fs::read(&bash).unwrap(), before);
        assert_eq!(fs::read(&zsh).unwrap(), b"host zsh\n");
        fs::remove_dir_all(root).unwrap();
    }
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
fn install_help_exposes_smart_left_without_exposing_internal_probe_command() {
    let install = binary()
        .args(["tmux", "install", "--help"])
        .output()
        .unwrap();
    assert!(install.status.success());
    assert!(
        String::from_utf8(install.stdout)
            .unwrap()
            .contains("--smart-left")
    );

    let root = binary().arg("--help").output().unwrap();
    assert!(root.status.success());
    assert!(
        !String::from_utf8(root.stdout)
            .unwrap()
            .contains("smart-left")
    );
}

#[test]
fn interactive_runtime_requires_explicit_tmux_context() {
    let output = binary().output().unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("invoking path"));
    assert!(stderr.contains("required when opening the interactive popup"));
}
