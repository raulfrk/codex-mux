mod installer_e2e_support;

use std::{
    ffi::{OsStr, OsString},
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
};

use installer_e2e_support::{
    RealTmux, Scratch, assert_failure, assert_success, backups_below, fake_codex, packaged_binary,
    run_packaged, write_fake_tmux,
};

const BEGIN: &str = "# >>> codex-mux >>>";

#[test]
fn packaged_cli_round_trips_real_tmux_and_preserves_host_bytes() {
    let Some(binary) = packaged_binary() else {
        return;
    };
    let scratch = Scratch::new("real-round-trip");
    let config = scratch.join("tmux.conf");
    let original = b"set -g status off\n# exact host bytes without final newline";
    fs::write(&config, original).unwrap();
    let codex = fake_codex(&scratch, "custom codex");
    let alternate_codex = fake_codex(&scratch, "alternate-codex");
    let Some(server) = RealTmux::start(&scratch, &config) else {
        return;
    };
    let environment = server.environment();

    let install = invoke(
        &binary,
        &scratch,
        server.path(),
        None,
        &environment,
        &[
            "--codex".into(),
            codex.as_os_str().into(),
            "tmux".into(),
            "install".into(),
            "--key".into(),
            "C-g".into(),
            "--config".into(),
            "tmux.conf".into(),
        ],
    );
    assert_success(
        &install,
        "install packaged binding with bare-relative config",
    );
    let stdout = String::from_utf8_lossy(&install.stdout);
    assert!(stdout.contains("installed codex-mux binding"), "{stdout}");
    assert!(stdout.contains("reloaded running tmux server"), "{stdout}");

    let backup = config.with_extension("codex-mux.bak");
    assert_eq!(fs::read(&backup).unwrap(), original);
    let installed = fs::read(&config).unwrap();
    assert!(installed.starts_with(original));
    assert_eq!(count(&installed, BEGIN.as_bytes()), 1);
    let live = server.run(&["list-keys", "-T", "prefix", "C-g"]);
    assert_success(&live, "inspect installed live binding");
    assert!(String::from_utf8_lossy(&live.stdout).contains("display-popup"));

    let status = invoke_status(&binary, &scratch, server.path(), &environment, &codex);
    assert_success(&status, "status after packaged install");
    let status_text = String::from_utf8_lossy(&status.stdout);
    assert!(status_text.contains("key: C-g"), "{status_text}");
    assert!(
        status_text.contains("codex-thread-id-title: not installed"),
        "{status_text}"
    );
    assert!(status_text.contains("drift: none"), "{status_text}");
    assert_eq!(fs::read(&config).unwrap(), installed);

    let reinstall = invoke(
        &binary,
        &scratch,
        server.path(),
        None,
        &environment,
        &[
            "--codex".into(),
            codex.as_os_str().into(),
            "tmux".into(),
            "install".into(),
            "--key".into(),
            "C-g".into(),
            "--config".into(),
            "tmux.conf".into(),
        ],
    );
    assert_success(&reinstall, "idempotent packaged reinstall");
    assert!(
        String::from_utf8_lossy(&reinstall.stdout).contains("already current"),
        "{}",
        String::from_utf8_lossy(&reinstall.stdout)
    );
    assert_eq!(fs::read(&config).unwrap(), installed);
    assert_eq!(backups_below(scratch.path()), vec![backup.clone()]);

    let key_change = invoke(
        &binary,
        &scratch,
        server.path(),
        None,
        &environment,
        &[
            "--codex".into(),
            codex.as_os_str().into(),
            "tmux".into(),
            "install".into(),
            "--key".into(),
            "g".into(),
            "--config".into(),
            "tmux.conf".into(),
        ],
    );
    assert_success(&key_change, "change packaged live binding key");
    assert!(
        !server
            .run(&["list-keys", "-T", "prefix", "C-g"])
            .status
            .success()
    );
    assert_success(
        &server.run(&["list-keys", "-T", "prefix", "g"]),
        "inspect changed live binding",
    );

    let before_drift = fs::read(&config).unwrap();
    let drift = invoke_status(
        &binary,
        &scratch,
        server.path(),
        &environment,
        &alternate_codex,
    );
    assert_success(&drift, "report alternate-Codex drift");
    let drift_text = String::from_utf8_lossy(&drift.stdout);
    assert!(drift_text.contains("drift: Codex path:"), "{drift_text}");
    assert_eq!(fs::read(&config).unwrap(), before_drift);

    let uninstall = invoke(
        &binary,
        &scratch,
        server.path(),
        None,
        &environment,
        &[
            "tmux".into(),
            "uninstall".into(),
            "--config".into(),
            "tmux.conf".into(),
        ],
    );
    assert_success(&uninstall, "uninstall packaged binding");
    assert_eq!(fs::read(&config).unwrap(), original);
    assert!(
        !server
            .run(&["list-keys", "-T", "prefix", "g"])
            .status
            .success()
    );

    let no_op = invoke(
        &binary,
        &scratch,
        server.path(),
        None,
        &environment,
        &[
            "tmux".into(),
            "uninstall".into(),
            "--config".into(),
            "tmux.conf".into(),
        ],
    );
    assert_success(&no_op, "repeat packaged uninstall");
    assert!(
        String::from_utf8_lossy(&no_op.stdout).contains("was not installed"),
        "{}",
        String::from_utf8_lossy(&no_op.stdout)
    );
    assert_eq!(fs::read(&config).unwrap(), original);
    assert_eq!(fs::read(&backup).unwrap(), original);
}

