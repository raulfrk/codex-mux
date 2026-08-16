use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use codex_mux::{
    MuxError, Result,
    domain::{CodexExecutable, CommandOutput, ProcessInspector, TmuxCommandRunner},
    linux_process::LinuxProcessInspector,
    tmux::{inventory::PaneInventory, runner::SystemTmuxRunner},
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
fn basename_fallback_requires_canonical_file_identity() {
    let root = TemporaryDirectory::new("canonical-fallback");
    let real = root.path().join("real/codex");
    let configured = root.path().join("configured/codex");
    fs::create_dir_all(real.parent().unwrap()).unwrap();
    fs::create_dir_all(configured.parent().unwrap()).unwrap();
    fs::write(&real, b"fixture").unwrap();
    symlink(&real, &configured).unwrap();
    let row =
        b"%1\x1f$1\x1f@1\x1fmain\x1fSymlinked\x1f/work/project\x1fcodex\x1f101\x1f/dev/pts/1\n"
            .to_vec();
    let processes = FakeProcesses(HashMap::from([(101, ProcessAnswer::Path(real))]));
    let inventory = PaneInventory::new(
        FakeRunner::returning(row),
        processes,
        CodexExecutable::new(configured).unwrap(),
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

    let rows = b"%1\x1f$1\x1f@1\x1fmain\x1fone\x1f/work/one\x1fcodex\x1f101\x1f/dev/pts/1\n%2\x1f$1\x1f@1\x1fmain\x1ftwo\x1f/work/two\x1fcodex\x1f102\x1f/dev/pts/2\n".to_vec();
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
                "exec /usr/bin/sleep 30",
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
    let directory = proc_root.join(pid.to_string());
    fs::create_dir(&directory).unwrap();
    fs::write(
        directory.join("stat"),
        format!("{pid} (fixture process) S 1 {pgrp} 1 {tty_nr} {tpgid} 0 0 0"),
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
