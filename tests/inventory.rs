use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use codex_mux::{
    MuxError, Result,
    config::MatchScope,
    domain::{CodexExecutable, CommandOutput, PaneProcess, ProcessInspector, TmuxCommandRunner},
    linux_process::LinuxProcessInspector,
    tmux::{inventory::PaneInventory, runner::SystemTmuxRunner, smart_left::DirectCodexInspector},
};

const SEPARATOR: u8 = 0x1f;

#[derive(Clone, Default)]
struct FakeRunner {
    output: Arc<Mutex<Option<CommandOutput>>>,
    arguments: Arc<Mutex<Vec<OsString>>>,
}

impl FakeRunner {
    fn returning(stdout: Vec<u8>) -> Self {
        Self {
            output: Arc::new(Mutex::new(Some(CommandOutput {
                stdout,
                stderr: Vec::new(),
                status: Some(0),
            }))),
            ..Self::default()
        }
    }
}

impl TmuxCommandRunner for FakeRunner {
    fn run(&self, arguments: &[OsString]) -> Result<CommandOutput> {
        *self.arguments.lock().unwrap() = arguments.to_vec();
        Ok(self.output.lock().unwrap().take().unwrap())
    }
}

#[derive(Default)]
struct FakeProcesses(HashMap<u32, ProcessAnswer>);

enum ProcessAnswer {
    Path(PathBuf),
    Missing,
    Inaccessible,
}

impl ProcessInspector for FakeProcesses {
    fn foreground_executable(&self, pane_pid: u32) -> Result<Option<PathBuf>> {
        match self.0.get(&pane_pid) {
            Some(ProcessAnswer::Path(path)) => Ok(Some(path.clone())),
            Some(ProcessAnswer::Inaccessible) => Err(MuxError::Filesystem {
                path: PathBuf::from(format!("/proc/{pane_pid}")),
                source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            }),
            Some(ProcessAnswer::Missing) | None => Ok(None),
        }
    }
}

fn fixture() -> Vec<u8> {
    include_bytes!("fixtures/inventory/server-wide.txt")
        .iter()
        .copied()
        .map(|byte| if byte == b'|' { SEPARATOR } else { byte })
        .collect::<Vec<_>>()
}

#[test]
fn server_wide_fixture_returns_exact_stable_targets_titles_and_paths() {
    let runner = FakeRunner::returning(fixture());
    let captured = runner.arguments.clone();
    let processes = FakeProcesses(HashMap::from([
        (101, ProcessAnswer::Path(PathBuf::from("/opt/bin/codex"))),
        (
            102,
            ProcessAnswer::Path(PathBuf::from("/opt/bin/renamed-agent")),
        ),
        (
            103,
            ProcessAnswer::Path(PathBuf::from("/tmp/unrelated/codex")),
        ),
    ]));
    let inventory = PaneInventory::new(
        runner,
        processes,
        CodexExecutable::new("/opt/bin/codex").unwrap(),
    );

    let panes = inventory.discover().unwrap();

    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].id.as_str(), "%1");
    assert_eq!(panes[0].session_id.as_str(), "$1");
    assert_eq!(panes[0].display_title(), "Implement parser");
    assert_eq!(panes[0].current_path, Path::new("/work/codex mux"));

    let arguments = captured.lock().unwrap();
    assert_eq!(&arguments[..2], ["list-panes", "-a"]);
    assert_eq!(arguments[2], "-F");
    let format = arguments[3].to_string_lossy();
    for field in [
        "pane_id",
        "session_id",
        "window_id",
        "window_name",
        "pane_title",
        "pane_current_path",
        "pane_current_command",
        "pane_pid",
        "pane_tty",
    ] {
        assert!(format.contains(field), "missing tmux field {field}");
    }
}

#[test]
fn custom_renamed_executable_and_unnamed_project_fallback_work() {
    let processes = FakeProcesses(HashMap::from([(
        102,
        ProcessAnswer::Path(PathBuf::from("/opt/bin/renamed-agent")),
    )]));
    let inventory = PaneInventory::new(
        FakeRunner::returning(fixture()),
        processes,
        CodexExecutable::new("/opt/bin/renamed-agent").unwrap(),
    );

    let panes = inventory.discover().unwrap();

    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].id.as_str(), "%2");
    assert_eq!(panes[0].display_title(), "über-project");
    assert_eq!(panes[0].current_path, Path::new("/srv/über-project"));
}

