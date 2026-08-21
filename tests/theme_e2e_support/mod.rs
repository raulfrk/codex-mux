use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);
const TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(25);

pub fn packaged_binary() -> Option<PathBuf> {
    let path = match std::env::var_os("CODEX_MUX_E2E_BINARY") {
        Some(path) => PathBuf::from(path),
        None => {
            eprintln!(
                "skipping packaged theme E2E: CODEX_MUX_E2E_BINARY is set only by the required E2E driver"
            );
            return None;
        }
    };
    assert!(path.is_absolute(), "packaged E2E binary must be absolute");
    let metadata = fs::metadata(&path).expect("inspect packaged E2E binary");
    assert!(metadata.is_file(), "packaged E2E binary must be a file");
    assert_ne!(
        metadata.permissions().mode() & 0o111,
        0,
        "packaged E2E binary must be executable"
    );
    for tool in ["tmux", "script", "rustc"] {
        assert!(
            Command::new(tool).arg("--version").output().is_ok(),
            "required packaged E2E tool is unavailable: {tool}"
        );
    }
    Some(path)
}

pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("cmte-{label}-{}-{id}", std::process::id()));
        fs::create_dir(&path).expect("create packaged theme scratch directory");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, value: impl AsRef<Path>) -> PathBuf {
        self.path.join(value)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        assert!(
            self.path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("cmte-")),
            "refusing to clean unexpected path"
        );
        let _ = fs::remove_dir_all(&self.path);
        if !thread::panicking() {
            assert!(
                !self.path.exists(),
                "packaged theme scratch root survived cleanup: {}",
                self.path.display()
            );
        }
    }
}

pub struct TmuxServer {
    socket: String,
    socket_path: PathBuf,
}

impl TmuxServer {
    fn start(config: &Path, cwd: &Path) -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let socket = format!("codex-mux-packaged-theme-{}-{id}", std::process::id());
        let output = Command::new("tmux")
            .args(["-L", &socket, "-f"])
            .arg(config)
            .args(["new-session", "-d", "-s", "origin", "-c"])
            .arg(cwd)
            .output()
            .expect("start isolated tmux server");
        assert_success(&output, "start isolated tmux server");
        let socket_output = Command::new("tmux")
            .args(["-L", &socket, "display-message", "-p", "#{socket_path}"])
            .output()
            .expect("resolve isolated tmux socket");
        assert_success(&socket_output, "resolve isolated tmux socket");
        let socket_path = PathBuf::from(
            String::from_utf8(socket_output.stdout)
                .expect("tmux socket path must be UTF-8")
                .trim(),
        );
        Self {
            socket,
            socket_path,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new("tmux");
        command.args(["-L", &self.socket]);
        command
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command()
            .args(arguments)
            .output()
            .expect("run isolated tmux command")
    }

    fn checked(&self, arguments: &[&str]) -> String {
        let output = self.run(arguments);
        assert_success(&output, &format!("tmux {}", arguments.join(" ")));
        String::from_utf8(output.stdout).expect("tmux output must be UTF-8")
    }

    fn environment(&self) -> String {
        let socket = self
            .checked(&["display-message", "-p", "#{socket_path}"])
            .trim()
            .to_owned();
        let pid = self
            .checked(&["display-message", "-p", "#{pid}"])
            .trim()
            .to_owned();
        format!("{socket},{pid},0")
    }

    fn wait_until(&self, description: &str, mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if predicate() {
                return;
            }
            thread::sleep(POLL);
        }
        panic!("timed out waiting for {description}");
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = self.run(&["kill-server"]);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut server_alive = true;
        while Instant::now() < deadline {
            server_alive = self.run(&["list-sessions"]).status.success();
            if !server_alive {
                break;
            }
            thread::sleep(POLL);
        }
        if !server_alive
            && self.socket_path.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .starts_with("codex-mux-packaged-theme-")
            })
        {
            let _ = fs::remove_file(&self.socket_path);
        }
        if !thread::panicking() {
            assert!(!server_alive, "isolated tmux server survived cleanup");
            assert!(
                !self.socket_path.exists(),
                "isolated tmux socket survived cleanup: {}",
                self.socket_path.display()
            );
        }
    }
}

pub struct PtyProcess {
    child: Child,
    capture: PathBuf,
}

