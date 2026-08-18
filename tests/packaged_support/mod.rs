use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);
const TIMEOUT: Duration = Duration::from_secs(8);

pub fn require_prerequisites() -> Option<PathBuf> {
    let Some(binary) = std::env::var_os("CODEX_MUX_E2E_BINARY").map(PathBuf::from) else {
        eprintln!(
            "CODEX_MUX_E2E_BINARY is unset; skipping packaged runtime E2E (scripts/e2e.sh supplies it)"
        );
        return None;
    };
    for (tool, version_flag) in [
        ("tmux", "-V"),
        ("script", "--version"),
        ("rustc", "--version"),
        ("bwrap", "--version"),
        ("bash", "--version"),
        ("zsh", "--version"),
    ] {
        let status = Command::new(tool)
            .arg(version_flag)
            .status()
            .unwrap_or_else(|error| {
                panic!("required packaged E2E tool {tool} is unavailable: {error}")
            });
        assert!(status.success(), "required packaged E2E tool {tool} failed");
    }
    let binary = binary
        .canonicalize()
        .expect("canonicalize CODEX_MUX_E2E_BINARY");
    assert!(binary.is_file(), "packaged binary is not a file");
    Some(binary)
}

pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new(label: &str) -> Self {
        let root = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .expect("packaged E2E requires isolated TMPDIR");
        let path = root.join(format!(
            "codex-mux-packaged-{label}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create packaged E2E scratch directory");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, path: impl AsRef<Path>) -> PathBuf {
        self.0.join(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        assert!(
            self.0
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("codex-mux-packaged-"))
        );
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub struct Server {
    socket: String,
}

impl Server {
    pub fn start(config: &Path, cwd: &Path) -> Self {
        let socket = format!(
            "codex-mux-packaged-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let output = Command::new("tmux")
            .args(["-L", &socket, "-f"])
            .arg(config)
            .args(["new-session", "-d", "-s", "origin", "-c"])
            .arg(cwd)
            .arg("/bin/sh")
            .output()
            .expect("start isolated tmux server");
        assert_success(&output, "start isolated tmux server");
        Self { socket }
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new("tmux");
        command.args(["-L", &self.socket]);
        command
    }

    pub fn socket(&self) -> &str {
        &self.socket
    }

    pub fn run(&self, arguments: &[&str]) -> Output {
        self.command().args(arguments).output().expect("run tmux")
    }

    pub fn checked(&self, arguments: &[&str]) -> String {
        let output = self.run(arguments);
        assert_success(&output, &format!("tmux {}", arguments.join(" ")));
        String::from_utf8(output.stdout).expect("tmux output must be UTF-8")
    }

    pub fn environment(&self) -> String {
        format!(
            "{},{},0",
            self.checked(&["display-message", "-p", "#{socket_path}"])
                .trim(),
            self.checked(&["display-message", "-p", "#{pid}"]).trim()
        )
    }

    pub fn wait(&self, description: &str, condition: impl FnMut() -> bool) {
        wait(description, condition, || self.diagnostics());
    }

    fn diagnostics(&self) -> String {
        let panes = self.run(&[
            "list-panes",
            "-a",
            "-F",
            "#{session_name}:#{window_index}.#{pane_index} #{pane_id} #{pane_current_command} #{pane_current_path}",
        ]);
        let clients = self.run(&[
            "list-clients",
            "-F",
            "#{client_tty} #{client_session} #{pane_id}",
        ]);
        format!(
            "panes={} clients={} pane_err={} client_err={}",
            String::from_utf8_lossy(&panes.stdout),
            String::from_utf8_lossy(&clients.stdout),
            String::from_utf8_lossy(&panes.stderr),
            String::from_utf8_lossy(&clients.stderr)
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.run(&["kill-server"]);
    }
}

pub struct Pty {
    child: Child,
    capture: PathBuf,
}

impl Pty {
    pub fn attach(server: &Server, session: &str, size: (u16, u16), capture: &Path) -> Self {
        let command = format!(
            "stty cols {} rows {}; exec tmux -L {} attach-session -t {}",
            size.0,
            size.1,
            server.socket(),
            session
        );
        let output = fs::File::create(capture).expect("create client capture");
        let error = output.try_clone().expect("clone client capture");
        let child = Command::new("script")
            .args(["-qfec", &command, "/dev/null"])
            .env("TERM", "xterm-256color")
            .env_remove("TMUX")
            .stdin(Stdio::piped())
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(error))
            .spawn()
            .expect("attach disposable tmux client");
        Self {
            child,
            capture: capture.to_owned(),
        }
    }

    pub fn binary(
        binary: &Path,
        arguments: &[String],
        environment: &[(&str, &str)],
        size: (u16, u16),
        capture: &Path,
        stty: Option<(&Path, &Path)>,
    ) -> Self {
        let mut words = vec![quote(binary.to_string_lossy().as_ref())];
        words.extend(arguments.iter().map(|argument| quote(argument)));
        let invocation = words.join(" ");
        let command = if let Some((before, after)) = stty {
            format!(
                "stty cols {} rows {}; stty -g > {}; {}; code=$?; stty -g > {}; exit $code",
                size.0,
                size.1,
                quote(before.to_string_lossy().as_ref()),
                invocation,
                quote(after.to_string_lossy().as_ref())
            )
        } else {
            format!("stty cols {} rows {}; exec {invocation}", size.0, size.1)
        };
        let output = fs::File::create(capture).expect("create PTY capture");
        let error = output.try_clone().expect("clone PTY capture");
        let mut process = Command::new("script");
        process
            .args(["-qfec", &command, "/dev/null"])
            .env("TERM", "xterm-256color")
            .env_remove("TMUX")
            .stdin(Stdio::piped())
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(error));
        for (name, value) in environment {
            process.env(name, value);
        }
        Self {
            child: process.spawn().expect("start packaged binary PTY"),
            capture: capture.to_owned(),
        }
    }

    pub fn send(&mut self, input: &[u8]) {
        let stdin = self.child.stdin.as_mut().expect("PTY stdin");
        stdin.write_all(input).expect("write PTY input");
        stdin.flush().expect("flush PTY input");
    }

    pub fn wait_text(&self, expected: &str) -> String {
        wait(
            &format!("{expected:?} in PTY capture"),
            || fs::read_to_string(&self.capture).is_ok_and(|text| text.contains(expected)),
            || fs::read_to_string(&self.capture).unwrap_or_default(),
        );
        fs::read_to_string(&self.capture).unwrap()
    }

    pub fn capture_len(&self) -> u64 {
        fs::metadata(&self.capture).map_or(0, |metadata| metadata.len())
    }

    pub fn wait_growth(&self, previous: u64) {
        wait(
            "PTY capture to grow after key input",
            || self.capture_len() > previous,
            || fs::read_to_string(&self.capture).unwrap_or_default(),
        );
    }

    pub fn wait_appended_frame(&self, previous: u64, expected: &str) -> String {
        const FRAME_END: &str = "\u{1b}[?25l";
        let previous = usize::try_from(previous).expect("PTY capture length fits usize");
        wait(
            &format!("complete appended frame containing {expected:?}"),
            || {
                fs::read(&self.capture).is_ok_and(|bytes| {
                    bytes
                        .get(previous..)
                        .map(String::from_utf8_lossy)
                        .and_then(|tail| {
                            tail.find(expected)
                                .map(|text| tail[text + expected.len()..].contains(FRAME_END))
                        })
                        .unwrap_or(false)
                })
            },
            || fs::read_to_string(&self.capture).unwrap_or_default(),
        );
        let bytes = fs::read(&self.capture).expect("read PTY capture after complete frame");
        String::from_utf8_lossy(&bytes[previous..]).into_owned()
    }

    pub fn wait_exit(&mut self) -> std::process::ExitStatus {
        let mut status = None;
        wait(
            "packaged binary to exit",
            || {
                status = self.child.try_wait().expect("poll packaged binary");
                status.is_some()
            },
            || fs::read_to_string(&self.capture).unwrap_or_default(),
        );
        status.unwrap()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

pub fn wait(
    description: &str,
    mut condition: impl FnMut() -> bool,
    diagnostics: impl Fn() -> String,
) {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {description}; {}", diagnostics());
}

pub fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