#[test]
fn valid_pane_local_name_is_the_visible_inventory_title() {
    let thread = "12345678-1234-1234-1234-123456789abc";
    let row = format!(
        "%1\x1f$1\x1f@1\x1ftmux-window\x1f{thread}\x1f/work/project\x1fcodex\x1f101\x1f/dev/pts/1\x1f{thread}\x1fSnappy naming\x1f1700000000\x1f1\n"
    );
    let processes = FakeProcesses(HashMap::from([(
        101,
        ProcessAnswer::Path(PathBuf::from("/opt/bin/codex")),
    )]));
    let inventory = PaneInventory::new(
        FakeRunner::returning(row.into_bytes()),
        processes,
        CodexExecutable::new("/opt/bin/codex").unwrap(),
    );

    let panes = inventory.discover().unwrap();

    assert_eq!(panes[0].title.as_deref(), Some(thread));
    assert_eq!(panes[0].generated_title.as_deref(), Some("Snappy naming"));
    assert_eq!(panes[0].generated_at_unix, Some(1_700_000_000));
    assert!(panes[0].immediate_naming);
    assert_eq!(panes[0].display_title(), "Snappy naming");
}

#[test]
fn malformed_or_too_short_title_prefix_cannot_reuse_generated_metadata() {
    let thread = "12345678-1234-1234-1234-123456789abc";
    for title in ["...", "12345678...", "12345678-123..."] {
        let row = format!(
            "%1\x1f$1\x1f@1\x1ftmux-window\x1f{title}\x1f/work/project\x1fcodex\x1f101\x1f/dev/pts/1\x1f{thread}\x1fStale name\x1f1700000000\n"
        );
        let inventory = PaneInventory::new(
            FakeRunner::returning(row.into_bytes()),
            FakeProcesses(HashMap::from([(
                101,
                ProcessAnswer::Path(PathBuf::from("/opt/bin/codex")),
            )])),
            CodexExecutable::new("/opt/bin/codex").unwrap(),
        );

        let panes = inventory.discover().unwrap();
        assert_eq!(panes[0].generated_title, None);
        assert_eq!(panes[0].generated_at_unix, None);
    }
}

#[test]
fn inventory_recognizes_configured_and_profile_executables_together() {
    let processes = FakeProcesses(HashMap::from([
        (101, ProcessAnswer::Path(PathBuf::from("/opt/bin/codex"))),
        (
            102,
            ProcessAnswer::Path(PathBuf::from("/opt/bin/renamed-agent")),
        ),
    ]));
    let inventory = PaneInventory::with_executables(
        FakeRunner::returning(fixture()),
        processes,
        vec![
            CodexExecutable::new("/opt/bin/codex").unwrap(),
            CodexExecutable::new("/opt/bin/renamed-agent").unwrap(),
        ],
    );

    let panes = inventory.discover().unwrap();

    assert_eq!(panes.len(), 2);
    assert_eq!(panes[0].id.as_str(), "%1");
    assert_eq!(panes[1].id.as_str(), "%2");
}

#[test]
fn basename_fallback_requires_canonical_file_identity() {
    let root = TemporaryDirectory::new("canonical-fallback");
    let real = root.path().join("real/codex");
    let configured = root.path().join("configured/codex");
    fs::create_dir_all(real.parent().unwrap()).unwrap();
    fs::create_dir_all(configured.parent().unwrap()).unwrap();
    fs::write(&real, b"fixture").unwrap();
    symlink(&real, &configured).unwrap();
    let row =
        b"%1\x1f$1\x1f@1\x1fmain\x1fSymlinked\x1f/work/project\x1fcodex\x1f101\x1f/dev/pts/1\x1f\x1f\n"
            .to_vec();
    let processes = FakeProcesses(HashMap::from([(101, ProcessAnswer::Path(real))]));
    let inventory = PaneInventory::new(
        FakeRunner::returning(row),
        processes,
        CodexExecutable::new(&configured).unwrap(),
    );

    let panes = inventory.discover().unwrap();

    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].id.as_str(), "%1");
}

#[test]
fn empty_dead_and_inaccessible_processes_degrade_without_panics() {
    let empty = PaneInventory::new(
        FakeRunner::returning(Vec::new()),
        FakeProcesses::default(),
        CodexExecutable::new("/opt/bin/codex").unwrap(),
    );
    assert!(empty.discover().unwrap().is_empty());

    let rows = b"%1\x1f$1\x1f@1\x1fmain\x1fone\x1f/work/one\x1fcodex\x1f101\x1f/dev/pts/1\x1f\x1f\n%2\x1f$1\x1f@1\x1fmain\x1ftwo\x1f/work/two\x1fcodex\x1f102\x1f/dev/pts/2\x1f\x1f\n".to_vec();
    let processes = FakeProcesses(HashMap::from([
        (101, ProcessAnswer::Missing),
        (102, ProcessAnswer::Inaccessible),
    ]));
    let inventory = PaneInventory::new(
        FakeRunner::returning(rows),
        processes,
        CodexExecutable::new("/opt/bin/codex").unwrap(),
    );
    assert!(inventory.discover().unwrap().is_empty());
}