impl PtyProcess {
    fn spawn(
        binary: &Path,
        arguments: &[String],
        environment: &[(&str, &str)],
        capture: PathBuf,
        columns: u16,
        rows: u16,
    ) -> Self {
        let output = fs::File::create(&capture).expect("create PTY capture");
        let mut words = vec![shell_quote(&binary.to_string_lossy())];
        words.extend(arguments.iter().map(|argument| shell_quote(argument)));
        let command_line = format!("stty cols {columns} rows {rows}; exec {}", words.join(" "));
        let mut command = Command::new("script");
        command
            .args(["-qfec", &command_line, "/dev/null"])
            .env_remove("TMUX")
            .env_remove("NO_COLOR")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::piped())
            .stdout(Stdio::from(output))
            .stderr(Stdio::null());
        for (name, value) in environment {
            command.env(name, value);
        }
        let child = command.spawn().expect("start packaged binary in PTY");
        Self { child, capture }
    }

    pub fn send(&mut self, bytes: &[u8]) {
        let input = self.child.stdin.as_mut().expect("PTY input is open");
        input.write_all(bytes).expect("write PTY input");
        input.flush().expect("flush PTY input");
    }

    pub fn wait_for_text(&self, expected: &str) -> Vec<u8> {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            let contents = fs::read(&self.capture).unwrap_or_default();
            if String::from_utf8_lossy(&contents).contains(expected) {
                return contents;
            }
            thread::sleep(POLL);
        }
        panic!(
            "timed out waiting for {expected:?}; capture={}",
            bounded_capture(&self.capture)
        );
    }

    pub fn wait_for_growth(&self, previous_length: usize) -> Vec<u8> {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            let contents = fs::read(&self.capture).unwrap_or_default();
            if contents.len() > previous_length {
                return contents;
            }
            thread::sleep(POLL);
        }
        panic!(
            "timed out waiting for PTY redraw; capture={}",
            bounded_capture(&self.capture)
        );
    }

    pub fn wait_for_appended_text(&self, previous_length: usize, expected: &str) -> Vec<u8> {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            let contents = fs::read(&self.capture).unwrap_or_default();
            if contents.len() > previous_length
                && String::from_utf8_lossy(&contents[previous_length..]).contains(expected)
            {
                return contents;
            }
            thread::sleep(POLL);
        }
        panic!(
            "timed out waiting for appended {expected:?}; capture={}",
            bounded_capture(&self.capture)
        );
    }

    pub fn wait_for_exit(&mut self) -> Vec<u8> {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if self.child.try_wait().expect("poll packaged PTY").is_some() {
                return fs::read(&self.capture).unwrap_or_default();
            }
            thread::sleep(POLL);
        }
        panic!(
            "timed out waiting for packaged binary to exit; capture={}",
            bounded_capture(&self.capture)
        );
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut exited = false;
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                exited = true;
                break;
            }
            thread::sleep(POLL);
        }
        if !thread::panicking() {
            assert!(exited, "packaged PTY process survived cleanup");
        }
    }
}

pub struct ThemeFixture {
    client: Child,
    server: TmuxServer,
    scratch: Scratch,
    binary: PathBuf,
    codex: PathBuf,
    client_tty: String,
    pane_id: String,
    session_id: String,
    window_id: String,
    popup_number: u64,
}

impl ThemeFixture {
    pub fn new(label: &str, binary: PathBuf) -> Self {
        let scratch = Scratch::new(label);
        let home = scratch.join("home");
        let project = scratch.join("theme-agent-project");
        fs::create_dir(&home).expect("create isolated HOME");
        fs::create_dir(&project).expect("create agent project");
        let config = scratch.join("tmux.conf");
        fs::write(&config, "set -g status off\n").expect("write tmux config");
        let codex = compile_fake_codex(&scratch);
        let server = TmuxServer::start(&config, scratch.path());
        let output = server
            .command()
            .args(["new-session", "-d", "-s", "agent", "-c"])
            .arg(&project)
            .arg("--")
            .arg(&codex)
            .output()
            .expect("start fake Codex pane");
        assert_success(&output, "start fake Codex pane");
        let pane_id = server
            .checked(&["display-message", "-p", "-t", "agent", "#{pane_id}"])
            .trim()
            .to_owned();
        server.checked(&["select-pane", "-t", &pane_id, "-T", "theme-agent-thread"]);
        server.wait_until("fake Codex foreground process", || {
            server
                .checked(&[
                    "display-message",
                    "-p",
                    "-t",
                    &pane_id,
                    "#{pane_current_command}",
                ])
                .trim()
                == "codex-theme-e2e"
        });
        let pane = server
            .checked(&["display-message", "-p", "-t", "origin", "#{pane_id}"])
            .trim()
            .to_owned();
        let session_id = server
            .checked(&["display-message", "-p", "-t", "origin", "#{session_id}"])
            .trim()
            .to_owned();
        let attach = format!(
            "stty cols 120 rows 40; exec tmux -L {} attach-session -t origin",
            server.socket
        );
        let client = Command::new("script")
            .args(["-qfec", &attach, "/dev/null"])
            .env_remove("TMUX")
            .env("HOME", &home)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("attach invoking tmux client");
        let mut client_tty = None;
        server.wait_until("invoking tmux client", || {
            client_tty = server
                .checked(&["list-clients", "-F", "#{client_tty}"])
                .lines()
                .next()
                .map(str::to_owned);
            client_tty.is_some()
        });
        let window_id = server
            .checked(&["display-message", "-p", "-t", &pane, "#{window_id}"])
            .trim()
            .to_owned();
        Self {
            client,
            server,
            scratch,
            binary,
            codex,
            client_tty: client_tty.unwrap(),
            pane_id: pane,
            session_id,
            window_id,
            popup_number: 0,
        }
    }