#[test]
fn packaged_cli_refuses_unsafe_or_ambiguous_inputs_without_side_effects() {
    let Some(binary) = packaged_binary() else {
        return;
    };

    for case in failure_cases() {
        let scratch = Scratch::new(case.name);
        let home = scratch.join("home");
        let fake_path = scratch.join("bin");
        fs::create_dir_all(&home).unwrap();
        write_fake_tmux(&fake_path, case.tmux_script);
        let codex = fake_codex(&scratch, "codex");
        let mut tracked = Vec::new();
        let arguments = (case.setup)(&scratch, &home, &codex, &mut tracked);
        let before = tracked
            .iter()
            .map(|path| (path.clone(), fs::read(path).unwrap()))
            .collect::<Vec<_>>();

        let output = invoke(&binary, &scratch, &fake_path, Some(&home), &[], &arguments);
        assert_failure(&output, case.expected, case.name);
        for (path, bytes) in before {
            assert_eq!(
                fs::read(&path).unwrap(),
                bytes,
                "{} mutated {}",
                case.name,
                path.display()
            );
        }
        assert!(
            backups_below(scratch.path()).is_empty(),
            "{} created a backup on refusal",
            case.name
        );
    }
}

#[test]
fn packaged_cli_retries_an_unchanged_file_after_recoverable_reload_failure() {
    let Some(binary) = packaged_binary() else {
        return;
    };
    let scratch = Scratch::new("reload-retry");
    let config = scratch.join("tmux.conf");
    let original = b"set -g status off\n";
    fs::write(&config, original).unwrap();
    let codex = fake_codex(&scratch, "codex");
    let fake_path = scratch.join("bin");
    let state = scratch.join("tmux-state");
    fs::create_dir(&state).unwrap();
    write_fake_tmux(
        &fake_path,
        r#"#!/bin/sh
set -eu
: "${CODEX_MUX_FAKE_CONFIG:?}"
: "${CODEX_MUX_FAKE_STATE:?}"
printf '%s\n' "$*" >> "$CODEX_MUX_FAKE_STATE/calls"
case "${1-}" in
  display-message) printf '%s\n' "$CODEX_MUX_FAKE_CONFIG" ;;
  source-file)
    if [ ! -e "$CODEX_MUX_FAKE_STATE/failed-once" ]; then
      : > "$CODEX_MUX_FAKE_STATE/failed-once"
      printf 'deterministic source-file failure\n' >&2
      exit 23
    fi
    ;;
  unbind-key) ;;
  *) printf 'unexpected fake tmux arguments: %s\n' "$*" >&2; exit 97 ;;