#[test]
fn failed_tmux_command_is_an_inventory_error() {
    let runner = FakeRunner::default();
    *runner.output.lock().unwrap() = Some(CommandOutput {
        stdout: Vec::new(),
        stderr: b"no server running\n".to_vec(),
        status: Some(1),
    });
    let inventory = PaneInventory::new(
        runner,
        FakeProcesses::default(),
        CodexExecutable::new("/opt/bin/codex").unwrap(),
    );

    let message = inventory.discover().unwrap_err().to_string();
    assert!(message.contains("no server running"));
}

#[test]
fn proc_group_wrapper_requires_absolute_configured_path_evidence() {
    let root = TemporaryDirectory::new("proc-wrapper");
    let configured = root.path().join("bin/codex-custom");
    fs::create_dir_all(configured.parent().unwrap()).unwrap();
    fs::write(&configured, b"fixture").unwrap();
    write_process(root.path(), 10, 10, 34816, 20, "/bin/sh", &["/bin/sh"]);
    write_process(
        root.path(),
        20,
        20,
        34816,
        20,
        "/usr/bin/env",
        &["/usr/bin/env", configured.to_str().unwrap()],
    );
    let inspector = LinuxProcessInspector::with_proc_root(
        CodexExecutable::new(&configured).unwrap(),
        root.path(),
    );

    assert_eq!(
        inspector.foreground_executable(10).unwrap(),
        Some(configured)
    );
    assert!(inspector.foreground_process_matches(10).unwrap());
    assert!(!inspector.foreground_process_is_exact(10).unwrap());
}

#[test]
fn launcher_and_differently_located_underlying_binary_share_the_matcher() {
    let root = TemporaryDirectory::new("proc-launcher-underlying");
    let launcher = root.path().join("launch/codex-launcher");
    let underlying = root.path().join("runtime/codex");
    fs::create_dir_all(launcher.parent().unwrap()).unwrap();
    fs::create_dir_all(underlying.parent().unwrap()).unwrap();
    fs::write(&launcher, b"#!/bin/sh\n").unwrap();
    fs::write(&underlying, b"binary").unwrap();
    write_process(root.path(), 10, 10, 34816, 20, "/bin/sh", &["/bin/sh"]);
    write_process(
        root.path(),
        20,
        20,
        34816,
        20,
        underlying.to_str().unwrap(),
        &[underlying.to_str().unwrap()],
    );
    let launcher_executable = CodexExecutable::new(launcher).unwrap();
    let underlying_executable = CodexExecutable::new(&underlying).unwrap();
    let inspector = LinuxProcessInspector::with_proc_root_and_executables(
        launcher_executable.clone(),
        vec![launcher_executable, underlying_executable],
        root.path(),
    );

    assert_eq!(
        inspector.foreground_executable(10).unwrap(),
        Some(underlying)
    );
    assert!(inspector.foreground_process_matches(10).unwrap());
}

#[test]
fn proc_group_wrapper_recognizes_a_profile_specific_executable() {
    let root = TemporaryDirectory::new("proc-profile-wrapper");
    let primary = root.path().join("bin/codex");
    let profile = root.path().join("bin/codex-custom");
    fs::create_dir_all(primary.parent().unwrap()).unwrap();
    fs::write(&primary, b"primary").unwrap();
    fs::write(&profile, b"profile").unwrap();
    write_process(root.path(), 10, 10, 34816, 20, "/bin/sh", &["/bin/sh"]);
    write_process(
        root.path(),
        20,
        20,
        34816,
        20,
        "/usr/bin/env",
        &["/usr/bin/env", profile.to_str().unwrap()],
    );
    let primary = CodexExecutable::new(primary).unwrap();
    let profile_executable = CodexExecutable::new(&profile).unwrap();
    let inspector = LinuxProcessInspector::with_proc_root_and_executables(
        primary.clone(),
        vec![primary, profile_executable],
        root.path(),
    );

    assert_eq!(inspector.foreground_executable(10).unwrap(), Some(profile));
    assert!(!inspector.foreground_process_is_exact(10).unwrap());
}

#[test]
fn batched_proc_snapshot_resolves_multiple_foreground_groups() {
    let root = TemporaryDirectory::new("proc-batch");
    let configured = root.path().join("bin/codex-custom");
    fs::create_dir_all(configured.parent().unwrap()).unwrap();
    fs::write(&configured, b"fixture").unwrap();
    write_process(root.path(), 10, 10, 34816, 20, "/bin/sh", &["/bin/sh"]);
    write_process(
        root.path(),
        20,
        20,
        34816,
        20,
        "/usr/bin/env",
        &["/usr/bin/env", configured.to_str().unwrap()],
    );
    write_process(root.path(), 11, 11, 34817, 30, "/bin/sh", &["/bin/sh"]);
    write_process(
        root.path(),
        30,
        30,
        34817,
        30,
        configured.to_str().unwrap(),
        &[configured.to_str().unwrap()],
    );
    let inspector = LinuxProcessInspector::with_proc_root(
        CodexExecutable::new(&configured).unwrap(),
        root.path(),
    );

    let results = inspector.foreground_executables(&[10, 11]);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].as_ref().unwrap(), &Some(configured.clone()));
    assert_eq!(results[1].as_ref().unwrap(), &Some(configured));
}