    pub fn config_path(&self) -> PathBuf {
        self.xdg().join("codex-mux/config.toml")
    }

    pub fn xdg(&self) -> PathBuf {
        self.scratch.join("xdg")
    }

    pub fn project_path(&self) -> PathBuf {
        self.scratch.join("theme-agent-project")
    }

    pub fn write_config(&self, contents: &[u8]) {
        let path = self.config_path();
        fs::create_dir_all(path.parent().unwrap()).expect("create theme config directory");
        fs::write(path, contents).expect("write theme config");
    }

    pub fn popup(&mut self, columns: u16, rows: u16, no_color: bool) -> PtyProcess {
        self.popup_number += 1;
        let capture = self
            .scratch
            .join(format!("popup-{}.capture", self.popup_number));
        let arguments = vec![
            "--codex".to_owned(),
            self.codex.to_string_lossy().into_owned(),
            "--client".to_owned(),
            self.client_tty.clone(),
            "--invoking-pane".to_owned(),
            self.pane_id.clone(),
            "--invoking-session".to_owned(),
            self.session_id.clone(),
            "--invoking-window".to_owned(),
            self.window_id.clone(),
            "--invoking-path".to_owned(),
            self.scratch.path().to_string_lossy().into_owned(),
        ];
        let tmux = self.server.environment();
        let home = self.scratch.join("home");
        let xdg = self.xdg();
        fs::create_dir_all(&xdg).expect("create isolated XDG root");
        let mut environment = vec![
            ("TMUX", tmux.as_str()),
            ("HOME", home.to_str().unwrap()),
            ("XDG_CONFIG_HOME", xdg.to_str().unwrap()),
        ];
        if no_color {
            environment.push(("NO_COLOR", "1"));
        }
        PtyProcess::spawn(
            &self.binary,
            &arguments,
            &environment,
            capture,
            columns,
            rows,
        )
    }
}

impl Drop for ThemeFixture {
    fn drop(&mut self) {
        if let Some(input) = self.client.stdin.as_mut() {
            let _ = input.write_all(b"\x02d");
            let _ = input.flush();
        }
        if self.client.try_wait().ok().flatten().is_none() {
            let _ = self.client.kill();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut exited = false;
        while Instant::now() < deadline {
            if self.client.try_wait().ok().flatten().is_some() {
                exited = true;
                break;
            }
            thread::sleep(POLL);
        }
        if !thread::panicking() {
            assert!(exited, "invoking tmux client survived cleanup");
        }
    }
}

pub fn wait_for_exact_file(path: &Path, expected: &[u8]) {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if fs::read(path).ok().as_deref() == Some(expected) {
            return;
        }
        thread::sleep(POLL);
    }
    panic!(
        "timed out waiting for exact file {}: actual={:?}",
        path.display(),
        fs::read(path).ok()
    );
}

pub fn assert_screen_data(screen: &[u8], project: &Path) {
    let screen = String::from_utf8_lossy(screen);
    assert!(
        screen.contains("theme-agent-thread"),
        "missing title: {screen}"
    );
    assert!(
        screen.contains(&project.to_string_lossy().into_owned()),
        "missing path: {screen}"
    );
    assert!(screen.contains("Commands"), "missing action help: {screen}");
    for action in ["switch", "new", "session", "resume", "close", "themes"] {
        assert!(
            screen.contains(action),
            "missing action {action:?}: {screen}"
        );
    }
}

fn compile_fake_codex(scratch: &Scratch) -> PathBuf {
    let source = scratch.join("fake_codex.rs");
    let executable = scratch.join("codex-theme-e2e");
    fs::write(
        &source,
        "fn main() { std::thread::sleep(std::time::Duration::from_secs(120)); }\n",
    )
    .expect("write fake Codex source");
    let output = Command::new("rustc")
        .args(["--edition=2024", "-o"])
        .arg(&executable)
        .arg(&source)
        .output()
        .expect("compile fake Codex executable");
    assert_success(&output, "compile fake Codex executable");
    executable
}

fn assert_success(output: &Output, operation: &str) {
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

fn bounded_capture(path: &Path) -> String {
    let bytes = fs::read(path).unwrap_or_default();
    let start = bytes.len().saturating_sub(4_096);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}