esac
"#,
    );
    let environment = [
        ("CODEX_MUX_FAKE_CONFIG", config.as_os_str()),
        ("CODEX_MUX_FAKE_STATE", state.as_os_str()),
    ];
    let arguments = [
        OsString::from("--codex"),
        codex.as_os_str().into(),
        OsString::from("tmux"),
        OsString::from("install"),
        OsString::from("--key"),
        OsString::from("a"),
        OsString::from("--config"),
        config.as_os_str().into(),
    ];

    let first = invoke(
        &binary,
        &scratch,
        &fake_path,
        None,
        &environment,
        &arguments,
    );
    assert_failure(
        &first,
        "could not synchronize running tmux",
        "first packaged reload",
    );
    assert!(String::from_utf8_lossy(&first.stderr).contains("deterministic source-file failure"));
    let installed = fs::read(&config).unwrap();
    assert_ne!(installed, original);
    let backup = config.with_extension("codex-mux.bak");
    assert_eq!(fs::read(&backup).unwrap(), original);

    let retry = invoke(
        &binary,
        &scratch,
        &fake_path,
        None,
        &environment,
        &arguments,
    );
    assert_success(&retry, "retry unchanged packaged install");
    assert!(
        String::from_utf8_lossy(&retry.stdout).contains("already current"),
        "{}",
        String::from_utf8_lossy(&retry.stdout)
    );
    assert_eq!(fs::read(&config).unwrap(), installed);
    assert_eq!(backups_below(scratch.path()), vec![backup]);
    let calls = fs::read_to_string(state.join("calls")).unwrap();
    assert_eq!(calls.matches("source-file").count(), 2, "{calls}");
}

fn invoke_status(
    binary: &Path,
    scratch: &Scratch,
    path: &Path,
    environment: &[(&str, &OsStr)],
    codex: &Path,
) -> std::process::Output {
    invoke(
        binary,
        scratch,
        path,
        None,
        environment,
        &[
            "--codex".into(),
            codex.as_os_str().into(),
            "tmux".into(),
            "status".into(),
            "--config".into(),
            "tmux.conf".into(),
        ],
    )
}

fn invoke(
    binary: &Path,
    scratch: &Scratch,
    path: &Path,
    home: Option<&Path>,
    environment: &[(&str, &OsStr)],
    arguments: &[OsString],
) -> std::process::Output {
    run_packaged(binary, scratch.path(), path, home, environment, arguments)
}

struct FailureCase {
    name: &'static str,
    expected: &'static str,
    tmux_script: &'static str,
    setup: fn(&Scratch, &Path, &Path, &mut Vec<PathBuf>) -> Vec<OsString>,
}