#[test]
fn batched_proc_snapshot_isolates_pane_errors_and_uses_leader_fallback() {
    let root = TemporaryDirectory::new("proc-batch-isolation");
    let configured = root.path().join("bin/codex-custom");
    fs::create_dir_all(configured.parent().unwrap()).unwrap();
    fs::write(&configured, b"fixture").unwrap();
    write_process(root.path(), 10, 10, 34816, 20, "/bin/sh", &["/bin/sh"]);
    write_process(
        root.path(),
        20,
        20,
        34816,
        20,
        "/tmp/leader",
        &["/tmp/leader"],
    );
    fs::create_dir_all(root.path().join("12/stat")).unwrap();
    let inspector = LinuxProcessInspector::with_proc_root(
        CodexExecutable::new(&configured).unwrap(),
        root.path(),
    );

    let results = inspector.foreground_executables(&[10, 99, 12]);
    assert_eq!(results.len(), 3);
    assert_eq!(
        results[0].as_ref().unwrap(),
        &Some(PathBuf::from("/tmp/leader"))
    );
    assert_eq!(results[1].as_ref().unwrap(), &None);
    assert!(results[2].is_err());
}

#[test]
fn direct_foreground_interactive_bash_and_zsh_are_shell_targets() {
    for (pid, shell, arguments) in [
        (
            41,
            "/bin/bash",
            vec!["/bin/bash", "--rcfile", "/tmp/bashrc", "-i"],
        ),
        (42, "/bin/zsh", vec!["/bin/zsh", "-o", "interactive"]),
    ] {
        let root = TemporaryDirectory::new("proc-interactive-shell");
        write_process(
            root.path(),
            pid,
            pid as i64,
            34816,
            pid as i64,
            shell,
            &arguments,
        );
        let inspector = LinuxProcessInspector::with_proc_root(
            CodexExecutable::new("/opt/bin/codex").unwrap(),
            root.path(),
        );

        assert!(
            inspector
                .foreground_process_is_shell(
                    pid,
                    Path::new(shell).file_name().unwrap().to_str().unwrap()
                )
                .unwrap()
        );
    }
}

#[test]
fn unrelated_executable_named_bash_is_not_a_shell_target() {
    let root = TemporaryDirectory::new("proc-fake-bash");
    let fake = root.path().join("bin/bash");
    fs::create_dir_all(fake.parent().unwrap()).unwrap();
    fs::write(&fake, b"not bash").unwrap();
    write_process(
        root.path(),
        61,
        61,
        34816,
        61,
        fake.to_str().unwrap(),
        &[fake.to_str().unwrap()],
    );
    let inspector = LinuxProcessInspector::with_proc_root(
        CodexExecutable::new("/opt/bin/codex").unwrap(),
        root.path(),
    );

    assert!(!inspector.foreground_process_is_shell(61, "bash").unwrap());
}

#[test]
fn shell_target_rejects_jobs_wrappers_command_mode_and_identity_mismatch() {
    let cases = [
        (51, 50, 34816, 50, "/bin/bash", vec!["/bin/bash"], "bash"),
        (
            52,
            52,
            34816,
            52,
            "/usr/bin/env",
            vec!["env", "/bin/bash"],
            "bash",
        ),
        (
            53,
            53,
            34816,
            53,
            "/bin/bash",
            vec!["bash", "-c", "echo"],
            "bash",
        ),
        (54, 54, 34816, 54, "/bin/bash", vec!["bash"], "zsh"),
        (55, 55, 0, 55, "/bin/bash", vec!["bash"], "bash"),
    ];
    for (pid, pgrp, tty, tpgid, executable, arguments, command) in cases {
        let root = TemporaryDirectory::new("proc-rejected-shell");
        write_process(root.path(), pid, pgrp, tty, tpgid, executable, &arguments);
        let inspector = LinuxProcessInspector::with_proc_root(
            CodexExecutable::new("/opt/bin/codex").unwrap(),
            root.path(),
        );
        assert!(
            !inspector.foreground_process_is_shell(pid, command).unwrap(),
            "unexpectedly accepted {executable} {arguments:?}"
        );
    }
}

#[test]
fn exact_codex_in_a_shell_foreground_group_is_direct() {
    let root = TemporaryDirectory::new("proc-direct-foreground");
    let configured = root.path().join("bin/codex-custom");
    fs::create_dir_all(configured.parent().unwrap()).unwrap();
    fs::write(&configured, b"fixture").unwrap();
    write_process(root.path(), 10, 10, 34816, 20, "/bin/sh", &["/bin/sh"]);
    write_process(
        root.path(),
        20,
        20,
        34816,
        20,
        configured.to_str().unwrap(),
        &[configured.to_str().unwrap()],
    );
    let inspector = LinuxProcessInspector::with_proc_root(
        CodexExecutable::new(&configured).unwrap(),
        root.path(),
    );

    assert!(inspector.foreground_process_is_exact(10).unwrap());
}

