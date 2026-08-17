use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);
static TMUX_TEST_LOCK: Mutex<()> = Mutex::new(());

pub const POLL_INTERVAL: Duration = Duration::from_millis(25);
pub const TEST_TIMEOUT: Duration = Duration::from_secs(5);

pub fn serial_tmux_test() -> MutexGuard<'static, ()> {
    TMUX_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

pub fn tools_available() -> bool {
    ["tmux", "script"]
        .into_iter()
        .all(|tool| Command::new(tool).arg("--version").output().is_ok())
}

pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    pub fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock predates Unix epoch")
            .as_nanos();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "codex-mux-e2e-{label}-{}-{nonce}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create E2E scratch directory");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, name: impl AsRef<Path>) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        assert!(
            self.path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("codex-mux-e2e-")),
            "refusing to clean an unexpected E2E path"
        );
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub struct TmuxServer {
    socket: String,
}

impl TmuxServer {
    pub fn start(config: &Path, session: &str, cwd: &Path) -> Self {
        let nonce = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let socket = format!("codex-mux-e2e-{}-{nonce}", std::process::id());
        let output = Command::new("tmux")
            .args(["-L", &socket, "-f"])
            .arg(config)
            .args(["new-session", "-d", "-s", session, "-c"])
            .arg(cwd)
            .output()
            .expect("start disposable tmux server");
        assert_success(&output, "start disposable tmux server");
        Self { socket }
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new("tmux");
        command.args(["-L", &self.socket]);
        command
    }

    pub fn run(&self, arguments: &[&str]) -> Output {
        self.command()
            .args(arguments)
            .output()
            .expect("run tmux command")
    }

    pub fn checked(&self, arguments: &[&str]) -> String {
        let output = self.run(arguments);
        assert_success(&output, &format!("tmux {}", arguments.join(" ")));
        String::from_utf8(output.stdout).expect("tmux output must be UTF-8")
    }

    pub fn socket(&self) -> &str {
        &self.socket
    }

    pub fn tmux_environment(&self) -> String {
        let socket_path = self.checked(&["display-message", "-p", "#{socket_path}"]);
        let server_pid = self.checked(&["display-message", "-p", "#{pid}"]);
        let value = format!("{},{},0", socket_path.trim(), server_pid.trim());
        let output = Command::new("tmux")
            .env("TMUX", &value)
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()
            .expect("verify derived TMUX environment");
        assert_success(&output, "connect through derived TMUX environment");
        value
    }

    pub fn wait_until(&self, description: &str, mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        while Instant::now() < deadline {
            if predicate() {
                return;
            }
            thread::sleep(POLL_INTERVAL);
        }
        let messages = self.run(&["show-messages"]);
        let panes = self.run(&[
            "list-panes",
            "-a",
            "-F",
            "#{pane_id} #{pane_pid} #{pane_current_command} #{cursor_x} #{cursor_y}",
        ]);
        panic!(
            "timed out waiting for {description}; messages={} panes={}",
            String::from_utf8_lossy(&messages.stdout),
            String::from_utf8_lossy(&panes.stdout)
        );
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = self.run(&["kill-server"]);
    }
}

pub struct PtyProcess {
    child: Child,
}

impl PtyProcess {
    pub fn attach(server: &TmuxServer, session: &str, columns: u16, rows: u16) -> Self {
        let shell_command = format!(
            "stty cols {columns} rows {rows}; exec tmux -L {} attach-session -t {session}",
            server.socket()
        );
        Self::spawn_shell(&shell_command, &[])
    }

    pub fn attach_captured(
        server: &TmuxServer,
        session: &str,
        columns: u16,
        rows: u16,
        output: &Path,
    ) -> Self {
        let shell_command = format!(
            "stty cols {columns} rows {rows}; exec tmux -L {} attach-session -t {session}",
            server.socket()
        );
        let output = fs::File::create(output).expect("create attached-client PTY capture");
        Self::spawn_shell_with_stdout(&shell_command, &[], Stdio::from(output))
    }

    pub fn run_binary_captured(
        arguments: &[String],
        environment: &[(&str, &str)],
        output: &Path,
    ) -> Self {
        let output = fs::File::create(output).expect("create PTY capture");
        Self::run_binary_with_stdout(arguments, environment, Stdio::from(output))
    }

    fn run_binary_with_stdout(
        arguments: &[String],
        environment: &[(&str, &str)],
        stdout: Stdio,
    ) -> Self {
        let binary = env!("CARGO_BIN_EXE_codex-mux");
        let mut words = vec![shell_quote(
            Path::new(binary).as_os_str().to_string_lossy().as_ref(),
        )];
        words.extend(arguments.iter().map(|argument| shell_quote(argument)));
        let command = format!("stty cols 120 rows 40; exec {}", words.join(" "));
        Self::spawn_shell_with_stdout(&command, environment, stdout)
    }

    fn spawn_shell(shell_command: &str, environment: &[(&str, &str)]) -> Self {
        Self::spawn_shell_with_stdout(shell_command, environment, Stdio::null())
    }

    fn spawn_shell_with_stdout(
        shell_command: &str,
        environment: &[(&str, &str)],
        stdout: Stdio,
    ) -> Self {
        let mut command = Command::new("script");
        command
            .args(["-qfec", shell_command, "/dev/null"])
            .env_remove("TMUX")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::piped())
            .stdout(stdout)
            .stderr(Stdio::null());
        for (name, value) in environment {
            command.env(name, value);
        }
        let child = command.spawn().expect("start pseudo-terminal process");
        Self { child }
    }

    pub fn send(&mut self, bytes: &[u8]) {
        let input = self.child.stdin.as_mut().expect("PTY stdin is open");
        input.write_all(bytes).expect("write PTY input");
        input.flush().expect("flush PTY input");
    }

    pub fn wait_for_exit(&mut self) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        while Instant::now() < deadline {
            if self.child.try_wait().expect("poll PTY process").is_some() {
                return;
            }
            thread::sleep(POLL_INTERVAL);
        }
        panic!("timed out waiting for pseudo-terminal process to exit");
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

pub fn wait_for_file_text(path: &Path, expected: &str) -> String {
    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut last = String::new();
    while Instant::now() < deadline {
        if let Ok(contents) = fs::read_to_string(path) {
            if contents.contains(expected) {
                return contents;
            }
            last = contents;
        }
        thread::sleep(POLL_INTERVAL);
    }
    panic!(
        "timed out waiting for {expected:?} in {}; capture={last:?}",
        path.display()
    );
}

pub fn wait_for_file_text_after(path: &Path, offset: usize, expected: &str) -> Vec<u8> {
    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut last = String::new();
    while Instant::now() < deadline {
        if let Ok(contents) = fs::read(path) {
            let tail = String::from_utf8_lossy(contents.get(offset..).unwrap_or_default());
            if tail.contains(expected) {
                return contents;
            }
            last = tail.into_owned();
        }
        thread::sleep(POLL_INTERVAL);
    }
    panic!(
        "timed out waiting for {expected:?} after byte {offset} in {}; tail={last:?}",
        path.display(),
    );
}

pub fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed ({}): stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