fn failure_cases() -> Vec<FailureCase> {
    const NO_SERVER: &str = "#!/bin/sh\nprintf 'failed to connect to server: No such file or directory\\n' >&2\nexit 1\n";
    const SYSTEM_ONLY: &str = "#!/bin/sh\nprintf '/etc/tmux.conf\\n'\n";
    const INSPECTION_ERROR: &str =
        "#!/bin/sh\nprintf 'permission denied while inspecting server\\n' >&2\nexit 42\n";
    vec![
        FailureCase {
            name: "missing standard config",
            expected: "standard entrypoints were missing or ambiguous",
            tmux_script: NO_SERVER,
            setup: |_, _, codex, _| discovery_args(codex),
        },
        FailureCase {
            name: "ambiguous standard config",
            expected: "standard entrypoints were missing or ambiguous",
            tmux_script: NO_SERVER,
            setup: |_, home, codex, tracked| {
                let first = home.join(".tmux.conf");
                let second = home.join(".config/tmux/tmux.conf");
                fs::create_dir_all(second.parent().unwrap()).unwrap();
                fs::write(&first, b"first\n").unwrap();
                fs::write(&second, b"second\n").unwrap();
                tracked.extend([first, second]);
                discovery_args(codex)
            },
        },
        FailureCase {
            name: "running server reports only system config",
            expected: "did not report exactly one safe user configuration",
            tmux_script: SYSTEM_ONLY,
            setup: |_, home, codex, tracked| {
                let config = home.join(".tmux.conf");
                fs::write(&config, b"host\n").unwrap();
                tracked.push(config);
                discovery_args(codex)
            },
        },
        FailureCase {
            name: "partial begin marker",
            expected: "malformed codex-mux marker block",
            tmux_script: NO_SERVER,
            setup: setup_partial_begin,
        },
        FailureCase {
            name: "partial end marker",
            expected: "malformed codex-mux marker block",
            tmux_script: NO_SERVER,
            setup: setup_partial_end,
        },
        FailureCase {
            name: "duplicate markers",
            expected: "malformed codex-mux marker block",
            tmux_script: NO_SERVER,
            setup: setup_duplicate,
        },
        FailureCase {
            name: "nested markers",
            expected: "malformed codex-mux marker block",
            tmux_script: NO_SERVER,
            setup: setup_nested,
        },
        FailureCase {
            name: "symbolic-link config",
            expected: "symbolic links are refused",
            tmux_script: NO_SERVER,
            setup: |scratch, _, codex, tracked| {
                let target = scratch.join("real.conf");
                let link = scratch.join("tmux.conf");
                fs::write(&target, b"host\n").unwrap();
                symlink(&target, &link).unwrap();
                tracked.push(target);
                explicit_args(codex, &link, "a")
            },
        },
        FailureCase {
            name: "nonregular config",
            expected: "not a regular file",
            tmux_script: NO_SERVER,
            setup: |scratch, _, codex, _| {
                let config = scratch.join("tmux.conf");
                fs::create_dir(&config).unwrap();
                explicit_args(codex, &config, "a")
            },
        },
        FailureCase {
            name: "read-only config",
            expected: "owner write bit is not set",
            tmux_script: NO_SERVER,
            setup: |scratch, _, codex, tracked| {
                let config = scratch.join("tmux.conf");
                fs::write(&config, b"host\n").unwrap();
                fs::set_permissions(&config, fs::Permissions::from_mode(0o444)).unwrap();
                tracked.push(config.clone());
                explicit_args(codex, &config, "a")
            },
        },
        FailureCase {
            name: "invalid key",
            expected: "invalid binding key",
            tmux_script: NO_SERVER,
            setup: |scratch, _, codex, tracked| {
                let config = scratch.join("tmux.conf");
                fs::write(&config, b"host\n").unwrap();
                tracked.push(config.clone());
                explicit_args(codex, &config, "a;kill-server")
            },
        },
        FailureCase {
            name: "unexpected tmux inspection error",
            expected: "could not inspect tmux configuration files: permission denied while inspecting server",
            tmux_script: INSPECTION_ERROR,
            setup: |scratch, _, codex, tracked| {
                let config = scratch.join("tmux.conf");
                fs::write(&config, b"host\n").unwrap();
                tracked.push(config.clone());
                explicit_args(codex, &config, "a")
            },
        },
    ]
}

fn setup_marker(
    scratch: &Scratch,
    codex: &Path,
    tracked: &mut Vec<PathBuf>,
    bytes: &[u8],
) -> Vec<OsString> {
    let config = scratch.join("tmux.conf");
    fs::write(&config, bytes).unwrap();
    tracked.push(config.clone());
    explicit_args(codex, &config, "a")
}

fn setup_partial_begin(s: &Scratch, _: &Path, c: &Path, t: &mut Vec<PathBuf>) -> Vec<OsString> {
    setup_marker(s, c, t, b"# >>> codex-mux >>>\n")
}
fn setup_partial_end(s: &Scratch, _: &Path, c: &Path, t: &mut Vec<PathBuf>) -> Vec<OsString> {
    setup_marker(s, c, t, b"# <<< codex-mux <<<\n")
}
fn setup_duplicate(s: &Scratch, _: &Path, c: &Path, t: &mut Vec<PathBuf>) -> Vec<OsString> {
    setup_marker(
        s,
        c,
        t,
        b"# >>> codex-mux >>>\n# <<< codex-mux <<<\n# >>> codex-mux >>>\n# <<< codex-mux <<<\n",
    )
}
fn setup_nested(s: &Scratch, _: &Path, c: &Path, t: &mut Vec<PathBuf>) -> Vec<OsString> {
    setup_marker(
        s,
        c,
        t,
        b"# >>> codex-mux >>>\n# >>> codex-mux >>>\n# <<< codex-mux <<<\n# <<< codex-mux <<<\n",
    )
}

fn discovery_args(codex: &Path) -> Vec<OsString> {
    vec![
        "--codex".into(),
        codex.as_os_str().into(),
        "tmux".into(),
        "install".into(),
    ]
}

fn explicit_args(codex: &Path, config: &Path, key: &str) -> Vec<OsString> {
    vec![
        "--codex".into(),
        codex.as_os_str().into(),
        "tmux".into(),
        "install".into(),
        "--key".into(),
        key.into(),
        "--config".into(),
        config.as_os_str().into(),
    ]
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|part| *part == needle)
        .count()
}