#[test]
fn exact_codex_outside_the_pane_foreground_group_is_not_direct() {
    for (name, codex_pgrp, codex_tty) in [
        ("background-group", 20, 34816),
        ("different-tty", 30, 34817),
    ] {
        let root = TemporaryDirectory::new(name);
        let configured = root.path().join("bin/codex-custom");
        fs::create_dir_all(configured.parent().unwrap()).unwrap();
        fs::write(&configured, b"fixture").unwrap();
        write_process(root.path(), 10, 10, 34816, 30, "/bin/sh", &["/bin/sh"]);
        write_process(
            root.path(),
            20,
            codex_pgrp,
            codex_tty,
            codex_pgrp,
            configured.to_str().unwrap(),
            &[configured.to_str().unwrap()],
        );
        let inspector = LinuxProcessInspector::with_proc_root(
            CodexExecutable::new(&configured).unwrap(),
            root.path(),
        );

        assert!(!inspector.foreground_process_is_exact(10).unwrap());
    }
}

#[test]
fn unrelated_process_with_codex_as_data_is_not_a_wrapper_match() {
    let root = TemporaryDirectory::new("proc-not-wrapper");
    let configured = root.path().join("bin/codex-custom");
    fs::create_dir_all(configured.parent().unwrap()).unwrap();
    fs::write(&configured, b"fixture").unwrap();
    write_process(root.path(), 10, 10, 34816, 20, "/bin/sh", &["/bin/sh"]);
    write_process(
        root.path(),
        20,
        20,
        34816,
        20,
        "/usr/bin/cat",
        &["cat", configured.to_str().unwrap()],
    );
    let inspector = LinuxProcessInspector::with_proc_root(
        CodexExecutable::new(&configured).unwrap(),
        root.path(),
    );

    assert_eq!(
        inspector.foreground_executable(10).unwrap(),
        Some(PathBuf::from("/usr/bin/cat"))
    );
}

#[test]
fn unrelated_same_basename_interpreter_cannot_match_a_launcher_argument() {
    let root = TemporaryDirectory::new("proc-fake-interpreter");
    let configured = root.path().join("codex-launcher");
    let fake_bash = root.path().join("bash");
    fs::write(&configured, b"#!/bin/sh\n").unwrap();
    fs::write(&fake_bash, b"unrelated").unwrap();
    write_process(
        root.path(),
        10,
        10,
        34816,
        10,
        fake_bash.to_str().unwrap(),
        &[fake_bash.to_str().unwrap(), configured.to_str().unwrap()],
    );
    let inspector = LinuxProcessInspector::with_proc_root(
        CodexExecutable::new(&configured).unwrap(),
        root.path(),
    );

    assert_ne!(
        inspector.foreground_executable(10).unwrap(),
        Some(configured.clone())
    );
    assert!(!inspector.foreground_process_matches(10).unwrap());
}

#[test]
fn exact_configured_pane_matches_without_a_foreground_process_group() {
    let root = TemporaryDirectory::new("proc-detached-direct");
    let configured = root.path().join("bin/codex-custom");
    fs::create_dir_all(configured.parent().unwrap()).unwrap();
    fs::write(&configured, b"fixture").unwrap();
    write_process(
        root.path(),
        10,
        10,
        0,
        -1,
        configured.to_str().unwrap(),
        &[configured.to_str().unwrap()],
    );
    let inspector = LinuxProcessInspector::with_proc_root(
        CodexExecutable::new(&configured).unwrap(),
        root.path(),
    );

    assert_eq!(
        inspector.foreground_executable(10).unwrap(),
        Some(configured)
    );
}

#[test]
fn detached_wrapper_is_not_treated_as_a_foreground_codex_process() {
    let root = TemporaryDirectory::new("proc-detached-wrapper");
    let configured = root.path().join("bin/codex-custom");
    fs::create_dir_all(configured.parent().unwrap()).unwrap();
    fs::write(&configured, b"fixture").unwrap();
    write_process(
        root.path(),
        10,
        10,
        0,
        -1,
        "/usr/bin/env",
        &["env", configured.to_str().unwrap()],
    );
    let inspector = LinuxProcessInspector::with_proc_root(
        CodexExecutable::new(&configured).unwrap(),
        root.path(),
    );

    assert_eq!(inspector.foreground_executable(10).unwrap(), None);
}

