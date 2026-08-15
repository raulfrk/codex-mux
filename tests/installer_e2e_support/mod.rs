use std::{
    ffi::OsStr,
    fs,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Scratch {
    root: PathBuf,
}

impl Scratch {
    pub fn new(_label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock predates Unix epoch")
            .as_nanos();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("cmux-pkg-{}-{nonce:x}-{id}", std::process::id()));
        fs::create_dir(&root).expect("create packaged installer scratch directory");
        Self { root }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn join(&self, path: impl AsRef<Path>) -> PathBuf {
        self.root.join(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let safe = self
            .root
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("cmux-pkg-"));
        if !safe {
            if thread::panicking() {
                return;
            }
            panic!("refusing to clean unexpected E2E path");
        }
        let result = fs::remove_dir_all(&self.root);
        if thread::panicking() {
            return;
        }
        result.unwrap_or_else(|error| {
            panic!(
                "remove packaged installer scratch {}: {error}",
                self.root.display()
            )
        });
        assert!(
            !self.root.exists(),
            "packaged installer scratch still exists after cleanup: {}",
            self.root.display()
        );
    }
}

pub fn packaged_binary() -> Option<PathBuf> {
    let Some(path) = std::env::var_os("CODEX_MUX_E2E_BINARY").map(PathBuf::from) else {
        eprintln!(
            "CODEX_MUX_E2E_BINARY is unset; skipping packaged installer E2E (scripts/e2e.sh supplies it)"
        );
        return None;
    };
    assert!(path.is_absolute(), "CODEX_MUX_E2E_BINARY must be absolute");
    let metadata = fs::metadata(&path).unwrap_or_else(|error| {
        panic!(
            "CODEX_MUX_E2E_BINARY {} cannot be inspected: {error}",
            path.display()
        )
    });
    assert!(metadata.is_file(), "E2E binary is not a regular file");
    assert_ne!(
        metadata.permissions().mode() & 0o111,
        0,
        "E2E binary is not executable"
    );
    Some(
        path.canonicalize()
            .expect("canonicalize CODEX_MUX_E2E_BINARY"),
    )
}

pub fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write executable fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("make fixture executable");
}

pub fn fake_codex(scratch: &Scratch, name: &str) -> PathBuf {
    let path = scratch.join(name);
    write_executable(&path, "#!/bin/sh\nexit 0\n");
    path
}

pub fn write_fake_tmux(directory: &Path, body: &str) -> PathBuf {
    fs::create_dir_all(directory).expect("create fake tmux PATH");
    let path = directory.join("tmux");
    write_executable(&path, body);
    path
}

pub fn run_packaged<I, S>(
    binary: &Path,
    cwd: &Path,
    path: &Path,
    home: Option<&Path>,
    extra_environment: &[(&str, &OsStr)],
    arguments: I,
) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(binary);
    command
        .current_dir(cwd)
        .env_remove("TMUX")
        .env_remove("HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env("PATH", path)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(home) = home {
        command.env("HOME", home);
    }
    for (key, value) in extra_environment {
        command.env(key, value);
    }

    output_with_timeout(&mut command, "packaged codex-mux command")
}

fn output_with_timeout(command: &mut Command, operation: &str) -> Output {
    try_output_with_timeout(command, operation).unwrap_or_else(|error| panic!("{error}"))
}

fn try_output_with_timeout(command: &mut Command, operation: &str) -> Result<Output, String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("start {operation}: {error}"))?;
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        if child
            .try_wait()
            .map_err(|error| format!("poll {operation}: {error}"))?
            .is_some()
        {
            return child
                .wait_with_output()
                .map_err(|error| format!("collect {operation} output: {error}"));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .map_err(|error| format!("collect timed-out {operation} output: {error}"))?;
            return Err(format!(
                "{operation} exceeded {:?}: stdout={} stderr={}",
                COMMAND_TIMEOUT,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
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

pub fn assert_failure(output: &Output, expected_stderr: &str, operation: &str) {
    assert!(
        !output.status.success(),
        "{operation} unexpectedly succeeded"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected_stderr),
        "{operation} stderr omitted {expected_stderr:?}: {stderr}"
    );
}

pub fn backups_below(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    collect_backups(root, &mut found);
    found.sort();
    found
}

fn collect_backups(root: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("inspect scratch entry").path();
        if path.is_dir() {
            collect_backups(&path, found);
        } else if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().contains("codex-mux.bak"))
        {
            found.push(path);
        }
    }
}

pub struct RealTmux {
    executable: PathBuf,
    socket: String,
    socket_path: PathBuf,
    tmux_tmpdir: PathBuf,
    wrapper_dir: PathBuf,
}