#[test]
fn pane_tree_finds_background_launcher_but_rejects_unrelated_supervisor() {
    let root = TemporaryDirectory::new("proc-pane-tree");
    let launcher = root.path().join("bin/launcher");
    fs::create_dir_all(launcher.parent().unwrap()).unwrap();
    fs::write(&launcher, b"fixture").unwrap();
    // Pane shell; foreground supervisor is intentionally unrelated and the
    // configured launcher stays in a background process group.
    write_process(root.path(), 10, 10, 34816, 30, "/bin/sh", &["/bin/sh"]);
    write_process_with_parent(
        root.path(),
        20,
        10,
        34816,
        10,
        10,
        launcher.to_str().unwrap(),
        &[launcher.to_str().unwrap()],
    );
    write_process_with_parent(
        root.path(),
        30,
        30,
        34816,
        10,
        10,
        "/usr/bin/cat",
        &["supervisor"],
    );
    let inspector = LinuxProcessInspector::with_proc_root_and_matcher(
        vec![CodexExecutable::new(&launcher).unwrap()],
        MatchScope::PaneTree,
        &[],
        root.path(),
    )
    .unwrap();
    assert!(
        inspector
            .pane_process_matches(&PaneProcess {
                pid: 10,
                tty: PathBuf::new()
            })
            .unwrap()
    );
}

#[test]
fn pane_tty_finds_non_descendant_launcher_and_command_regex_matches_normalized_argv() {
    let root = TemporaryDirectory::new("proc-pane-tty");
    let configured = root.path().join("bin/configured-launcher");
    fs::create_dir_all(configured.parent().unwrap()).unwrap();
    fs::write(&configured, b"fixture").unwrap();
    let rdev = fs::metadata("/dev/null").unwrap().rdev();
    let major = ((rdev >> 8) & 0x0fff) | ((rdev >> 32) & !0x0fff);
    let minor = (rdev & 0x00ff) | ((rdev >> 12) & !0x00ff);
    let tty_nr = (((major & 0x0fff) << 8)
        | (minor & 0x00ff)
        | ((minor & !0x00ff) << 12)
        | ((major & !0x0fff) << 32)) as i64;
    write_process(root.path(), 10, 10, tty_nr, 10, "/bin/sh", &["/bin/sh"]);
    write_process_with_parent(
        root.path(),
        20,
        20,
        tty_nr,
        20,
        999,
        "/usr/bin/cat",
        &["/dynamic/launcher-v17", "--attached"],
    );
    let inspector = LinuxProcessInspector::with_proc_root_and_matcher(
        vec![CodexExecutable::new(&configured).unwrap()],
        MatchScope::PaneTty,
        &[r"^/dynamic/launcher-v[0-9]+ --attached$".to_owned()],
        root.path(),
    )
    .unwrap();

    assert!(
        inspector
            .pane_process_matches(&PaneProcess {
                pid: 10,
                tty: PathBuf::from("/dev/null"),
            })
            .unwrap()
    );
}

#[test]
fn interpreter_option_operands_are_never_treated_as_launcher_scripts() {
    let root = TemporaryDirectory::new("proc-interpreter-options");
    let configured = root.path().join("launcher");
    fs::write(&configured, b"fixture").unwrap();
    for (pid, executable, arguments) in [
        (
            10,
            "/bin/bash",
            vec!["bash", "--rcfile", configured.to_str().unwrap(), "-i"],
        ),
        (
            20,
            "/usr/bin/python3",
            vec!["python3", "-c", configured.to_str().unwrap()],
        ),
        (
            30,
            "/usr/bin/python3",
            vec!["python3", "-X", "dev", configured.to_str().unwrap()],
        ),
    ] {
        write_process(
            root.path(),
            pid,
            pid as i64,
            34816,
            pid as i64,
            executable,
            &arguments,
        );
        let inspector = LinuxProcessInspector::with_proc_root(
            CodexExecutable::new(&configured).unwrap(),
            root.path(),
        );
        assert!(!inspector.foreground_process_matches(pid).unwrap());
    }
}

#[test]
fn forged_argv_zero_does_not_override_unrelated_executable_identity() {
    let root = TemporaryDirectory::new("proc-forged-argv-zero");
    let configured = root.path().join("bin/codex-custom");
    fs::create_dir_all(configured.parent().unwrap()).unwrap();
    fs::write(&configured, b"fixture").unwrap();
    write_process(root.path(), 10, 10, 34816, 20, "/bin/sh", &["/bin/sh"]);
    write_process(
        root.path(),
        20,
        20,
        34816,
        20,
        "/usr/bin/sleep",
        &[configured.to_str().unwrap(), "30"],
    );
    let inspector = LinuxProcessInspector::with_proc_root(
        CodexExecutable::new(&configured).unwrap(),
        root.path(),
    );

    assert_eq!(
        inspector.foreground_executable(10).unwrap(),
        Some(PathBuf::from("/usr/bin/sleep"))
    );
}

#[test]
fn malformed_or_missing_proc_entries_return_none() {
    let root = TemporaryDirectory::new("proc-malformed");
    fs::create_dir(root.path().join("10")).unwrap();
    fs::write(root.path().join("10/stat"), b"malformed").unwrap();
    let inspector = LinuxProcessInspector::with_proc_root(
        CodexExecutable::new("/opt/bin/codex").unwrap(),
        root.path(),
    );

    assert_eq!(inspector.foreground_executable(10).unwrap(), None);
    assert_eq!(inspector.foreground_executable(999).unwrap(), None);
}

#[test]
#[ignore = "host tmux smoke; packaged E2E provides the isolated CI gate"]
fn disposable_tmux_server_smoke_discovers_a_foreground_process() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux is unavailable; skipping disposable-server smoke test");
        return;
    }

    let socket = format!(
        "codex-mux-inventory-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let server = TmuxServer::start(&socket);
    let executable = fs::canonicalize("/usr/bin/sleep").unwrap();
    let pane_pid = Command::new("tmux")
        .args(["-L", &socket, "list-panes", "-a", "-F", "#{pane_pid}"])
        .output()
        .unwrap();
    assert!(
        pane_pid.status.success(),
        "could not inspect disposable tmux pane: {}",
        String::from_utf8_lossy(&pane_pid.stderr)
    );
    let pane_pid = String::from_utf8(pane_pid.stdout)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    let proc_executable = format!("/proc/{pane_pid}/exe");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match fs::read_link(&proc_executable) {
            Ok(actual) if actual == executable => break,
            Ok(actual) if Instant::now() >= deadline => {
                panic!(
                    "disposable pane process did not become {}: {}",
                    executable.display(),
                    actual.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "runner denies /proc/{pane_pid}/exe inspection; skipping disposable-server smoke test"
                );
                return;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("could not inspect disposable pane process {pane_pid}: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "disposable pane process {pane_pid} never became inspectable"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let runner = PrefixedRunner {
        inner: SystemTmuxRunner::default(),
        socket: socket.clone(),
    };
    let inventory = PaneInventory::new(
        runner,
        LinuxProcessInspector::new(CodexExecutable::new(&executable).unwrap()),
        CodexExecutable::new(&executable).unwrap(),
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let panes = loop {
        let panes = inventory.discover().unwrap();
        if !panes.is_empty() || Instant::now() >= deadline {
            break panes;
        }
        thread::sleep(Duration::from_millis(20));
    };

    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].session_id.as_str(), "$0");
    assert_eq!(panes[0].current_path, env::current_dir().unwrap());
    drop(server);
}

#[test]
fn real_tmux_recognizes_foreground_script_and_spawned_underlying_binary() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux is unavailable; skipping real-wrapper integration test");
        return;
    }
    for underlying_mode in [false, true] {
        let root = TemporaryDirectory::new(if underlying_mode {
            "real-underlying-wrapper"
        } else {
            "real-foreground-wrapper"
        });
        let launcher = root.path().join("codex-launcher");
        let underlying = root.path().join("runtime-codex");
        if underlying_mode {
            fs::copy("/usr/bin/sleep", &underlying).unwrap();
            fs::write(
                &launcher,
                format!("#!/bin/sh\nexec '{}' 30\n", underlying.display()),
            )
            .unwrap();
        } else {
            fs::write(&launcher, b"#!/bin/sh\nsleep 30\n").unwrap();
        }
        let mut permissions = fs::metadata(&launcher).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&launcher, permissions).unwrap();

        let socket = format!(
            "codex-mux-real-wrapper-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let server = TmuxServer::start_command(&socket, &format!("exec '{}'", launcher.display()));
        let pane_pid = Command::new("tmux")
            .args(["-L", &socket, "list-panes", "-a", "-F", "#{pane_pid}"])
            .output()
            .unwrap();
        let pane_pid = String::from_utf8(pane_pid.stdout)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        let launcher_executable = CodexExecutable::new(&launcher).unwrap();
        let mut matches = vec![launcher_executable.clone()];
        if underlying_mode {
            matches.push(CodexExecutable::new(&underlying).unwrap());
        }
        let inspector = LinuxProcessInspector::matching_executables(matches.clone());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !inspector.foreground_process_matches(pane_pid).unwrap() {
            assert!(Instant::now() < deadline, "real wrapper never matched");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            DirectCodexInspector::is_direct_codex(
                &inspector,
                &PaneProcess {
                    pid: pane_pid,
                    tty: PathBuf::new()
                },
            )
            .unwrap(),
            "Smart Left process boundary rejected a wrapper accepted by inventory"
        );
        let inventory = PaneInventory::with_executables(
            PrefixedRunner {
                inner: SystemTmuxRunner::default(),
                socket: socket.clone(),
            },
            LinuxProcessInspector::matching_executables(matches.clone()),
            matches,
        );
        assert_eq!(inventory.discover().unwrap().len(), 1);
        drop(server);
    }
}