impl RealTmux {
    pub fn start(scratch: &Scratch, config: &Path) -> Option<Self> {
        let executable = ["/usr/bin/tmux", "/bin/tmux"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file());
        let Some(executable) = executable else {
            eprintln!("tmux is unavailable; skipping real-server packaged installer E2E");
            return None;
        };
        let socket = format!(
            "cmux-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let tmux_tmpdir = scratch.join("tmux-tmp");
        fs::create_dir(&tmux_tmpdir).expect("create isolated TMUX_TMPDIR");
        fs::set_permissions(&tmux_tmpdir, fs::Permissions::from_mode(0o700))
            .expect("protect isolated TMUX_TMPDIR");
        let wrapper_dir = scratch.join("real-tmux-path");
        let script = format!(
            "#!/bin/sh\nunset TMUX\nexec '{}' -L '{}' \"$@\"\n",
            executable.display(),
            socket
        );
        write_fake_tmux(&wrapper_dir, &script);

        let mut command = Command::new(&executable);
        command
            .env_remove("TMUX")
            .env("TMUX_TMPDIR", &tmux_tmpdir)
            .args(["-L", &socket, "-f"])
            .arg(config)
            .args(["new-session", "-d", "-s", "packaged-e2e"]);
        let output = output_with_timeout(&mut command, "isolated tmux server startup");
        assert_success(&output, "start isolated tmux server");
        let mut server = Self {
            executable,
            socket,
            socket_path: PathBuf::new(),
            tmux_tmpdir,
            wrapper_dir,
        };
        let socket_output = server.run(&["display-message", "-p", "#{socket_path}"]);
        assert_success(&socket_output, "inspect isolated tmux socket path");
        server.socket_path = PathBuf::from(
            String::from_utf8(socket_output.stdout)
                .expect("tmux socket path must be UTF-8")
                .trim(),
        );
        assert!(
            server.socket_path.is_absolute() && server.socket_path.exists(),
            "isolated tmux socket is not an existing absolute path: {}",
            server.socket_path.display()
        );
        Some(server)
    }

    pub fn path(&self) -> &Path {
        &self.wrapper_dir
    }

    pub fn environment(&self) -> [(&str, &OsStr); 1] {
        [("TMUX_TMPDIR", self.tmux_tmpdir.as_os_str())]
    }

    pub fn run(&self, arguments: &[&str]) -> Output {
        self.command(arguments, "isolated tmux command")
    }

    fn command(&self, arguments: &[&str], operation: &str) -> Output {
        self.try_command(arguments, operation)
            .unwrap_or_else(|error| panic!("{error}"))
    }

    fn try_command(&self, arguments: &[&str], operation: &str) -> Result<Output, String> {
        let mut command = Command::new(&self.executable);
        command
            .env_remove("TMUX")
            .env("TMUX_TMPDIR", &self.tmux_tmpdir)
            .args(["-L", &self.socket])
            .args(arguments);
        try_output_with_timeout(&mut command, operation)
    }

    fn shutdown(&self) -> Result<(), String> {
        let killed = self.try_command(&["kill-server"], "isolated tmux server shutdown")?;
        if !killed.status.success() {
            return Err(format!(
                "kill isolated tmux server failed ({}): stdout={} stderr={}",
                killed.status,
                String::from_utf8_lossy(&killed.stdout),
                String::from_utf8_lossy(&killed.stderr)
            ));
        }

        let deadline = Instant::now() + COMMAND_TIMEOUT;
        loop {
            let probe = self.try_command(
                &["has-session", "-t", "packaged-e2e"],
                "isolated tmux shutdown probe",
            )?;
            let (socket_absent_or_dead, socket_state) =
                match fs::symlink_metadata(&self.socket_path) {
                    Ok(metadata) if metadata.file_type().is_socket() => {
                        (true, "dead socket pathname")
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        (true, "absent pathname")
                    }
                    Ok(_) => (false, "unexpected non-socket pathname"),
                    Err(_) => (false, "uninspectable pathname"),
                };
            if !probe.status.success() && socket_absent_or_dead {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "isolated tmux server remained reachable or had unsafe socket state at {} after kill: socket_state={socket_state} probe_status={} probe_stderr={}",
                    self.socket_path.display(),
                    probe.status,
                    String::from_utf8_lossy(&probe.stderr)
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for RealTmux {
    fn drop(&mut self) {
        let result = self.shutdown();
        if thread::panicking() {
            return;
        }
        if let Err(error) = result {
            panic!("{error}");
        }
    }
}