#[test]
fn real_tmux_pane_tree_recognizes_background_launcher_behind_supervisor() {
    if Command::new("tmux").arg("-V").output().is_err() || !Path::new("/usr/bin/python3").is_file()
    {
        eprintln!("tmux or python3 unavailable; skipping real pane-tree integration test");
        return;
    }
    let root = TemporaryDirectory::new("real-pane-tree-wrapper");
    let launcher = root.path().join("codex-launcher.sh");
    fs::write(&launcher, b"#!/bin/sh\nsleep 30\n").unwrap();
    let supervisor = root.path().join("supervisor.py");
    fs::write(
        &supervisor,
        format!(
            "#!/usr/bin/python3\nimport os\npid=os.fork()\nif pid==0:\n os.setpgid(0,0)\n os.execl('{}','{}')\nos.waitpid(pid,0)\n",
            launcher.display(),
            launcher.display()
        ),
    ).unwrap();
    let mut permissions = fs::metadata(&launcher).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&launcher, permissions).unwrap();
    let mut permissions = fs::metadata(&supervisor).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&supervisor, permissions).unwrap();
    let socket = format!(
        "codex-mux-pane-tree-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let server = TmuxServer::start_command(&socket, &format!("exec '{}'", supervisor.display()));
    let output = Command::new("tmux")
        .args([
            "-L",
            &socket,
            "list-panes",
            "-a",
            "-F",
            "#{pane_pid}\x1f#{pane_tty}\x1f#{pane_current_command}",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let row = String::from_utf8(output.stdout).unwrap();
    let fields = row.trim().split('\x1f').collect::<Vec<_>>();
    let pane = PaneProcess {
        pid: fields[0].parse().unwrap(),
        tty: PathBuf::from(fields[1]),
    };
    let executable = CodexExecutable::new(&launcher).unwrap();
    let inspector =
        LinuxProcessInspector::with_matcher(vec![executable.clone()], MatchScope::PaneTree, &[])
            .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !inspector.pane_process_matches(&pane).unwrap() {
        assert!(
            Instant::now() < deadline,
            "background launcher never matched"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let stat = fs::read_to_string(format!("/proc/{}/stat", pane.pid)).unwrap();
    let fields = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_ascii_whitespace()
        .collect::<Vec<_>>();
    let foreground_pid = fields[5].parse::<u32>().unwrap();
    assert!(
        fs::read_link(format!("/proc/{foreground_pid}/exe"))
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("python3")
    );
    assert!(
        DirectCodexInspector::is_direct_codex(&inspector, &pane).unwrap(),
        "Smart Left process boundary rejected the background launcher"
    );
    let inventory = PaneInventory::with_executables(
        PrefixedRunner {
            inner: SystemTmuxRunner::default(),
            socket: socket.clone(),
        },
        inspector,
        vec![executable],
    );
    assert_eq!(inventory.discover().unwrap().len(), 1);
    drop(server);
}

struct PrefixedRunner {
    inner: SystemTmuxRunner,
    socket: String,
}

impl TmuxCommandRunner for PrefixedRunner {
    fn run(&self, arguments: &[OsString]) -> Result<CommandOutput> {
        let mut prefixed = vec![OsString::from("-L"), OsString::from(&self.socket)];
        prefixed.extend_from_slice(arguments);
        self.inner.run(&prefixed)
    }
}

struct TmuxServer(String);

impl TmuxServer {
    fn start(socket: &str) -> Self {
        Self::start_command(socket, "exec /usr/bin/sleep 30")
    }

    fn start_command(socket: &str, command: &str) -> Self {
        let output = Command::new("tmux")
            .args([
                "-L",
                socket,
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "inventory-smoke",
                command,
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "could not start disposable tmux server: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self(socket.to_owned())
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.0, "kill-server"])
            .status();
    }
}

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn new(label: &str) -> Self {
        let path = env::temp_dir().join(format!(
            "codex-mux-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_process(
    proc_root: &Path,
    pid: u32,
    pgrp: i64,
    tty_nr: i64,
    tpgid: i64,
    executable: &str,
    arguments: &[&str],
) {
    write_process_with_parent(
        proc_root, pid, pgrp, tty_nr, tpgid, 1, executable, arguments,
    );
}

#[allow(clippy::too_many_arguments)]
fn write_process_with_parent(
    proc_root: &Path,
    pid: u32,
    pgrp: i64,
    tty_nr: i64,
    tpgid: i64,
    parent_pid: u32,
    executable: &str,
    arguments: &[&str],
) {
    let directory = proc_root.join(pid.to_string());
    fs::create_dir(&directory).unwrap();
    fs::write(
        directory.join("stat"),
        format!(
            "{pid} (fixture process) S {parent_pid} {pgrp} 1 {tty_nr} {tpgid} \
             0 0 0 0 0 0 0 0 0 0 0 0 0 {}",
            u64::from(pid) * 1_000
        ),
    )
    .unwrap();
    symlink(executable, directory.join("exe")).unwrap();
    let mut command_line = Vec::new();
    for argument in arguments {
        command_line.extend_from_slice(argument.as_bytes());
        command_line.push(0);
    }
    fs::write(directory.join("cmdline"), command_line).unwrap();
}
