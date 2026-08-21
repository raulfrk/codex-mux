//! Privacy-bounded conversation extraction and Codex app-server naming contract.

use std::{
    collections::{HashMap, HashSet, VecDeque, hash_map::DefaultHasher},
    env, fs,
    hash::{Hash, Hasher},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::{
    fd::{AsRawFd, OwnedFd},
    unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt},
    },
};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::domain::{CodexExecutable, Pane, PaneId};
use crate::{MuxError, Result};

/// Codex model used exclusively for background session naming.
pub const NAMING_MODEL: &str = "gpt-5.6-luna";
/// Maximum UTF-8 payload sent to the naming model.
pub const MAX_CONVERSATION_BYTES: usize = 12 * 1024;
/// Maximum accepted generated title length in Unicode scalar values.
pub const MAX_NAME_CHARS: usize = 48;
const THREAD_LIST_PAGE_SIZE: u32 = 100;
const MAX_THREAD_LIST_PAGES: usize = 20;
const THREAD_TURNS_PAGE_SIZE: u32 = 100;
const MAX_THREAD_TURNS_PAGES: usize = 100;
const MAX_APP_SERVER_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_ROLLOUT_ENTRIES: usize = 100_000;
const MAX_ROLLOUT_DEPTH: usize = 8;
const MAX_ROLLOUT_IDENTITY_BYTES: u64 = 1024 * 1024;
const MAX_ROLLOUT_TRANSCRIPT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_HISTORY_REQUESTS: usize = 64;
const MAX_HISTORY_ENTRIES: usize = MAX_HISTORY_REQUESTS * THREAD_TURNS_PAGE_SIZE as usize;
const MAX_HISTORY_ID_BYTES: usize = 128;
const MAX_CURSOR_BYTES: usize = 4096;
const MAX_PROCESS_ENTRIES: usize = 100_000;
const MAX_PROCESS_FDS: usize = 512;
const MAX_PROCESS_SCAN_FDS: usize = 8_192;
const MAX_PROCESS_SCAN_BYTES: u64 = 32 * 1024 * 1024;
/// Interval between reconsidering an existing smart title.
pub const NAMING_REFRESH_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Cooldown before retrying a failed naming attempt or restarting an unhealthy provider.
const NAMING_RETRY_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Short retry while a resumed thread is still exposing its first completed turn.
const PENDING_NAMING_RETRY_INTERVAL: Duration = Duration::from_secs(2);
type AppServerMessage = std::result::Result<Value, String>;
const NAMING_LOG_MAX_BYTES: u64 = 256 * 1024;
const DIAGNOSTIC_CODES: &[&str] = &[
    "worker_start",
    "provider_start_failed",
    "provider_ready",
    "discovery_failed",
    "read_provider_unhealthy",
    "read_failed",
    "conversation_pending",
    "naming_provider_unhealthy",
    "naming_failed",
    "resolve_provider_unhealthy",
    "resolve_failed",
    "thread_state_db_miss",
    "thread_archive_cross_check",
    "thread_resolve_not_found",
    "thread_rollout_resolved",
    "thread_rollout_ambiguous",
    "thread_turns_read",
    "thread_rollout_read",
    "identity_changed",
    "name_published",
    "process_rejected",
    "pane_command_rejected",
    "pane_mode_active",
    "cursor_hidden",
    "state_read_failed",
    "composer_row_rejected",
    "composer_cursor_rejected",
    "state_recheck_failed",
    "state_changed_after_left",
    "process_changed_after_left",
    "popup_opened",
];

/// Privacy-safe, bounded operational diagnostics for Smart Naming.
#[derive(Clone, Debug)]
pub struct NamingDiagnostics {
    path: PathBuf,
}

impl NamingDiagnostics {
    /// Discovers the standard XDG state log path without creating it.
    pub fn discover() -> Result<Self> {
        let root = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
            .ok_or_else(|| MuxError::Command("HOME and XDG_STATE_HOME are unset".to_owned()))?;
        Ok(Self {
            path: root.join("codex-mux/smart-naming.log"),
        })
    }

    /// Discovers the Smart Left decision log alongside Smart Naming diagnostics.
    pub fn smart_left() -> Result<Self> {
        let mut diagnostics = Self::discover()?;
        diagnostics.path.set_file_name("smart-left.log");
        Ok(diagnostics)
    }

    /// Uses an explicit diagnostics path, primarily for embedding and tests.
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Exact diagnostics file displayed by status.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends a fixed reason code. Session and provider content cannot enter this API.
    pub fn event(&self, code: &'static str) {
        if !DIAGNOSTIC_CODES.contains(&code) {
            return;
        }
        let Some(parent) = self.path.parent() else {
            return;
        };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() >= NAMING_LOG_MAX_BYTES)
            && fs::rename(&self.path, self.path.with_extension("log.1")).is_err()
        {
            return;
        }
        let mut options = fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
            options.custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            );
        }
        if let Ok(mut file) = options.open(&self.path) {
            if !file
                .metadata()
                .is_ok_and(|metadata| safe_diagnostics_metadata(&metadata))
            {
                return;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
            }
            let _ = writeln!(file, "{} {code}", unix_seconds(SystemTime::now()));
        }
    }

    /// Reads the most recent sanitized event.
    #[must_use]
    pub fn latest(&self) -> Option<String> {
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            );
        }
        let file = options.open(&self.path).ok()?;
        let metadata = file.metadata().ok()?;
        if !safe_diagnostics_metadata(&metadata) || metadata.len() > NAMING_LOG_MAX_BYTES {
            return None;
        }
        let mut contents = String::new();
        file.take(NAMING_LOG_MAX_BYTES + 1)
            .read_to_string(&mut contents)
            .ok()?;
        let line = contents.lines().next_back()?;
        let mut fields = line.split(' ');
        let timestamp = fields.next()?;
        let code = fields.next()?;
        if fields.next().is_some()
            || timestamp.is_empty()
            || !timestamp.bytes().all(|byte| byte.is_ascii_digit())
            || !DIAGNOSTIC_CODES.contains(&code)
        {
            return None;
        }
        Some(format!("{timestamp} {code}"))
    }
}

fn safe_diagnostics_metadata(metadata: &fs::Metadata) -> bool {
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.uid() == rustix::process::geteuid().as_raw() && metadata.nlink() == 1
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn log_diagnostic(diagnostics: &Option<NamingDiagnostics>, code: &'static str) {
    if let Some(diagnostics) = diagnostics {
        diagnostics.event(code);
    }
}

/// Lazily constructs naming infrastructure only after an explicit opt-in.
///
/// Keeping construction behind this gate makes the default-off privacy
/// contract testable: disabled mode cannot spawn app-server or issue requests.
pub fn start_if_enabled<T>(enabled: bool, start: impl FnOnce() -> Result<T>) -> Result<Option<T>> {
    enabled.then(start).transpose()
}

/// Managed local Codex app-server process used by the naming worker.
pub struct AppServerProcess {
    child: Child,
    stdin: ChildStdin,
    messages: Option<Receiver<AppServerMessage>>,
    pending: VecDeque<Value>,
    reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    healthy: Arc<AtomicBool>,
    next_id: u64,
    cancelled: Arc<AtomicBool>,
}

impl AppServerProcess {
    /// Starts app-server with local stdio and the user's existing Codex authentication.
    pub fn spawn(codex: &Path) -> Result<Self> {
        Self::spawn_with_cancel(codex, Arc::new(AtomicBool::new(false)))
    }

    /// Starts app-server with a cancellation flag owned by its worker.
    pub fn spawn_with_cancel(codex: &Path, cancelled: Arc<AtomicBool>) -> Result<Self> {
        let mut child = app_server_command(codex)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| MuxError::Filesystem {
                path: codex.to_owned(),
                source,
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| protocol("app-server stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| protocol("app-server stdout unavailable"))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| protocol("app-server stderr unavailable"))?;
        let (sender, messages) = mpsc::sync_channel(64);
        let healthy = Arc::new(AtomicBool::new(true));
        let reader_healthy = healthy.clone();
        let reader = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(message) = read_app_server_message(&mut reader) {
                if !forward_app_server_message(&sender, &reader_healthy, message) {
                    break;
                }
            }
            reader_healthy.store(false, Ordering::Release);
        });
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let captured = stderr.clone();
        let stderr_reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr_pipe.take(4096).read_to_end(&mut bytes);
            *captured.lock().unwrap() = bytes;
        });
        let mut process = Self {
            child,
            stdin,
            messages: Some(messages),
            pending: VecDeque::new(),
            reader: Some(reader),
            stderr_reader: Some(stderr_reader),
            stderr,
            healthy,
            next_id: 1,
            cancelled,
        };
        process.initialize()?;
        Ok(process)
    }

    fn initialize(&mut self) -> Result<()> {
        let id = self.send_request("initialize", json!({
            "clientInfo": {"name": "codex-mux", "title": "codex-mux smart naming", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"experimentalApi": true}
        }))?;
        let response =
            self.receive_matching(Duration::from_secs(15), |message| message["id"] == id)?;
        response_result(response)?;
        self.write_message(&json!({"method": "initialized"}))
    }

    fn send_request(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_message(&json!({"id": id, "method": method, "params": params}))?;
        Ok(id)
    }

    fn write_message(&mut self, message: &Value) -> Result<()> {
        let mut encoded = serde_json::to_vec(message)
            .map_err(|error| protocol(&format!("could not encode request: {error}")))?;
        encoded.push(b'\n');
        self.stdin
            .write_all(&encoded)
            .and_then(|()| self.stdin.flush())
            .map_err(|source| {
                self.healthy.store(false, Ordering::Release);
                MuxError::Filesystem {
                    path: Path::new("codex app-server stdin").to_owned(),
                    source,
                }
            })
    }

    fn receive_matching(
        &mut self,
        timeout: Duration,
        matches: impl Fn(&Value) -> bool,
    ) -> Result<Value> {
        if let Some(index) = self.pending.iter().position(&matches) {
            return Ok(self.pending.remove(index).expect("pending index exists"));
        }
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return Err(MuxError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let wait = remaining.min(Duration::from_millis(100));
            let message = self
                .messages
                .as_ref()
                .expect("receiver exists")
                .recv_timeout(wait);
            let message = match message {
                Ok(Ok(message)) => message,
                Ok(Err(detail)) => return Err(protocol(&detail)),
                Err(mpsc::RecvTimeoutError::Timeout) if std::time::Instant::now() < deadline => {
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.healthy.store(false, Ordering::Release);
                    return Err(protocol("app-server readiness timed out"));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.healthy.store(false, Ordering::Release);
                    return Err({
                        let detail = String::from_utf8_lossy(&self.stderr.lock().unwrap())
                            .trim()
                            .to_owned();
                        protocol(if detail.is_empty() {
                            "app-server output closed unexpectedly"
                        } else {
                            &detail
                        })
                    });
                }
            };
            if matches(&message) {
                return Ok(message);
            }
            if should_retain(&message) {
                retain_pending(&mut self.pending, message)?;
            }
        }
    }
}

fn app_server_command(codex: &Path) -> Command {
    let mut command = Command::new(codex);
    command.args(["app-server", "--listen", "stdio://"]);
    command
}

fn forward_app_server_message(
    sender: &mpsc::SyncSender<AppServerMessage>,
    healthy: &AtomicBool,
    message: AppServerMessage,
) -> bool {
    let fatal = message.is_err();
    if fatal {
        healthy.store(false, Ordering::Release);
    }
    sender.send(message).is_ok() && !fatal
}

fn read_app_server_message(reader: &mut impl BufRead) -> Option<AppServerMessage> {
    let mut line = Vec::new();
    let read = match reader
        .take((MAX_APP_SERVER_MESSAGE_BYTES + 1) as u64)
        .read_until(b'\n', &mut line)
    {
        Ok(read) => read,
        Err(error) => return Some(Err(format!("could not read app-server output: {error}"))),
    };
    if read == 0 {
        return None;
    }
    if line.len() > MAX_APP_SERVER_MESSAGE_BYTES {
        return Some(Err(format!(
            "app-server message exceeded {MAX_APP_SERVER_MESSAGE_BYTES} bytes"
        )));
    }
    if !line.ends_with(b"\n") {
        return Some(Err("app-server output ended mid-message".to_owned()));
    }
    Some(
        serde_json::from_slice(&line)
            .map_err(|error| format!("app-server returned invalid JSON: {error}")),
    )
}

impl AppServerSession for AppServerProcess {
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.send_request(method, params)?;
        let response =
            self.receive_matching(Duration::from_secs(30), |message| message["id"] == id)?;
        response_result(response)
    }

    fn wait_for(&mut self, method: &str, thread_id: &str) -> Result<Value> {
        let message = self.receive_matching(Duration::from_secs(30), |message| {
            message["method"] == method
                && message.pointer("/params/threadId").and_then(Value::as_str) == Some(thread_id)
        })?;
        self.pending.retain(|pending| {
            pending.pointer("/params/threadId").and_then(Value::as_str) != Some(thread_id)
        });
        message
            .get("params")
            .cloned()
            .ok_or_else(|| protocol("notification omitted params"))
    }

    fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }
}

fn should_retain(message: &Value) -> bool {
    let response = message.get("id").is_some()
        && message.get("method").is_none()
        && (message.get("result").is_some() || message.get("error").is_some());
    response || message["method"] == "turn/completed"
}

fn retain_pending(pending: &mut VecDeque<Value>, message: Value) -> Result<()> {
    if pending.len() == 64 {
        return Err(protocol(
            "app-server pending message queue exceeded its bound",
        ));
    }
    pending.push_back(message);
    Ok(())
}

fn response_result(mut response: Value) -> Result<Value> {
    if let Some(error) = response.get("error") {
        return Err(protocol(&format!("app-server rejected request: {error}")));
    }
    response
        .get_mut("result")
        .map(Value::take)
        .ok_or_else(|| protocol("app-server response omitted result"))
}

impl Drop for AppServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.messages.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
    }
}

/// Synchronous request/notification seam implemented by the managed app-server process.
pub trait AppServerSession {
    /// Sends one JSON-RPC request and returns its `result` value.
    fn request(&mut self, method: &str, params: Value) -> Result<Value>;
    /// Waits for a notification matching the method and naming thread identifier.
    fn wait_for(&mut self, method: &str, thread_id: &str) -> Result<Value>;
    /// Reports whether the transport can serve another request.
    fn is_healthy(&self) -> bool {
        true
    }
}

/// A completed, privacy-bounded conversation ready for naming.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamingConversation {
    /// Stable source Codex thread identifier.
    pub thread_id: String,
    /// Bounded plain user/assistant transcript; never persisted by this crate.
    pub transcript: String,
}

/// Stable pane/thread identity consumed by the background worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamingTarget {
    /// Exact tmux pane identity.
    pub pane_id: PaneId,
    /// Exact pane title observed during discovery.
    pub pane_title: String,
    /// Full UUID or unambiguous UUID prefix exposed in the pane title.
    pub thread_hint: String,
    /// Exact working directory used to constrain app-server resolution.
    pub cwd: PathBuf,
    /// Existing pane-local Codex Mux title, when one is valid.
    pub generated_name: Option<String>,
    /// Unix timestamp of the last successful title generation.
    pub generated_at_unix: Option<u64>,
    /// A pane-local Resume request that should bypass a prior successful refresh cooldown.
    pub immediate_naming: bool,
}

impl NamingTarget {
    /// Extracts a target only from the supported UUID thread-title shape.
    #[must_use]
    pub fn from_pane(pane: &Pane) -> Option<Self> {
        if pane.manual_name {
            return None;
        }
        let observed_title = pane.title.as_deref()?;
        if pane.unpin_waiting
            && (pane.unpin_waiting_title.as_deref() == Some(observed_title)
                || pane.unpin_waiting_pid != Some(pane.pane_pid)
                || pane.unpin_waiting_session.as_ref() != Some(&pane.session_id))
        {
            return None;
        }
        let pane_title = observed_title.trim();
        let thread_hint = thread_hint(pane_title)?;
        Some(Self {
            pane_id: pane.id.clone(),
            pane_title: pane_title.to_owned(),
            thread_hint: thread_hint.to_owned(),
            cwd: pane.current_path.clone(),
            generated_name: pane.generated_title.clone(),
            generated_at_unix: pane.generated_at_unix,
            immediate_naming: pane.immediate_naming,
        })
    }

    /// Builds a target from an independently verified exact thread identity.
    #[must_use]
    pub fn from_verified_thread(pane: &Pane, thread_id: String) -> Option<Self> {
        if pane.manual_name || !looks_like_thread_id(&thread_id) {
            return None;
        }
        let pane_title = pane.title.as_deref()?.trim().to_owned();
        Some(Self {
            pane_id: pane.id.clone(),
            pane_title,
            thread_hint: thread_id,
            cwd: pane.current_path.clone(),
            generated_name: pane.generated_title.clone(),
            generated_at_unix: pane.generated_at_unix,
            immediate_naming: pane.immediate_naming,
        })
    }
}

/// Verified Codex shell-snapshot identity source for panes whose visible title
/// is not itself a thread identifier.
#[derive(Clone, Debug)]
#[cfg(test)]
pub struct ShellSnapshotStore {
    root: PathBuf,
}

/// Exact root-session identity inherited by model-spawned shell/tool processes.
#[derive(Clone, Copy, Debug, Default)]
#[cfg(test)]
pub struct ProcessEnvironmentStore;

#[cfg(test)]
impl ProcessEnvironmentStore {
    /// Resolves one exact `CODEX_SESSION_ID` from readable descendants of the
    /// tmux pane process whose inherited `TMUX_PANE` still names this pane.
    pub fn resolve(self, pane: &Pane) -> Result<Option<String>> {
        let first = process_environment_threads(pane)?;
        let second = process_environment_threads(pane)?;
        if first != second {
            return Ok(None);
        }
        match first.len() {
            0 => Ok(None),
            1 => Ok(first.into_iter().next()),
            _ => Err(protocol(
                "live pane descendants expose multiple Codex session IDs",
            )),
        }
    }
}

#[cfg(test)]
fn process_environment_threads(pane: &Pane) -> Result<HashSet<String>> {
    let mut processes = HashMap::new();
    let directory = fs::read_dir("/proc").map_err(|source| MuxError::Filesystem {
        path: PathBuf::from("/proc"),
        source,
    })?;
    for (index, entry) in directory.enumerate() {
        if index >= 100_000 {
            return Err(protocol(
                "process environment discovery exceeded its entry limit",
            ));
        }
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        if let Ok(identity) = process_stat_identity(pid) {
            processes.insert(pid, identity);
        }
    }
    let Some(root_identity) = processes.get(&pane.pane_pid).copied() else {
        return Ok(HashSet::new());
    };
    let parents = processes
        .iter()
        .map(|(&pid, &(parent, _))| (pid, parent))
        .collect::<HashMap<_, _>>();
    let mut matched = HashSet::new();
    for (&pid, &identity) in &processes {
        if pid == pane.pane_pid || !descends_from(pid, pane.pane_pid, &parents) {
            continue;
        }
        let Some((observed_pane, thread_id)) = process_identity_environment(pid)? else {
            continue;
        };
        if observed_pane != pane.id.as_str() || !looks_like_thread_id(&thread_id) {
            continue;
        }
        // Re-read the candidate's current parent and prove its current chain
        // against the same bounded snapshot before accepting its environment.
        let current_identity = match process_stat_identity(pid) {
            Ok(identity) => identity,
            Err(_) => continue,
        };
        if identity != current_identity || !descends_from(pid, pane.pane_pid, &parents) {
            continue;
        }
        matched.insert(thread_id);
    }
    if process_stat_identity(pane.pane_pid).ok() != Some(root_identity) {
        return Ok(HashSet::new());
    }
    Ok(matched)
}

fn process_stat_identity(pid: u32) -> Result<(u32, u64)> {
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    let stat = fs::read_to_string(&path).map_err(|source| MuxError::Filesystem { path, source })?;
    let fields = stat
        .rsplit_once(") ")
        .map(|(_, fields)| fields)
        .ok_or_else(|| protocol("process stat is malformed"))?;
    let mut fields = fields.split_whitespace();
    let parent = fields
        .nth(1)
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| protocol("process parent is malformed"))?;
    let started = fields
        .nth(17)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| protocol("process start time is malformed"))?;
    Ok((parent, started))
}

fn descends_from(mut pid: u32, root: u32, parents: &HashMap<u32, u32>) -> bool {
    let mut visited = HashSet::new();
    for _ in 0..128 {
        if pid == root {
            return true;
        }
        if !visited.insert(pid) {
            return false;
        }
        let Some(parent) = parents.get(&pid).copied() else {
            return false;
        };
        if parent == 0 || parent == pid {
            return false;
        }
        pid = parent;
    }
    false
}

fn revalidate_descendant(
    mut pid: u32,
    identity: (u32, u64),
    root: u32,
    root_identity: (u32, u64),
    snapshot: &HashMap<u32, (u32, u64)>,
) -> bool {
    let mut expected = identity;
    let mut visited = HashSet::new();
    while pid != root {
        if !visited.insert(pid)
            || snapshot.get(&pid).copied() != Some(expected)
            || process_stat_identity(pid).ok() != Some(expected)
        {
            return false;
        }
        pid = expected.0;
        let Some(next) = snapshot.get(&pid).copied() else {
            return false;
        };
        expected = next;
    }
    expected == root_identity && process_stat_identity(root).ok() == Some(root_identity)
}

#[cfg(test)]
fn process_identity_environment(pid: u32) -> Result<Option<(String, String)>> {
    let path = PathBuf::from(format!("/proc/{pid}/environ"));
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(None);
        }
        Err(source) => return Err(MuxError::Filesystem { path, source }),
    };
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| MuxError::Filesystem {
            path: path.clone(),
            source,
        })?;
    if bytes.len() > 1024 * 1024 {
        return Err(protocol("process environment exceeded its byte limit"));
    }
    let mut pane = None;
    let mut session = None;
    for field in bytes.split(|byte| *byte == 0) {
        if let Some(value) = field.strip_prefix(b"TMUX_PANE=") {
            pane = std::str::from_utf8(value).ok().map(ToOwned::to_owned);
        } else if let Some(value) = field.strip_prefix(b"CODEX_SESSION_ID=") {
            session = std::str::from_utf8(value).ok().map(ToOwned::to_owned);
        }
    }
    Ok(pane.zip(session))
}

#[cfg(test)]
impl ShellSnapshotStore {
    /// Discovers the snapshot directory from the daemon's Codex home.
    pub fn discover() -> Result<Self> {
        let root = env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
            .ok_or_else(|| protocol("Codex home is unavailable"))?
            .join("shell_snapshots");
        Ok(Self { root })
    }

    #[cfg(test)]
    /// Constructs a store at an explicit directory for deterministic tests.
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// Returns one exact root-session UUID only when all matching snapshot
    /// evidence for the live pane agrees.
    pub fn resolve(&self, pane: &Pane) -> Result<Option<String>> {
        #[cfg(unix)]
        use rustix::fs::{Dir, FileType, Mode, OFlags, open, openat};

        let process_started = linux_process_started_unix(pane.pane_pid)?;
        let directory = match open(
            &self.root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(directory) => directory,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(error) => {
                return Err(protocol(&format!(
                    "cannot open shell snapshot directory: {error}"
                )));
            }
        };
        let directory_file = fs::File::from(directory);
        validate_snapshot_directory(&directory_file)?;
        let directory: OwnedFd = directory_file.into();
        let entries = Dir::read_from(&directory)
            .map_err(|error| protocol(&format!("cannot read shell snapshot directory: {error}")))?;
        let mut matched: Option<String> = None;
        for (index, entry) in entries.enumerate() {
            if index >= 10_000 {
                return Err(protocol(
                    "shell snapshot discovery exceeded its entry limit",
                ));
            }
            let entry = entry.map_err(|error| {
                protocol(&format!("cannot read a shell snapshot entry: {error}"))
            })?;
            if entry.file_type() != FileType::RegularFile {
                continue;
            }
            let name = entry.file_name();
            let path = self.root.join(std::ffi::OsStr::from_bytes(name.to_bytes()));
            let Some(thread_id) = snapshot_thread_id(&path) else {
                continue;
            };
            let file = openat(
                &directory,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| protocol(&format!("cannot open shell snapshot: {error}")))?;
            let file = fs::File::from(file);
            let metadata = file.metadata().map_err(|source| MuxError::Filesystem {
                path: path.clone(),
                source,
            })?;
            if !safe_snapshot_metadata(&metadata, process_started) {
                continue;
            }
            let mut observed_pane = None;
            for line in BufReader::new(file).lines().take(20_000) {
                let line = line.map_err(|source| MuxError::Filesystem {
                    path: path.clone(),
                    source,
                })?;
                if let Some(value) = line.strip_prefix("export TMUX_PANE=") {
                    observed_pane = shell_snapshot_literal(value);
                    break;
                }
            }
            if observed_pane.as_deref() != Some(pane.id.as_str()) {
                continue;
            }
            if matched
                .as_deref()
                .is_some_and(|existing| existing != thread_id)
            {
                return Err(protocol(
                    "live pane matches multiple shell snapshot threads",
                ));
            }
            matched = Some(thread_id.to_owned());
        }
        Ok(matched)
    }
}

#[cfg(unix)]
#[cfg(test)]
fn validate_snapshot_directory(directory: &fs::File) -> Result<()> {
    let metadata = directory
        .metadata()
        .map_err(|source| MuxError::Filesystem {
            path: PathBuf::from("shell_snapshots"),
            source,
        })?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o022 != 0
    {
        return Err(protocol("shell snapshot directory is not trusted"));
    }
    Ok(())
}

#[cfg(unix)]
#[cfg(test)]
fn safe_snapshot_metadata(metadata: &fs::Metadata, process_started: u64) -> bool {
    metadata.is_file()
        && metadata.uid() == rustix::process::geteuid().as_raw()
        && metadata.nlink() == 1
        && metadata.mode() & 0o022 == 0
        && metadata.len() <= 1024 * 1024
        && !metadata.mtime().is_negative()
        && metadata.mtime() as u64 >= process_started
}

#[cfg(test)]
fn snapshot_thread_id(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let thread = name.split_once('.')?.0;
    looks_like_thread_id(thread).then_some(thread)
}

#[cfg(test)]
fn shell_snapshot_literal(value: &str) -> Option<String> {
    let value = value.trim();
    if let Some(value) = value
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
    {
        return (!value.contains('\'')).then(|| value.to_owned());
    }
    value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '%' | '_' | '-'))
        .then(|| value.to_owned())
}

#[cfg(test)]
fn linux_process_started_unix(pid: u32) -> Result<u64> {
    let stat_path = PathBuf::from(format!("/proc/{pid}/stat"));
    let stat = fs::read_to_string(&stat_path).map_err(|source| MuxError::Filesystem {
        path: stat_path,
        source,
    })?;
    let after_comm = stat
        .rsplit_once(") ")
        .map(|(_, fields)| fields)
        .ok_or_else(|| protocol("pane process stat is malformed"))?;
    let start_ticks = after_comm
        .split_whitespace()
        .nth(19)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| protocol("pane process start time is malformed"))?;
    let proc_stat = fs::read_to_string("/proc/stat").map_err(|source| MuxError::Filesystem {
        path: PathBuf::from("/proc/stat"),
        source,
    })?;
    let boot = proc_stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| protocol("system boot time is unavailable"))?;
    let ticks = rustix::param::clock_ticks_per_second();
    Ok(boot.saturating_add(start_ticks / ticks))
}

pub(crate) fn thread_hint(title: &str) -> Option<&str> {
    if looks_like_thread_id(title) {
        return Some(title);
    }
    let prefix = title.strip_suffix("...")?;
    (prefix.chars().filter(|character| *character != '-').count() >= 12
        && prefix.len() < 36
        && prefix
            .chars()
            .all(|character| character.is_ascii_hexdigit() || character == '-'))
    .then_some(prefix)
}

/// Provider seam used by the asynchronous naming worker.
pub trait ConversationNamer: Send + 'static {
    /// Resolves the target's current authoritative full thread identity.
    fn resolve(&mut self, target: &NamingTarget) -> Result<String> {
        looks_like_thread_id(&target.thread_hint)
            .then(|| target.thread_hint.clone())
            .ok_or_else(|| protocol("conversation namer cannot resolve a truncated thread title"))
    }
    /// Reads bounded completed conversation content.
    fn read(&mut self, target: &NamingTarget) -> Result<NamingConversation>;
    /// Generates one validated title.
    fn name(&mut self, conversation: &NamingConversation) -> Result<String>;
    /// Reports whether the underlying provider can serve another request.
    fn is_healthy(&self) -> bool {
        true
    }
}

impl<S: AppServerSession + Send + 'static> ConversationNamer for AppServerNamer<S> {
    fn resolve(&mut self, target: &NamingTarget) -> Result<String> {
        self.resolve_thread_id(target)
    }

    fn read(&mut self, target: &NamingTarget) -> Result<NamingConversation> {
        let thread_id = self.resolve(target)?;
        self.read_with_fallback(&thread_id)
    }

    fn name(&mut self, conversation: &NamingConversation) -> Result<String> {
        self.generate_name(conversation)
    }

    fn is_healthy(&self) -> bool {
        self.session.is_healthy()
    }
}

/// A generated title bound to the source thread to prevent pane-ID reuse.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneratedName {
    /// Source Codex thread identity.
    pub thread_id: String,
    /// Exact pane title that was resolved to the source thread.
    pub source_title: String,
    /// Exact pane working directory used during thread resolution.
    pub source_cwd: PathBuf,
    /// Validated generated title.
    pub name: String,
    /// Unix timestamp persisted with the pane-local title.
    pub generated_at_unix: u64,
}

/// In-memory generated names published by the worker; conversation text is never stored here.
pub type GeneratedNames = Arc<Mutex<HashMap<PaneId, GeneratedName>>>;

/// One bounded serial background naming lane with clean stop/join semantics.
pub struct NamingWorker {
    commands: Option<Sender<NamingCommand>>,
    thread: Option<JoinHandle<()>>,
    names: GeneratedNames,
    cancelled: Arc<AtomicBool>,
    children: Vec<NamingWorker>,
}

enum NamingCommand {
    Stop,
    Wake,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DiscoveryPhase {
    Scan,
    Revalidate,
}

impl NamingWorker {
    /// Starts a bounded set of independent provider lanes while preserving one
    /// merged publication surface for the daemon coordinator.
    pub fn spawn_parallel_logged<N, F, D>(
        lanes: usize,
        start_namer: F,
        discover: D,
        poll_interval: Duration,
        diagnostics: Option<NamingDiagnostics>,
    ) -> Self
    where
        N: ConversationNamer,
        F: Fn(Arc<AtomicBool>) -> Result<N> + Send + Sync + 'static,
        D: FnMut(Arc<AtomicBool>) -> Result<Vec<NamingTarget>> + Send + 'static,
    {
        let lanes = lanes.clamp(1, 8);
        let start_namer = Arc::new(start_namer);
        let discover = Arc::new(Mutex::new(discover));
        let discovery_cancelled = Arc::new(AtomicBool::new(false));
        let discovery_cache = Arc::new(Mutex::new(None::<(Vec<NamingTarget>, HashSet<usize>)>));
        let children = (0..lanes)
            .map(|lane| {
                let start_namer = start_namer.clone();
                let discover = discover.clone();
                let diagnostics = diagnostics.clone();
                let discovery_cancelled = discovery_cancelled.clone();
                let discovery_cache = discovery_cache.clone();
                Self::spawn_logged_phased(
                    move |cancelled| start_namer(cancelled),
                    move |phase| {
                        if discovery_cancelled.load(Ordering::Acquire) {
                            return Err(MuxError::Cancelled);
                        }
                        let mut cache = discovery_cache.lock().unwrap();
                        let refresh = phase == DiscoveryPhase::Revalidate
                            || cache.as_ref().is_none_or(|(_, seen)| seen.len() == lanes);
                        if refresh {
                            let targets = discover.lock().unwrap()(discovery_cancelled.clone())?;
                            *cache = Some((targets, HashSet::new()));
                        }
                        let (targets, seen) = cache.as_mut().expect("discovery cache initialized");
                        seen.insert(lane);
                        Ok(targets
                            .iter()
                            .filter(|target| {
                                let mut hasher = DefaultHasher::new();
                                // Full UUIDs and legacy truncated UUID titles for the same
                                // conversation must enter the same lane so one provider owns
                                // their exact-ID deduplication and fanout.
                                target
                                    .thread_hint
                                    .bytes()
                                    .filter(|byte| *byte != b'-')
                                    .take(12)
                                    .for_each(|byte| byte.hash(&mut hasher));
                                hasher.finish() as usize % lanes == lane
                            })
                            .cloned()
                            .collect())
                    },
                    poll_interval,
                    diagnostics,
                )
            })
            .collect();
        Self {
            commands: None,
            thread: None,
            names: Arc::new(Mutex::new(HashMap::new())),
            cancelled: discovery_cancelled,
            children,
        }
    }

    /// Starts a worker that continuously discovers existing and future targets.
    pub fn spawn<N, F, D>(start_namer: F, discover: D, poll_interval: Duration) -> Self
    where
        N: ConversationNamer,
        F: FnOnce(Arc<AtomicBool>) -> Result<N> + Send + 'static,
        D: FnMut() -> Result<Vec<NamingTarget>> + Send + 'static,
    {
        Self::spawn_with_intervals(
            start_namer,
            discover,
            poll_interval,
            NAMING_REFRESH_INTERVAL,
            NAMING_RETRY_INTERVAL,
        )
    }

    /// Starts a worker with privacy-safe operational event logging.
    pub fn spawn_logged<N, F, D>(
        start_namer: F,
        mut discover: D,
        poll_interval: Duration,
        diagnostics: Option<NamingDiagnostics>,
    ) -> Self
    where
        N: ConversationNamer,
        F: FnOnce(Arc<AtomicBool>) -> Result<N> + Send + 'static,
        D: FnMut() -> Result<Vec<NamingTarget>> + Send + 'static,
    {
        Self::spawn_logged_phased(start_namer, move |_| discover(), poll_interval, diagnostics)
    }

    fn spawn_logged_phased<N, F, D>(
        start_namer: F,
        discover: D,
        poll_interval: Duration,
        diagnostics: Option<NamingDiagnostics>,
    ) -> Self
    where
        N: ConversationNamer,
        F: FnOnce(Arc<AtomicBool>) -> Result<N> + Send + 'static,
        D: FnMut(DiscoveryPhase) -> Result<Vec<NamingTarget>> + Send + 'static,
    {
        Self::spawn_with_retry_intervals(
            start_namer,
            discover,
            poll_interval,
            NAMING_REFRESH_INTERVAL,
            NAMING_RETRY_INTERVAL,
            PENDING_NAMING_RETRY_INTERVAL,
            diagnostics,
        )
    }

    fn spawn_with_intervals<N, F, D>(
        start_namer: F,
        mut discover: D,
        poll_interval: Duration,
        refresh_interval: Duration,
        retry_interval: Duration,
    ) -> Self
    where
        N: ConversationNamer,
        F: FnOnce(Arc<AtomicBool>) -> Result<N> + Send + 'static,
        D: FnMut() -> Result<Vec<NamingTarget>> + Send + 'static,
    {
        Self::spawn_with_retry_intervals(
            start_namer,
            move |_| discover(),
            poll_interval,
            refresh_interval,
            retry_interval,
            PENDING_NAMING_RETRY_INTERVAL,
            None,
        )
    }

    fn spawn_with_retry_intervals<N, F, D>(
        start_namer: F,
        mut discover: D,
        poll_interval: Duration,
        refresh_interval: Duration,
        retry_interval: Duration,
        pending_retry_interval: Duration,
        diagnostics: Option<NamingDiagnostics>,
    ) -> Self
    where
        N: ConversationNamer,
        F: FnOnce(Arc<AtomicBool>) -> Result<N> + Send + 'static,
        D: FnMut(DiscoveryPhase) -> Result<Vec<NamingTarget>> + Send + 'static,
    {
        let (commands, command_receiver) = mpsc::channel();
        let names: GeneratedNames = Arc::new(Mutex::new(HashMap::new()));
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let published = names.clone();
        let thread = thread::spawn(move || {
            log_diagnostic(&diagnostics, "worker_start");
            let Ok(mut namer) = start_namer(worker_cancelled.clone()) else {
                log_diagnostic(&diagnostics, "provider_start_failed");
                return;
            };
            log_diagnostic(&diagnostics, "provider_ready");
            let mut cache = HashMap::<String, (u64, String)>::new();
            let mut last_attempt =
                HashMap::<AttemptIdentity, (std::time::Instant, Duration)>::new();
            'worker: loop {
                match drain_commands(&command_receiver) {
                    CommandSignal::Stop => break,
                    CommandSignal::Wake | CommandSignal::None => {}
                }
                let Ok(targets) = discover(DiscoveryPhase::Scan) else {
                    log_diagnostic(&diagnostics, "discovery_failed");
                    if wait_for_stop(&command_receiver, poll_interval) {
                        break;
                    }
                    continue;
                };
                published.lock().unwrap().retain(|pane_id, generated| {
                    targets.iter().any(|target| {
                        &target.pane_id == pane_id
                            && target.pane_title == generated.source_title
                            && target.cwd == generated.source_cwd
                    })
                });
                let mut by_thread = HashMap::<(String, PathBuf), Vec<NamingTarget>>::new();
                for target in targets {
                    by_thread
                        .entry((target.thread_hint.clone(), target.cwd.clone()))
                        .or_default()
                        .push(target);
                }
                let active_attempts = by_thread
                    .iter()
                    .map(|(identity, targets)| attempt_identity(identity, targets))
                    .collect::<std::collections::HashSet<_>>();
                last_attempt.retain(|identity, _| active_attempts.contains(identity));
                for (identity, targets) in by_thread {
                    match drain_commands(&command_receiver) {
                        CommandSignal::Stop => return,
                        CommandSignal::Wake => continue 'worker,
                        CommandSignal::None => {}
                    }
                    let now = unix_seconds(SystemTime::now());
                    let Some(due_target) = targets.iter().find(|target| naming_is_due(target, now))
                    else {
                        continue;
                    };
                    let attempt_key = attempt_identity(&identity, &targets);
                    if last_attempt
                        .get(&attempt_key)
                        .is_some_and(|(attempted, cooldown)| attempted.elapsed() < *cooldown)
                    {
                        continue;
                    }
                    last_attempt.insert(
                        attempt_key.clone(),
                        (std::time::Instant::now(), retry_interval),
                    );
                    let conversation = match namer.read(due_target) {
                        Ok(conversation) => conversation,
                        Err(_) if !namer.is_healthy() => {
                            log_diagnostic(&diagnostics, "read_provider_unhealthy");
                            let _ = wait_for_stop_ignoring_wakes(&command_receiver, retry_interval);
                            return;
                        }
                        Err(_) => {
                            log_diagnostic(&diagnostics, "read_failed");
                            continue;
                        }
                    };
                    if conversation.transcript.trim().is_empty() {
                        log_diagnostic(&diagnostics, "conversation_pending");
                        // A running first turn is pending, not a provider failure. Keeping the
                        // failure cooldown here would hide text completed just after this read.
                        last_attempt.insert(
                            attempt_key,
                            (
                                std::time::Instant::now(),
                                retry_interval.min(pending_retry_interval),
                            ),
                        );
                        continue;
                    }
                    match drain_commands(&command_receiver) {
                        CommandSignal::Stop => return,
                        CommandSignal::Wake => continue 'worker,
                        CommandSignal::None => {}
                    }
                    let thread_id = conversation.thread_id.clone();
                    let fingerprint = transcript_fingerprint(&conversation.transcript);
                    let name = if let Some((cached, name)) = cache.get(&thread_id) {
                        if *cached == fingerprint {
                            name.clone()
                        } else {
                            let name = match namer.name(&conversation) {
                                Ok(name) => name,
                                Err(_) if !namer.is_healthy() => {
                                    log_diagnostic(&diagnostics, "naming_provider_unhealthy");
                                    let _ = wait_for_stop_ignoring_wakes(
                                        &command_receiver,
                                        retry_interval,
                                    );
                                    return;
                                }
                                Err(_) => {
                                    log_diagnostic(&diagnostics, "naming_failed");
                                    continue;
                                }
                            };
                            cache.insert(thread_id.clone(), (fingerprint, name.clone()));
                            name
                        }
                    } else {
                        let name = match namer.name(&conversation) {
                            Ok(name) => name,
                            Err(_) if !namer.is_healthy() => {
                                log_diagnostic(&diagnostics, "naming_provider_unhealthy");
                                let _ =
                                    wait_for_stop_ignoring_wakes(&command_receiver, retry_interval);
                                return;
                            }
                            Err(_) => {
                                log_diagnostic(&diagnostics, "naming_failed");
                                continue;
                            }
                        };
                        cache.insert(thread_id.clone(), (fingerprint, name.clone()));
                        name
                    };
                    let still_resolved = match namer.resolve(due_target) {
                        Ok(thread_id) => thread_id,
                        Err(_) if !namer.is_healthy() => {
                            log_diagnostic(&diagnostics, "resolve_provider_unhealthy");
                            let _ = wait_for_stop_ignoring_wakes(&command_receiver, retry_interval);
                            return;
                        }
                        Err(_) => {
                            log_diagnostic(&diagnostics, "resolve_failed");
                            continue;
                        }
                    };
                    if still_resolved != thread_id {
                        log_diagnostic(&diagnostics, "identity_changed");
                        continue;
                    }
                    match drain_commands(&command_receiver) {
                        CommandSignal::Stop => return,
                        CommandSignal::Wake => continue 'worker,
                        CommandSignal::None => {}
                    }
                    let Ok(current) = discover(DiscoveryPhase::Revalidate) else {
                        continue;
                    };
                    let mut matching_groups =
                        HashMap::<(String, PathBuf), Vec<NamingTarget>>::new();
                    for target in current {
                        let matches_thread = target.thread_hint == thread_id
                            || namer
                                .resolve(&target)
                                .is_ok_and(|resolved| resolved == thread_id);
                        if !matches_thread {
                            continue;
                        }
                        matching_groups
                            .entry((target.thread_hint.clone(), target.cwd.clone()))
                            .or_default()
                            .push(target.clone());
                    }
                    let mut published = published.lock().unwrap();
                    let mut published_any = false;
                    for targets in matching_groups.values() {
                        for target in targets {
                            published.insert(
                                target.pane_id.clone(),
                                GeneratedName {
                                    thread_id: thread_id.clone(),
                                    source_title: target.pane_title.clone(),
                                    source_cwd: target.cwd.clone(),
                                    name: name.clone(),
                                    generated_at_unix: now,
                                },
                            );
                            published_any = true;
                        }
                    }
                    if published_any {
                        log_diagnostic(&diagnostics, "name_published");
                        last_attempt.insert(
                            attempt_key.clone(),
                            (std::time::Instant::now(), refresh_interval),
                        );
                        for (identity, targets) in matching_groups {
                            last_attempt.insert(
                                attempt_identity(&identity, &targets),
                                (std::time::Instant::now(), refresh_interval),
                            );
                        }
                    }
                }
                match command_receiver.recv_timeout(poll_interval) {
                    Ok(NamingCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Ok(NamingCommand::Wake) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        });
        Self {
            commands: Some(commands),
            thread: Some(thread),
            names,
            cancelled,
            children: Vec::new(),
        }
    }

    /// Returns the shared generated-name snapshot.
    #[must_use]
    pub fn names(&self) -> GeneratedNames {
        if !self.children.is_empty() {
            let mut merged = self.names.lock().unwrap();
            merged.clear();
            for child in &self.children {
                merged.extend(child.names.lock().unwrap().clone());
            }
        }
        self.names.clone()
    }

    /// Wakes discovery early after a pane acquires an exact resumed thread.
    pub fn trigger(&self) {
        for child in &self.children {
            child.trigger();
        }
        if let Some(commands) = &self.commands {
            let _ = commands.send(NamingCommand::Wake);
        }
    }

    /// Reports whether the worker lane ended, including provider startup failure.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        if !self.children.is_empty() {
            return self.children.iter().any(Self::is_finished);
        }
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Stops and joins the worker before returning.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        for child in self.children.drain(..) {
            child.stop();
        }
        if let Some(commands) = self.commands.take() {
            let _ = commands.send(NamingCommand::Stop);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn unix_seconds(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn naming_is_due(target: &NamingTarget, now_unix: u64) -> bool {
    if target.immediate_naming {
        return true;
    }
    if target.generated_name.is_some() {
        return target.generated_at_unix.is_none_or(|generated_at| {
            now_unix.saturating_sub(generated_at) >= NAMING_REFRESH_INTERVAL.as_secs()
        });
    }
    let _ = now_unix;
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandSignal {
    None,
    Stop,
    Wake,
}

type AttemptIdentity = (String, PathBuf, Vec<(PaneId, String, bool)>);

fn attempt_identity(identity: &(String, PathBuf), targets: &[NamingTarget]) -> AttemptIdentity {
    let mut panes = targets
        .iter()
        .map(|target| {
            (
                target.pane_id.clone(),
                target.pane_title.clone(),
                target.immediate_naming,
            )
        })
        .collect::<Vec<_>>();
    panes.sort_unstable();
    (identity.0.clone(), identity.1.clone(), panes)
}

fn drain_commands(commands: &mpsc::Receiver<NamingCommand>) -> CommandSignal {
    let mut signal = CommandSignal::None;
    while let Ok(command) = commands.try_recv() {
        if matches!(command, NamingCommand::Stop) {
            return CommandSignal::Stop;
        }
        signal = CommandSignal::Wake;
    }
    signal
}

fn wait_for_stop(commands: &mpsc::Receiver<NamingCommand>, duration: Duration) -> bool {
    matches!(
        commands.recv_timeout(duration),
        Ok(NamingCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected)
    )
}

/// Waits through provider backoff without letting a pane-local wake amplify a
/// persistent transport or model failure into rapid app-server restarts.
fn wait_for_stop_ignoring_wakes(
    commands: &mpsc::Receiver<NamingCommand>,
    duration: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + duration;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match commands.recv_timeout(remaining) {
            Ok(NamingCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => return true,
            Ok(NamingCommand::Wake) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => return false,
        }
    }
}

impl Drop for NamingWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn transcript_fingerprint(transcript: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    transcript.hash(&mut hasher);
    hasher.finish()
}

pub(crate) fn looks_like_thread_id(value: &str) -> bool {
    value.len() == 36
        && value.chars().enumerate().all(|(index, character)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                character == '-'
            } else {
                character.is_ascii_hexdigit()
            }
        })
}

/// Kernel-bound resolver for the root rollout currently held by an exact
/// configured Codex process in a pane's process tree.
#[derive(Clone, Debug)]
pub struct ProcessRolloutStore {
    executables: Vec<(u64, u64)>,
    rollout_root: PathBuf,
}

struct ProcessScanBudget<'a> {
    cancelled: &'a AtomicBool,
    fds: usize,
    bytes: u64,
}

impl<'a> ProcessScanBudget<'a> {
    fn new(cancelled: &'a AtomicBool) -> Self {
        Self {
            cancelled,
            fds: 0,
            bytes: 0,
        }
    }

    fn checkpoint(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(MuxError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn fd(&mut self) -> Result<()> {
        self.checkpoint()?;
        self.fds += 1;
        if self.fds > MAX_PROCESS_SCAN_FDS {
            return Err(protocol(
                "process rollout discovery exceeded its descriptor budget",
            ));
        }
        Ok(())
    }

    fn bytes(&mut self, count: u64) -> Result<()> {
        self.checkpoint()?;
        self.bytes = self.bytes.saturating_add(count);
        if self.bytes > MAX_PROCESS_SCAN_BYTES {
            return Err(protocol(
                "process rollout discovery exceeded its byte budget",
            ));
        }
        Ok(())
    }
}

impl ProcessRolloutStore {
    /// Captures the configured executable identities and Codex sessions root.
    pub fn discover(executables: &[CodexExecutable]) -> Result<Self> {
        let rollout_root = RolloutStore::discover()?.root;
        Self::at(executables, rollout_root)
    }

    /// Uses an explicit rollout root, primarily for hermetic integration tests.
    pub fn at(executables: &[CodexExecutable], rollout_root: impl Into<PathBuf>) -> Result<Self> {
        let executables = executables
            .iter()
            .map(|executable| {
                let path = executable.as_path();
                let metadata = fs::metadata(path).map_err(|source| MuxError::Filesystem {
                    path: path.to_owned(),
                    source,
                })?;
                Ok((metadata.dev(), metadata.ino()))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            executables,
            rollout_root: rollout_root.into(),
        })
    }

    /// Resolves one exact non-subagent rollout identity or fails closed.
    pub fn resolve(&self, pane: &Pane) -> Result<Option<String>> {
        let cancelled = AtomicBool::new(false);
        self.resolve_cancellable(pane, &cancelled)
    }

    pub(crate) fn resolve_cancellable(
        &self,
        pane: &Pane,
        cancelled: &AtomicBool,
    ) -> Result<Option<String>> {
        self.resolve_with_socket_diagnostics(
            pane,
            unix_socket_diagnostics(Some(cancelled)),
            cancelled,
        )
    }

    pub(crate) fn resolve_all_cancellable(
        &self,
        panes: &[Pane],
        cancelled: &AtomicBool,
    ) -> Result<Vec<Option<String>>> {
        let mut budget = ProcessScanBudget::new(cancelled);
        let processes = process_snapshot(&mut budget)?;
        let diagnostics = match unix_socket_diagnostics(Some(cancelled)) {
            Ok(diagnostics) => {
                budget.bytes(diagnostics.scanned_bytes)?;
                Some(diagnostics)
            }
            Err(MuxError::Cancelled) => return Err(MuxError::Cancelled),
            Err(_) => None,
        };
        let needs_peer = match diagnostics.as_ref() {
            Some(diagnostics) => {
                self.has_peer_candidates(panes, &processes, diagnostics, &mut budget)?
            }
            None => false,
        };
        let fresh_diagnostics = fresh_socket_diagnostics(needs_peer, &mut budget)?;
        let proc_unix = fresh_diagnostics
            .as_ref()
            .map(|_| read_bounded_proc_unix(&mut budget))
            .transpose()?;
        panes
            .iter()
            .map(|pane| {
                self.resolve_in_snapshot(
                    pane,
                    &processes,
                    diagnostics.as_ref(),
                    fresh_diagnostics.as_ref(),
                    proc_unix.as_deref(),
                    &mut budget,
                )
            })
            .collect()
    }

    fn resolve_with_socket_diagnostics(
        &self,
        pane: &Pane,
        diagnostics: Result<UnixSocketDiagnostics>,
        cancelled: &AtomicBool,
    ) -> Result<Option<String>> {
        let mut budget = ProcessScanBudget::new(cancelled);
        let processes = process_snapshot(&mut budget)?;
        let diagnostics = diagnostics
            .ok()
            .map(|diagnostics| {
                budget.bytes(diagnostics.scanned_bytes)?;
                Ok(diagnostics)
            })
            .transpose()?;
        let needs_peer = match diagnostics.as_ref() {
            Some(diagnostics) => self.has_peer_candidates(
                std::slice::from_ref(pane),
                &processes,
                diagnostics,
                &mut budget,
            )?,
            None => false,
        };
        let fresh_diagnostics = fresh_socket_diagnostics(needs_peer, &mut budget)?;
        let proc_unix = fresh_diagnostics
            .as_ref()
            .map(|_| read_bounded_proc_unix(&mut budget))
            .transpose()?;
        self.resolve_in_snapshot(
            pane,
            &processes,
            diagnostics.as_ref(),
            fresh_diagnostics.as_ref(),
            proc_unix.as_deref(),
            &mut budget,
        )
    }

    fn has_peer_candidates(
        &self,
        panes: &[Pane],
        processes: &HashMap<u32, (u32, u64)>,
        diagnostics: &UnixSocketDiagnostics,
        budget: &mut ProcessScanBudget<'_>,
    ) -> Result<bool> {
        let parents = processes
            .iter()
            .map(|(&pid, &(parent, _))| (pid, parent))
            .collect::<HashMap<_, _>>();
        for &pid in processes.keys() {
            if !self.executable_matches(pid)
                || !panes
                    .iter()
                    .any(|pane| descends_from(pid, pane.pane_pid, &parents))
            {
                continue;
            }
            if process_socket_inodes(pid, budget)?
                .iter()
                .any(|inode| diagnostics.peers.contains_key(inode))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn resolve_in_snapshot(
        &self,
        pane: &Pane,
        processes: &HashMap<u32, (u32, u64)>,
        diagnostics: Option<&UnixSocketDiagnostics>,
        fresh_diagnostics: Option<&UnixSocketDiagnostics>,
        proc_unix: Option<&str>,
        budget: &mut ProcessScanBudget<'_>,
    ) -> Result<Option<String>> {
        let Some(root_identity) = processes.get(&pane.pane_pid).copied() else {
            return Ok(None);
        };
        let parents = processes
            .iter()
            .map(|(&pid, &(parent, _))| (pid, parent))
            .collect::<HashMap<_, _>>();
        let candidates = processes
            .iter()
            .filter(|(pid, _)| {
                descends_from(**pid, pane.pane_pid, &parents) && self.executable_matches(**pid)
            })
            .map(|(&pid, &identity)| (pid, identity))
            .collect::<Vec<_>>();
        // Direct rollout descriptors are authoritative without sock_diag.
        // Netlink is optional evidence used only for legacy control peers.
        let mut peer_inodes = HashSet::new();
        if let Some(diagnostics) = diagnostics {
            for (pid, _) in &candidates {
                for inode in process_socket_inodes(*pid, budget)? {
                    if let Some(peer) = diagnostics.peers.get(&inode) {
                        peer_inodes.insert(*peer);
                    }
                }
            }
        }
        let peer_owners = if peer_inodes.is_empty() {
            HashMap::new()
        } else {
            process_socket_owners(processes, &peer_inodes, budget)?
        };
        let mut matched = HashSet::new();
        let mut direct_candidates = Vec::new();
        for (pid, identity) in candidates {
            let identities = self.process_rollouts(pid, budget)?;
            if !revalidate_descendant(pid, identity, pane.pane_pid, root_identity, processes)
                || !self.executable_matches(pid)
            {
                continue;
            }
            if !identities.is_empty() {
                direct_candidates.push((pid, identity));
            }

            // Older Codex clients delegate the active thread to a private
            // app-server control peer. Follow only kernel-reported Unix peer
            // inodes, then require that peer to own the exact private control
            // socket before considering its rollout descriptors.
            let Some(diagnostics) = diagnostics else {
                continue;
            };
            let sockets = process_socket_inodes(pid, budget)?;
            for (candidate_inode, peer_inode) in sockets
                .iter()
                .filter_map(|inode| diagnostics.peers.get(inode).map(|peer| (*inode, *peer)))
            {
                for &peer_pid in peer_owners.get(&peer_inode).into_iter().flatten() {
                    let Some(&peer_identity) = processes.get(&peer_pid) else {
                        continue;
                    };
                    let Some(fresh) = fresh_diagnostics else {
                        continue;
                    };
                    let Some(proc_unix) = proc_unix else {
                        continue;
                    };
                    if process_stat_identity(peer_pid).ok() != Some(peer_identity)
                        || !revalidate_descendant(
                            pid,
                            identity,
                            pane.pane_pid,
                            root_identity,
                            processes,
                        )
                        || !process_socket_inodes(pid, budget)?.contains(&candidate_inode)
                        || !process_socket_inodes(peer_pid, budget)?.contains(&peer_inode)
                        || fresh.peers.get(&candidate_inode) != Some(&peer_inode)
                        || !self.peer_is_private_control_connection(
                            peer_pid, peer_inode, fresh, proc_unix, budget,
                        )?
                    {
                        continue;
                    }
                    let peer_rollouts = self.process_rollouts(peer_pid, budget)?;
                    if process_stat_identity(peer_pid).ok() != Some(peer_identity)
                        || !revalidate_descendant(
                            pid,
                            identity,
                            pane.pane_pid,
                            root_identity,
                            processes,
                        )
                        || !process_socket_inodes(pid, budget)?.contains(&candidate_inode)
                        || !process_socket_inodes(peer_pid, budget)?.contains(&peer_inode)
                        || !self.peer_is_private_control_connection(
                            peer_pid, peer_inode, fresh, proc_unix, budget,
                        )?
                    {
                        continue;
                    }
                    matched.extend(peer_rollouts);
                }
            }
        }
        for (pid, identity) in direct_candidates {
            if revalidate_descendant(pid, identity, pane.pane_pid, root_identity, processes)
                && self.executable_matches(pid)
            {
                let rollouts = self.process_rollouts(pid, budget)?;
                if revalidate_descendant(pid, identity, pane.pane_pid, root_identity, processes)
                    && self.executable_matches(pid)
                {
                    matched.extend(rollouts);
                }
            }
        }
        budget.checkpoint()?;
        if process_stat_identity(pane.pane_pid).ok() != Some(root_identity) {
            return Ok(None);
        }
        match matched.len() {
            0 => Ok(None),
            1 => Ok(matched.into_iter().next()),
            _ => Err(protocol(
                "pane Codex processes hold multiple root conversation rollouts",
            )),
        }
    }

    fn executable_matches(&self, pid: u32) -> bool {
        let path = PathBuf::from(format!("/proc/{pid}/exe"));
        fs::metadata(path).ok().is_some_and(|metadata| {
            self.executables
                .iter()
                .any(|identity| *identity == (metadata.dev(), metadata.ino()))
        })
    }

    fn private_control_server_matches(
        &self,
        pid: u32,
        codex_home: &Path,
        budget: &mut ProcessScanBudget<'_>,
    ) -> Result<bool> {
        if self.executable_matches(pid) {
            return Ok(true);
        }
        let Ok(executable) = fs::read_link(format!("/proc/{pid}/exe")) else {
            return Ok(false);
        };
        let releases = codex_home.join("packages/standalone/releases");
        let Ok(relative) = executable.strip_prefix(&releases) else {
            return Ok(false);
        };
        let components = relative.components().collect::<Vec<_>>();
        if components.len() != 3
            || components[1].as_os_str() != "bin"
            || components[2].as_os_str() != "codex"
        {
            return Ok(false);
        }
        let euid = rustix::process::geteuid().as_raw();
        let trusted_path = [
            codex_home.to_path_buf(),
            codex_home.join("packages"),
            codex_home.join("packages/standalone"),
            releases,
            executable.parent().unwrap().parent().unwrap().to_path_buf(),
            executable.parent().unwrap().to_path_buf(),
        ]
        .iter()
        .all(|directory| {
            fs::symlink_metadata(directory)
                .ok()
                .is_some_and(|metadata| {
                    metadata.is_dir() && metadata.uid() == euid && metadata.mode() & 0o022 == 0
                })
        });
        let trusted_executable = fs::symlink_metadata(&executable)
            .ok()
            .is_some_and(|metadata| {
                let running = fs::metadata(format!("/proc/{pid}/exe")).ok();
                metadata.is_file()
                    && metadata.uid() == euid
                    && metadata.mode() & 0o022 == 0
                    && metadata.mode() & 0o111 != 0
                    && running.as_ref().is_some_and(|running| {
                        running.dev() == metadata.dev() && running.ino() == metadata.ino()
                    })
            });
        let command_path = PathBuf::from(format!("/proc/{pid}/cmdline"));
        budget.bytes(64 * 1024 + 1)?;
        let mut command = Vec::new();
        let command_ok = fs::File::open(&command_path)
            .and_then(|file| file.take(64 * 1024 + 1).read_to_end(&mut command))
            .is_ok()
            && command.len() <= 64 * 1024;
        if !command_ok {
            return Ok(false);
        }
        let arguments = command
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .collect::<Vec<_>>();
        Ok(trusted_path
            && trusted_executable
            && arguments.get(1).copied() == Some(b"app-server")
            && arguments.get(2).copied() == Some(b"--listen")
            && arguments.get(3).copied() == Some(b"unix://"))
    }

    fn peer_is_private_control_connection(
        &self,
        pid: u32,
        peer_inode: u32,
        sockets: &UnixSocketDiagnostics,
        proc_unix: &str,
        budget: &mut ProcessScanBudget<'_>,
    ) -> Result<bool> {
        let Some(codex_home) = self.rollout_root.parent() else {
            return Ok(false);
        };
        let expected = codex_home
            .join("app-server-control")
            .join("app-server-control.sock");
        let private_socket = fs::symlink_metadata(&expected)
            .ok()
            .is_some_and(|metadata| {
                metadata.file_type().is_socket()
                    && metadata.uid() == rustix::process::geteuid().as_raw()
                    && metadata.mode() & 0o022 == 0
            });
        if !private_socket {
            return Ok(false);
        }
        if !self.private_control_server_matches(pid, codex_home, budget)? {
            return Ok(false);
        }
        let euid = rustix::process::geteuid().as_raw();
        if [
            codex_home,
            expected.parent().expect("control socket parent"),
        ]
        .iter()
        .any(|directory| {
            !fs::symlink_metadata(directory)
                .ok()
                .is_some_and(|metadata| {
                    metadata.is_dir() && metadata.uid() == euid && metadata.mode() & 0o022 == 0
                })
        }) {
            return Ok(false);
        }
        if sockets.names.get(&peer_inode).map(Vec::as_slice)
            != Some(expected.as_os_str().as_bytes())
        {
            return Ok(false);
        }
        let sockets = process_socket_inodes(pid, budget)?;
        let mut exact_peer = false;
        let mut owned_listener = false;
        for line in proc_unix.lines().skip(1) {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let inode = fields
                .get(6)
                .and_then(|inode| inode.parse::<u32>().ok())
                .unwrap_or_default();
            let expected_path = fields
                .get(7)
                .is_some_and(|path| Path::new(path) == expected);
            exact_peer |= inode == peer_inode && sockets.contains(&inode) && expected_path;
            owned_listener |=
                fields.get(5) == Some(&"01") && sockets.contains(&inode) && expected_path;
        }
        Ok(exact_peer
            && owned_listener
            && self.private_control_server_matches(pid, codex_home, budget)?)
    }

    fn process_rollouts(
        &self,
        pid: u32,
        budget: &mut ProcessScanBudget<'_>,
    ) -> Result<HashSet<String>> {
        let directory_path = PathBuf::from(format!("/proc/{pid}/fd"));
        let directory = match fs::read_dir(&directory_path) {
            Ok(directory) => directory,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                return Ok(HashSet::new());
            }
            Err(source) => {
                return Err(MuxError::Filesystem {
                    path: directory_path,
                    source,
                });
            }
        };
        let mut matched = HashSet::new();
        for (index, entry) in directory.enumerate() {
            budget.fd()?;
            if index >= MAX_PROCESS_FDS {
                return Err(protocol("Codex process exceeded the descriptor scan limit"));
            }
            let Ok(entry) = entry else { continue };
            let Some(before) = fd_info(pid, &entry.file_name()) else {
                continue;
            };
            if before.flags & 0o2_002_003 != 0o2_002_002 {
                continue;
            }
            let path = entry.path();
            let Ok(mut file) = fs::File::open(&path) else {
                continue;
            };
            if file.metadata().ok().map(|metadata| metadata.ino()) != Some(before.ino) {
                continue;
            }
            let actual = match fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())) {
                Ok(actual) => actual,
                Err(_) => continue,
            };
            if !actual.starts_with(&self.rollout_root) {
                continue;
            }
            let Some(thread_id) = rollout_thread_id(&actual) else {
                continue;
            };
            validate_rollout_file(&file, &actual)?;
            if rollout_is_root_identity(&mut file, &actual, &thread_id, budget)?
                && fd_info(pid, &entry.file_name()) == Some(before)
            {
                matched.insert(thread_id);
            }
        }
        Ok(matched)
    }
}

fn process_snapshot(budget: &mut ProcessScanBudget<'_>) -> Result<HashMap<u32, (u32, u64)>> {
    let mut processes = HashMap::new();
    let directory = fs::read_dir("/proc").map_err(|source| MuxError::Filesystem {
        path: PathBuf::from("/proc"),
        source,
    })?;
    for (index, entry) in directory.enumerate() {
        budget.checkpoint()?;
        if index >= MAX_PROCESS_ENTRIES {
            return Err(protocol("process discovery exceeded its entry limit"));
        }
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        if let Ok(identity) = process_stat_identity(pid) {
            processes.insert(pid, identity);
        }
    }
    Ok(processes)
}

fn read_bounded_proc_unix(budget: &mut ProcessScanBudget<'_>) -> Result<String> {
    let path = PathBuf::from("/proc/net/unix");
    let file = fs::File::open(&path).map_err(|source| MuxError::Filesystem {
        path: path.clone(),
        source,
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_PROCESS_SCAN_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| MuxError::Filesystem {
            path: path.clone(),
            source,
        })?;
    budget.bytes(bytes.len() as u64)?;
    if bytes.len() as u64 > MAX_PROCESS_SCAN_BYTES {
        return Err(protocol("/proc/net/unix exceeded the process byte budget"));
    }
    String::from_utf8(bytes).map_err(|_| protocol("/proc/net/unix was not valid UTF-8"))
}

fn fresh_socket_diagnostics(
    enabled: bool,
    budget: &mut ProcessScanBudget<'_>,
) -> Result<Option<UnixSocketDiagnostics>> {
    if !enabled {
        return Ok(None);
    }
    match unix_socket_diagnostics(Some(budget.cancelled)) {
        Ok(diagnostics) => {
            budget.bytes(diagnostics.scanned_bytes)?;
            Ok(Some(diagnostics))
        }
        Err(MuxError::Cancelled) => Err(MuxError::Cancelled),
        Err(_) => Ok(None),
    }
}

fn process_socket_inodes(pid: u32, budget: &mut ProcessScanBudget<'_>) -> Result<HashSet<u32>> {
    let path = PathBuf::from(format!("/proc/{pid}/fd"));
    let directory = match fs::read_dir(&path) {
        Ok(directory) => directory,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(HashSet::new());
        }
        Err(source) => return Err(MuxError::Filesystem { path, source }),
    };
    let mut sockets = HashSet::new();
    for (index, entry) in directory.enumerate() {
        budget.fd()?;
        if index >= MAX_PROCESS_FDS {
            return Err(protocol("process exceeded the descriptor scan limit"));
        }
        let Ok(entry) = entry else { continue };
        let Some(before) = fd_info(pid, &entry.file_name()) else {
            continue;
        };
        if before.flags & 0o2_000_000 != 0o2_000_000 {
            continue;
        }
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        let Some(target) = target.to_str() else {
            continue;
        };
        let Some(inode) = target
            .strip_prefix("socket:[")
            .and_then(|value| value.strip_suffix(']'))
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        if inode as u64 != before.ino || fd_info(pid, &entry.file_name()) != Some(before) {
            continue;
        }
        sockets.insert(inode);
    }
    Ok(sockets)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FdInfo {
    flags: u32,
    ino: u64,
}

fn fd_info(pid: u32, fd: &std::ffi::OsStr) -> Option<FdInfo> {
    let fd = fd.to_str()?;
    let contents = fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")).ok()?;
    let flags = contents.lines().find_map(|line| {
        line.strip_prefix("flags:\t")
            .and_then(|flags| u32::from_str_radix(flags, 8).ok())
    })?;
    let ino = contents
        .lines()
        .find_map(|line| line.strip_prefix("ino:\t").and_then(|ino| ino.parse().ok()))?;
    Some(FdInfo { flags, ino })
}

fn process_socket_owners(
    processes: &HashMap<u32, (u32, u64)>,
    wanted: &HashSet<u32>,
    budget: &mut ProcessScanBudget<'_>,
) -> Result<HashMap<u32, Vec<u32>>> {
    let mut owners = HashMap::<u32, Vec<u32>>::new();
    let mut visited = 0usize;
    for &pid in processes.keys() {
        let Ok(directory) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for entry in directory {
            budget.fd()?;
            visited += 1;
            if visited > MAX_PROCESS_ENTRIES {
                return Err(protocol("socket owner discovery exceeded its entry limit"));
            }
            let Ok(entry) = entry else { continue };
            let Ok(target) = fs::read_link(entry.path()) else {
                continue;
            };
            let Some(target) = target.to_str() else {
                continue;
            };
            let Some(inode) = target
                .strip_prefix("socket:[")
                .and_then(|value| value.strip_suffix(']'))
                .and_then(|value| value.parse::<u32>().ok())
            else {
                continue;
            };
            if wanted.contains(&inode) {
                owners.entry(inode).or_default().push(pid);
            }
        }
    }
    Ok(owners)
}

struct UnixSocketDiagnostics {
    peers: HashMap<u32, u32>,
    names: HashMap<u32, Vec<u8>>,
    scanned_bytes: u64,
}

fn unix_socket_diagnostics(cancelled: Option<&AtomicBool>) -> Result<UnixSocketDiagnostics> {
    use rustix_net::net::{
        AddressFamily, RecvFlags, SendFlags, SocketFlags, SocketType, bind,
        netlink::{SOCK_DIAG, SocketAddrNetlink},
        recv, sendto, socket_with,
        sockopt::{Timeout, set_socket_timeout},
    };

    const SOCK_DIAG_BY_FAMILY: u16 = 20;
    const NLM_F_REQUEST: u16 = 1;
    const NLM_F_DUMP: u16 = 0x300;
    const NLMSG_DONE: u16 = 3;
    const NLMSG_ERROR: u16 = 2;
    const UNIX_DIAG_PEER: u16 = 2;
    const UNIX_DIAG_NAME: u16 = 0;
    const UDIAG_SHOW_NAME: u32 = 1;
    const UDIAG_SHOW_PEER: u32 = 4;

    let socket = socket_with(
        AddressFamily::NETLINK,
        SocketType::RAW,
        SocketFlags::CLOEXEC,
        Some(SOCK_DIAG),
    )
    .map_err(|source| MuxError::Filesystem {
        path: PathBuf::from("netlink sock_diag"),
        source: source.into(),
    })?;
    bind(&socket, &SocketAddrNetlink::new(0, 0)).map_err(|source| MuxError::Filesystem {
        path: PathBuf::from("netlink sock_diag"),
        source: source.into(),
    })?;
    set_socket_timeout(&socket, Timeout::Recv, Some(Duration::from_millis(250))).map_err(
        |source| MuxError::Filesystem {
            path: PathBuf::from("netlink sock_diag"),
            source: source.into(),
        },
    )?;
    let mut request = Vec::with_capacity(40);
    request.extend_from_slice(&40_u32.to_ne_bytes());
    request.extend_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    request.extend_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
    request.extend_from_slice(&1_u32.to_ne_bytes());
    request.extend_from_slice(&0_u32.to_ne_bytes());
    request.push(1); // AF_UNIX
    request.push(0);
    request.extend_from_slice(&0_u16.to_ne_bytes());
    request.extend_from_slice(&u32::MAX.to_ne_bytes());
    request.extend_from_slice(&0_u32.to_ne_bytes());
    request.extend_from_slice(&(UDIAG_SHOW_NAME | UDIAG_SHOW_PEER).to_ne_bytes());
    request.extend_from_slice(&u32::MAX.to_ne_bytes());
    request.extend_from_slice(&u32::MAX.to_ne_bytes());
    sendto(
        &socket,
        &request,
        SendFlags::empty(),
        &SocketAddrNetlink::new(0, 0),
    )
    .map_err(|source| MuxError::Filesystem {
        path: PathBuf::from("netlink sock_diag"),
        source: source.into(),
    })?;

    let mut peers = HashMap::new();
    let mut names = HashMap::new();
    let mut scanned_bytes = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    for _ in 0..256 {
        if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Acquire)) {
            return Err(MuxError::Cancelled);
        }
        let (_, received) = recv(&socket, &mut buffer, RecvFlags::empty()).map_err(|source| {
            MuxError::Filesystem {
                path: PathBuf::from("netlink sock_diag"),
                source: source.into(),
            }
        })?;
        scanned_bytes = scanned_bytes.saturating_add(received as u64);
        if scanned_bytes > MAX_PROCESS_SCAN_BYTES {
            return Err(protocol(
                "netlink sock_diag exceeded the process byte budget",
            ));
        }
        let mut offset = 0usize;
        while offset + 16 <= received {
            let length =
                u32::from_ne_bytes(buffer[offset..offset + 4].try_into().unwrap()) as usize;
            let kind = u16::from_ne_bytes(buffer[offset + 4..offset + 6].try_into().unwrap());
            if length < 16 || offset + length > received {
                return Err(protocol("netlink sock_diag returned a malformed frame"));
            }
            if kind == NLMSG_DONE {
                return Ok(UnixSocketDiagnostics {
                    peers,
                    names,
                    scanned_bytes,
                });
            }
            if kind == NLMSG_ERROR {
                return Err(protocol("netlink sock_diag returned an error"));
            }
            if kind == SOCK_DIAG_BY_FAMILY && length >= 32 {
                let inode =
                    u32::from_ne_bytes(buffer[offset + 20..offset + 24].try_into().unwrap());
                let mut attribute = offset + 32;
                while attribute + 4 <= offset + length {
                    let attr_len =
                        u16::from_ne_bytes(buffer[attribute..attribute + 2].try_into().unwrap())
                            as usize;
                    let attr_kind = u16::from_ne_bytes(
                        buffer[attribute + 2..attribute + 4].try_into().unwrap(),
                    );
                    if attr_len < 4 || attribute + attr_len > offset + length {
                        return Err(protocol("netlink sock_diag returned a malformed attribute"));
                    }
                    if attr_kind == UNIX_DIAG_PEER && attr_len >= 8 {
                        let peer = u32::from_ne_bytes(
                            buffer[attribute + 4..attribute + 8].try_into().unwrap(),
                        );
                        peers.insert(inode, peer);
                    }
                    if attr_kind == UNIX_DIAG_NAME && attr_len > 4 {
                        let mut name = buffer[attribute + 4..attribute + attr_len].to_vec();
                        if name.last() == Some(&0) {
                            name.pop();
                        }
                        names.insert(inode, name);
                    }
                    attribute += (attr_len + 3) & !3;
                }
            }
            offset += (length + 3) & !3;
        }
    }
    Err(protocol("netlink sock_diag exceeded its response limit"))
}

fn rollout_is_root_identity(
    file: &mut fs::File,
    path: &Path,
    expected: &str,
    budget: &mut ProcessScanBudget<'_>,
) -> Result<bool> {
    file.seek(SeekFrom::Start(0))
        .map_err(|source| MuxError::Filesystem {
            path: path.to_owned(),
            source,
        })?;
    let mut reader = BufReader::new(file.take(MAX_ROLLOUT_IDENTITY_BYTES + 1));
    let mut line = Vec::new();
    let mut total = 0_u64;
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|source| MuxError::Filesystem {
                path: path.to_owned(),
                source,
            })?;
        if read == 0 {
            return Ok(false);
        }
        total += read as u64;
        budget.bytes(read as u64)?;
        if total > MAX_ROLLOUT_IDENTITY_BYTES {
            return Err(protocol("rollout identity exceeded its byte limit"));
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if value["type"] != "session_meta" {
            continue;
        }
        return Ok(
            value.pointer("/payload/id").and_then(Value::as_str) == Some(expected)
                && value.pointer("/payload/source/subagent").is_none(),
        );
    }
}

/// Bounded, local source of authoritative Codex thread identity and completed items.
#[derive(Clone, Debug)]
pub struct RolloutStore {
    root: PathBuf,
    cancelled: Option<Arc<AtomicBool>>,
}

impl RolloutStore {
    /// Discovers `$CODEX_HOME/sessions`, falling back to `~/.codex/sessions`.
    pub fn discover() -> Result<Self> {
        let codex_home = env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
            .ok_or_else(|| MuxError::Command("HOME and CODEX_HOME are unset".to_owned()))?;
        Ok(Self::at(codex_home.join("sessions")))
    }

    /// Uses an explicit sessions root, primarily for tests and embedding.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            cancelled: None,
        }
    }

    pub(crate) fn with_cancellation(mut self, cancelled: Arc<AtomicBool>) -> Self {
        self.cancelled = Some(cancelled);
        self
    }

    fn resolve_prefix(&self, prefix: &str) -> Result<Option<String>> {
        let mut matched: Option<String> = None;
        self.visit_rollouts(|thread_id, file, path| {
            if thread_id.starts_with(prefix)
                && rollout_identity_matches(file, path, thread_id, self.cancelled.as_ref())?
            {
                if matched
                    .as_deref()
                    .is_some_and(|existing| existing != thread_id)
                {
                    return Err(protocol("truncated pane title matches multiple rollouts"));
                }
                matched = Some(thread_id.to_owned());
            }
            Ok(())
        })?;
        Ok(matched)
    }

    fn read_completed(&self, thread_id: &str) -> Result<NamingConversation> {
        let mut matched = None;
        self.visit_rollouts(|candidate, file, path| {
            if candidate == thread_id
                && rollout_identity_matches(file, path, thread_id, self.cancelled.as_ref())?
            {
                let conversation =
                    read_rollout_transcript(file, path, thread_id, self.cancelled.as_ref())?;
                if matched.replace(conversation).is_some() {
                    return Err(protocol("thread id matches multiple rollout files"));
                }
            }
            Ok(())
        })?;
        matched.ok_or_else(|| protocol("verified rollout file was not found"))
    }

    #[cfg(unix)]
    fn visit_rollouts(
        &self,
        mut visit: impl FnMut(&str, &mut fs::File, &Path) -> Result<()>,
    ) -> Result<()> {
        use rustix::fs::{Mode, OFlags, open};

        ensure_not_cancelled(self.cancelled.as_ref())?;
        let root = match open(
            &self.root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(root) => root,
            Err(rustix::io::Errno::NOENT) => return Ok(()),
            Err(error) => return Err(rollout_io_error(&self.root, error)),
        };
        let root_file = fs::File::from(root);
        validate_rollout_directory(&root_file, &self.root)?;
        let root = root_file.into();
        let mut visited = 0usize;
        walk_rollout_directory(
            &root,
            &self.root,
            0,
            &mut visited,
            self.cancelled.as_ref(),
            &mut visit,
        )
    }

    #[cfg(not(unix))]
    fn visit_rollouts(
        &self,
        _visit: impl FnMut(&str, &mut fs::File, &Path) -> Result<()>,
    ) -> Result<()> {
        Err(protocol(
            "rollout fallback requires Unix descriptor-safe traversal",
        ))
    }
}

#[cfg(unix)]
fn walk_rollout_directory(
    directory: &OwnedFd,
    directory_path: &Path,
    depth: usize,
    visited: &mut usize,
    cancelled: Option<&Arc<AtomicBool>>,
    visit: &mut impl FnMut(&str, &mut fs::File, &Path) -> Result<()>,
) -> Result<()> {
    use rustix::fs::{Dir, FileType, Mode, OFlags, openat};

    if depth > MAX_ROLLOUT_DEPTH {
        return Err(protocol("rollout discovery exceeded its depth limit"));
    }
    let entries =
        Dir::read_from(directory).map_err(|error| rollout_io_error(directory_path, error))?;
    for entry in entries {
        ensure_not_cancelled(cancelled)?;
        *visited += 1;
        if *visited > MAX_ROLLOUT_ENTRIES {
            return Err(protocol("rollout discovery exceeded its entry limit"));
        }
        let entry = entry.map_err(|error| rollout_io_error(directory_path, error))?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let path = directory_path.join(std::ffi::OsStr::from_bytes(name.to_bytes()));
        match entry.file_type() {
            FileType::Directory => {
                let child = openat(
                    directory,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|error| rollout_io_error(&path, error))?;
                let child_file = fs::File::from(child);
                validate_rollout_directory(&child_file, &path)?;
                let child = child_file.into();
                walk_rollout_directory(&child, &path, depth + 1, visited, cancelled, visit)?;
            }
            FileType::RegularFile => {
                let Some(thread_id) = rollout_thread_id(&path) else {
                    continue;
                };
                let file = openat(
                    directory,
                    name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|error| rollout_io_error(&path, error))?;
                let mut file = fs::File::from(file);
                validate_rollout_file(&file, &path)?;
                visit(&thread_id, &mut file, &path)?;
            }
            FileType::Symlink => return Err(protocol("rollout tree contains a symlink")),
            FileType::Unknown => {
                return Err(protocol("rollout tree contains an unknown entry type"));
            }
            _ => {}
        }
    }
    Ok(())
}

fn ensure_not_cancelled(cancelled: Option<&Arc<AtomicBool>>) -> Result<()> {
    if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Acquire)) {
        return Err(MuxError::Cancelled);
    }
    Ok(())
}

#[cfg(unix)]
fn rollout_io_error(path: &Path, error: rustix::io::Errno) -> MuxError {
    MuxError::Filesystem {
        path: path.to_owned(),
        source: std::io::Error::from_raw_os_error(error.raw_os_error()),
    }
}

#[cfg(unix)]
fn validate_rollout_directory(file: &fs::File, path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().map_err(|source| MuxError::Filesystem {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o022 != 0
    {
        return Err(protocol("rollout directory is not private and owned"));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_rollout_file(file: &fs::File, path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().map_err(|source| MuxError::Filesystem {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.nlink() != 1
        || metadata.mode() & 0o022 != 0
    {
        return Err(protocol(
            "rollout is not a private owned single-link regular file",
        ));
    }
    Ok(())
}

fn rollout_thread_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let start = stem.len().checked_sub(36)?;
    let thread_id = &stem[start..];
    looks_like_thread_id(thread_id).then(|| thread_id.to_owned())
}

fn rollout_identity_matches(
    file: &mut fs::File,
    path: &Path,
    expected: &str,
    cancelled: Option<&Arc<AtomicBool>>,
) -> Result<bool> {
    file.seek(SeekFrom::Start(0))
        .map_err(|source| MuxError::Filesystem {
            path: path.to_owned(),
            source,
        })?;
    let mut reader = BufReader::new(file.take(MAX_ROLLOUT_IDENTITY_BYTES + 1));
    let mut line = Vec::new();
    let mut total = 0_u64;
    loop {
        ensure_not_cancelled(cancelled)?;
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|source| MuxError::Filesystem {
                path: path.to_owned(),
                source,
            })?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > MAX_ROLLOUT_IDENTITY_BYTES {
            return Err(protocol("rollout identity exceeded its byte limit"));
        }
        if let Ok(value) = serde_json::from_slice::<Value>(&line) {
            if value["type"] == "session_meta" {
                return Ok(value.pointer("/payload/id").and_then(Value::as_str) == Some(expected));
            }
        }
    }
    Ok(false)
}

fn read_rollout_transcript(
    file: &mut fs::File,
    path: &Path,
    thread_id: &str,
    cancelled: Option<&Arc<AtomicBool>>,
) -> Result<NamingConversation> {
    let length = file
        .metadata()
        .map_err(|source| MuxError::Filesystem {
            path: path.to_owned(),
            source,
        })?
        .len();
    if length > MAX_ROLLOUT_TRANSCRIPT_BYTES {
        file.seek(SeekFrom::Start(length - MAX_ROLLOUT_TRANSCRIPT_BYTES))
            .map_err(|source| MuxError::Filesystem {
                path: path.to_owned(),
                source,
            })?;
    } else {
        file.seek(SeekFrom::Start(0))
            .map_err(|source| MuxError::Filesystem {
                path: path.to_owned(),
                source,
            })?;
    }
    let mut reader = BufReader::new(file.take(MAX_ROLLOUT_TRANSCRIPT_BYTES + 1));
    if length > MAX_ROLLOUT_TRANSCRIPT_BYTES {
        let mut partial = Vec::new();
        let _ = reader.read_until(b'\n', &mut partial);
    }
    let mut transcript = RecentTranscript::default();
    let mut seen_item_ids = HashSet::new();
    let mut legacy_assistant_for_pair: Option<String> = None;
    let mut line = Vec::new();
    loop {
        ensure_not_cancelled(cancelled)?;
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|source| MuxError::Filesystem {
                path: path.to_owned(),
                source,
            })?;
        if read == 0 {
            break;
        }
        let paired_legacy_assistant = legacy_assistant_for_pair.take();
        if let Ok(value) = serde_json::from_slice::<Value>(&line) {
            if value["type"] == "event_msg"
                && value.pointer("/payload/type").and_then(Value::as_str) == Some("item_completed")
            {
                let item = &value["payload"]["item"];
                if let Some(id) = item["id"].as_str() {
                    if !seen_item_ids.insert(id.to_owned()) {
                        continue;
                    }
                }
                let role = match item["type"].as_str() {
                    Some("UserMessage") => Some("User"),
                    Some("AgentMessage") => Some("Assistant"),
                    _ => None,
                };
                if let Some(role) = role {
                    transcript.push_content(role, &item["content"]);
                }
            } else if value["type"] == "event_msg" {
                let payload = &value["payload"];
                let role = match payload["type"].as_str() {
                    Some("user_message") => Some("User"),
                    Some("agent_message") => Some("Assistant"),
                    _ => None,
                };
                if let Some(role) = role {
                    if let Some(message) = payload["message"].as_str() {
                        transcript.push(role, message);
                        if role == "Assistant" {
                            legacy_assistant_for_pair = Some(message.to_owned());
                        }
                    }
                }
            } else if value["type"] == "response_item"
                && value.pointer("/payload/type").and_then(Value::as_str) == Some("message")
                && value.pointer("/payload/role").and_then(Value::as_str) == Some("assistant")
            {
                let payload = &value["payload"];
                let id_is_new = payload["id"]
                    .as_str()
                    .is_none_or(|id| seen_item_ids.insert(id.to_owned()));
                let duplicates_legacy = payload["id"].as_str().is_none()
                    && rollout_single_text(&payload["content"])
                        .zip(paired_legacy_assistant.as_deref())
                        .is_some_and(|(response, legacy)| response == legacy);
                if id_is_new && !duplicates_legacy {
                    transcript.push_content("Assistant", &payload["content"]);
                }
            }
        }
    }
    Ok(NamingConversation {
        thread_id: thread_id.to_owned(),
        transcript: transcript.render(),
    })
}

fn rollout_single_text(content: &Value) -> Option<&str> {
    if let Some(text) = content.as_str() {
        return Some(text);
    }
    let parts = content.as_array()?;
    (parts.len() == 1)
        .then(|| parts[0]["text"].as_str())
        .flatten()
}

#[derive(Default)]
struct RecentTranscript {
    messages: VecDeque<String>,
    bytes: usize,
}

impl RecentTranscript {
    fn format_message(role: &str, text: &str) -> Option<String> {
        let mut message = format!("{role}: ");
        for character in text.chars() {
            if message.len() + character.len_utf8() + 1 > MAX_CONVERSATION_BYTES / 2 {
                break;
            }
            message.push(character);
        }
        message.push('\n');
        if message.trim() == format!("{role}:") {
            return None;
        }
        Some(message)
    }

    fn push(&mut self, role: &str, text: &str) {
        let Some(message) = Self::format_message(role, text) else {
            return;
        };
        while self.bytes + message.len() > MAX_CONVERSATION_BYTES {
            let Some(removed) = self.messages.pop_front() else {
                break;
            };
            self.bytes -= removed.len();
        }
        if message.len() <= MAX_CONVERSATION_BYTES {
            self.bytes += message.len();
            self.messages.push_back(message);
        }
    }

    fn push_newest_first(&mut self, role: &str, text: &str) {
        let Some(message) = Self::format_message(role, text) else {
            return;
        };
        if self.bytes + message.len() <= MAX_CONVERSATION_BYTES {
            self.bytes += message.len();
            self.messages.push_back(message);
        }
    }

    fn push_content(&mut self, role: &str, content: &Value) {
        if let Some(text) = content.as_str() {
            self.push(role, text);
            return;
        }
        let Some(parts) = content.as_array() else {
            return;
        };
        let text = parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            self.push(role, &text);
        }
    }

    fn render(self) -> String {
        self.messages.into_iter().collect()
    }

    fn render_reversed(self) -> String {
        self.messages.into_iter().rev().collect()
    }
}

/// Reads completed turns and asks an ephemeral Luna thread for a short title.
pub struct AppServerNamer<S> {
    session: S,
    diagnostics: Option<NamingDiagnostics>,
    rollouts: Option<RolloutStore>,
}

enum ThreadLookupError {
    Cancelled,
    Transport(MuxError),
    Evidence(MuxError),
}

impl<S: AppServerSession> AppServerNamer<S> {
    /// Wraps an initialized, version-compatible app-server session.
    #[must_use]
    pub const fn new(session: S) -> Self {
        Self {
            session,
            diagnostics: None,
            rollouts: None,
        }
    }

    /// Wraps a session with fixed privacy-safe resolution diagnostics.
    #[must_use]
    pub const fn with_diagnostics(session: S, diagnostics: NamingDiagnostics) -> Self {
        Self {
            session,
            diagnostics: Some(diagnostics),
            rollouts: None,
        }
    }

    /// Adds the local authoritative rollout store used for identity and read fallback.
    #[must_use]
    pub fn with_rollouts(mut self, rollouts: RolloutStore) -> Self {
        self.rollouts = Some(rollouts);
        self
    }

    fn resolve_thread_id(&mut self, target: &NamingTarget) -> Result<String> {
        if looks_like_thread_id(&target.thread_hint) {
            return Ok(target.thread_hint.clone());
        }

        let mut matched = None;
        if let Some(rollouts) = &self.rollouts {
            match rollouts.resolve_prefix(&target.thread_hint) {
                Ok(Some(thread_id)) => {
                    log_diagnostic(&self.diagnostics, "thread_rollout_resolved");
                    matched = Some(thread_id);
                }
                Ok(None) => {}
                Err(error) => {
                    log_diagnostic(&self.diagnostics, "thread_rollout_ambiguous");
                    return Err(error);
                }
            }
        }

        let state_match = match self.find_thread_id(target, true, matched.clone()) {
            Ok(value) => value,
            Err(ThreadLookupError::Cancelled) => return Err(MuxError::Cancelled),
            Err(ThreadLookupError::Transport(_error)) if matched.is_some() => {
                return Ok(matched.unwrap());
            }
            Err(ThreadLookupError::Transport(error) | ThreadLookupError::Evidence(error)) => {
                return Err(error);
            }
        };
        if state_match.is_none() {
            log_diagnostic(&self.diagnostics, "thread_state_db_miss");
        }
        log_diagnostic(&self.diagnostics, "thread_archive_cross_check");
        let archive_match = match self.find_thread_id(target, false, state_match) {
            Ok(value) => value,
            Err(ThreadLookupError::Cancelled) => return Err(MuxError::Cancelled),
            Err(ThreadLookupError::Transport(_error)) if matched.is_some() => matched,
            Err(ThreadLookupError::Transport(error) | ThreadLookupError::Evidence(error)) => {
                return Err(error);
            }
        };
        archive_match.ok_or_else(|| {
            log_diagnostic(&self.diagnostics, "thread_resolve_not_found");
            protocol("truncated pane title did not match one unique Codex thread")
        })
    }

    /// Finds one exact UUID prefix in one bounded app-server source.
    fn find_thread_id(
        &mut self,
        target: &NamingTarget,
        use_state_db_only: bool,
        mut matched: Option<String>,
    ) -> std::result::Result<Option<String>, ThreadLookupError> {
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        for _ in 0..MAX_THREAD_LIST_PAGES {
            let response = self
                .session
                .request(
                    "thread/list",
                    json!({
                        "cursor": cursor,
                        "limit": THREAD_LIST_PAGE_SIZE,
                        "sortKey": "updated_at",
                        "sortDirection": "desc",
                        "useStateDbOnly": use_state_db_only
                    }),
                )
                .map_err(|error| match error {
                    MuxError::Cancelled => ThreadLookupError::Cancelled,
                    error => ThreadLookupError::Transport(error),
                })?;
            let threads = response["data"].as_array().ok_or_else(|| {
                ThreadLookupError::Evidence(protocol("thread/list did not return thread data"))
            })?;
            if threads.len() > THREAD_LIST_PAGE_SIZE as usize {
                return Err(ThreadLookupError::Evidence(protocol(
                    "thread/list exceeded its requested page size",
                )));
            }
            for thread in threads {
                let Some(id) = thread["id"].as_str() else {
                    continue;
                };
                if looks_like_thread_id(id) && id.starts_with(&target.thread_hint) {
                    if matched.as_deref().is_some_and(|existing| existing != id) {
                        return Err(ThreadLookupError::Evidence(protocol(
                            "truncated pane title matches multiple threads",
                        )));
                    }
                    matched = Some(id.to_owned());
                }
            }

            let next =
                bounded_cursor(&response["nextCursor"]).map_err(ThreadLookupError::Evidence)?;
            if next.is_none() {
                return Ok(matched);
            }
            if !seen_cursors.insert(next.clone().unwrap()) {
                return Err(ThreadLookupError::Evidence(protocol(
                    "thread/list repeated its pagination cursor",
                )));
            }
            cursor = next;
        }
        Err(ThreadLookupError::Evidence(protocol(
            "thread/list exceeded the bounded pagination limit",
        )))
    }

    /// Reads structured completed turns through the bounded paginated API.
    pub fn read_completed(&mut self, thread_id: &str) -> Result<NamingConversation> {
        let mut transcript = RecentTranscript::default();
        let mut completed_turns = HashSet::new();
        let mut request_budget = MAX_HISTORY_REQUESTS;
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        let mut turns_complete = false;
        for _ in 0..MAX_THREAD_TURNS_PAGES {
            take_history_request(&mut request_budget)?;
            let response = self.session.request(
                "thread/turns/list",
                json!({
                    "threadId": thread_id,
                    "cursor": cursor,
                    "limit": THREAD_TURNS_PAGE_SIZE,
                    "itemsView": "notLoaded",
                    "sortDirection": "desc"
                }),
            )?;
            let turns = response["data"]
                .as_array()
                .ok_or_else(|| protocol("thread/turns/list did not return turn data"))?;
            if turns.len() > THREAD_TURNS_PAGE_SIZE as usize {
                return Err(protocol(
                    "thread/turns/list exceeded its requested page size",
                ));
            }
            for turn in turns.iter().filter(|turn| turn["status"] == "completed") {
                let Some(turn_id) = turn["id"].as_str() else {
                    continue;
                };
                if turn_id.is_empty() || turn_id.len() > MAX_HISTORY_ID_BYTES {
                    return Err(protocol("thread/turns/list returned an invalid turn id"));
                }
                if completed_turns.len() >= MAX_HISTORY_ENTRIES
                    && !completed_turns.contains(turn_id)
                {
                    return Err(protocol("conversation history exceeded its entry budget"));
                }
                completed_turns.insert(turn_id.to_owned());
            }
            let next = bounded_cursor(&response["nextCursor"])?;
            if next.is_none() {
                turns_complete = true;
                break;
            }
            if !seen_cursors.insert(next.clone().unwrap()) {
                return Err(protocol("thread/turns/list repeated its pagination cursor"));
            }
            cursor = next;
            continue;
        }
        if !turns_complete {
            return Err(protocol(
                "thread/turns/list exceeded the bounded pagination limit",
            ));
        }
        self.read_thread_items(
            thread_id,
            &completed_turns,
            &mut transcript,
            &mut request_budget,
        )?;
        Ok(NamingConversation {
            thread_id: thread_id.to_owned(),
            transcript: transcript.render_reversed(),
        })
    }

    fn read_thread_items(
        &mut self,
        thread_id: &str,
        completed_turns: &HashSet<String>,
        transcript: &mut RecentTranscript,
        request_budget: &mut usize,
    ) -> Result<()> {
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        for _ in 0..MAX_THREAD_TURNS_PAGES {
            take_history_request(request_budget)?;
            let response = self.session.request(
                "thread/items/list",
                json!({
                    "threadId": thread_id,
                    "cursor": cursor,
                    "limit": THREAD_TURNS_PAGE_SIZE,
                    "sortDirection": "desc"
                }),
            )?;
            let items = response["data"]
                .as_array()
                .ok_or_else(|| protocol("thread/items/list did not return item data"))?;
            if items.len() > THREAD_TURNS_PAGE_SIZE as usize {
                return Err(protocol(
                    "thread/items/list exceeded its requested page size",
                ));
            }
            for entry in items {
                let Some(turn_id) = entry["turnId"].as_str() else {
                    continue;
                };
                if turn_id.is_empty() || turn_id.len() > MAX_HISTORY_ID_BYTES {
                    return Err(protocol("thread/items/list returned an invalid turn id"));
                }
                if !completed_turns.contains(turn_id) {
                    continue;
                }
                let item = &entry["item"];
                match item["type"].as_str() {
                    Some("userMessage") => {
                        if let Some(content) = item["content"].as_array() {
                            let text = content
                                .iter()
                                .filter_map(|input| input["text"].as_str())
                                .collect::<Vec<_>>()
                                .join("\n");
                            if !text.is_empty() {
                                transcript.push_newest_first("User", &text);
                            }
                        }
                    }
                    Some("agentMessage") => {
                        if let Some(text) = item["text"].as_str() {
                            transcript.push_newest_first("Assistant", text);
                        }
                    }
                    _ => {}
                }
            }
            let next = bounded_cursor(&response["nextCursor"])?;
            if let Some(next) = &next {
                if !seen_cursors.insert(next.clone()) {
                    return Err(protocol("thread/items/list repeated its pagination cursor"));
                }
            }
            if transcript.bytes >= MAX_CONVERSATION_BYTES || next.is_none() {
                return Ok(());
            }
            cursor = next;
        }
        Err(protocol(
            "thread/items/list exceeded the bounded pagination limit",
        ))
    }

    fn read_with_fallback(&mut self, thread_id: &str) -> Result<NamingConversation> {
        match self.read_completed(thread_id) {
            Ok(conversation) => {
                log_diagnostic(&self.diagnostics, "thread_turns_read");
                Ok(conversation)
            }
            Err(primary) => {
                if matches!(primary, MuxError::Cancelled) {
                    return Err(primary);
                }
                let Some(rollouts) = &self.rollouts else {
                    return Err(primary);
                };
                match rollouts.read_completed(thread_id) {
                    Ok(conversation) => {
                        log_diagnostic(&self.diagnostics, "thread_rollout_read");
                        Ok(conversation)
                    }
                    Err(MuxError::Cancelled) => Err(MuxError::Cancelled),
                    Err(_) => Err(primary),
                }
            }
        }
    }

    /// Generates and strictly validates a title using an ephemeral Luna thread.
    pub fn generate_name(&mut self, conversation: &NamingConversation) -> Result<String> {
        if conversation.transcript.trim().is_empty() {
            return Err(protocol(
                "conversation has no completed user or assistant text",
            ));
        }
        let started = self.session.request(
            "thread/start",
            json!({
                "model": NAMING_MODEL,
                "ephemeral": true,
                "sandbox": "read-only",
                "approvalPolicy": "never",
                "baseInstructions": "Return only a concise descriptive tmux session title. No quotes, markup, punctuation suffix, or explanation."
            }),
        )?;
        let naming_thread = started
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .ok_or_else(|| protocol("thread/start omitted the naming thread id"))?;
        self.session.request(
            "turn/start",
            json!({
                "threadId": naming_thread,
                "model": NAMING_MODEL,
                "input": [{"type": "text", "text": conversation.transcript}],
                "outputSchema": {
                    "type": "object",
                    "properties": {"title": {"type": "string", "maxLength": MAX_NAME_CHARS}},
                    "required": ["title"],
                    "additionalProperties": false
                }
            }),
        )?;
        let completed = self.session.wait_for("turn/completed", naming_thread)?;
        let output = completed
            .pointer("/turn/items")
            .and_then(Value::as_array)
            .and_then(|items| {
                items.iter().rev().find_map(|item| {
                    (item["type"] == "agentMessage")
                        .then(|| item["text"].as_str())
                        .flatten()
                })
            })
            .ok_or_else(|| protocol("Luna completed without a title"))?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct NamingOutput {
            title: String,
        }
        let output: NamingOutput = serde_json::from_str(output)
            .map_err(|_| protocol("Luna returned malformed structured output"))?;
        validate_name(&output.title)
    }
}

fn take_history_request(remaining: &mut usize) -> Result<()> {
    if *remaining == 0 {
        return Err(protocol("conversation history exceeded its request budget"));
    }
    *remaining -= 1;
    Ok(())
}

fn bounded_cursor(value: &Value) -> Result<Option<String>> {
    if value.is_null() {
        return Ok(None);
    }
    let Some(cursor) = value.as_str() else {
        return Err(protocol("app-server returned an invalid pagination cursor"));
    };
    if cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES {
        return Err(protocol("app-server returned an invalid pagination cursor"));
    }
    Ok(Some(cursor.to_owned()))
}

fn validate_name(title: &str) -> Result<String> {
    let title = title.trim();
    let valid = !title.is_empty()
        && title.chars().count() <= MAX_NAME_CHARS
        && !title.chars().any(char::is_control)
        && !title.chars().any(is_unsafe_format_character)
        && !title.contains(['*', '#', '[', ']', '<', '>'])
        && !title.starts_with(['"', '\'', '`'])
        && !title.ends_with(['"', '\'', '`'])
        && title.chars().last().is_some_and(char::is_alphanumeric);
    valid
        .then(|| title.to_owned())
        .ok_or_else(|| protocol("Luna returned an invalid title"))
}

fn is_unsafe_format_character(character: char) -> bool {
    matches!(
        character,
        '\u{2028}' | '\u{2029}'
            | '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}' | '\u{feff}'
    )
}

fn protocol(message: &str) -> MuxError {
    MuxError::Command(format!("Codex app-server protocol error: {message}"))
}

#[cfg(test)]
mod transport_tests {
    use super::*;

    static PROCESS_ENVIRONMENT_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    #[ignore]
    fn post_exec_descriptor_holder_helper() {
        let Ok(mode) = env::var("CODEX_MUX_DESCRIPTOR_HELPER") else {
            return;
        };
        let _rollout = (mode == "rollout").then(|| {
            fs::OpenOptions::new()
                .read(true)
                .append(true)
                .open(env::var_os("CODEX_MUX_HELPER_PATH").unwrap())
                .unwrap()
        });
        let _socket = (mode == "socket").then(|| {
            std::os::unix::net::UnixStream::connect(env::var_os("CODEX_MUX_HELPER_PATH").unwrap())
                .unwrap()
        });
        thread::sleep(Duration::from_secs(30));
    }

    fn snapshot_pane(title: &str) -> Pane {
        Pane {
            id: PaneId::new("%4242").unwrap(),
            session_id: crate::domain::SessionId::new("$1").unwrap(),
            title: Some(title.to_owned()),
            generated_title: None,
            generated_at_unix: None,
            immediate_naming: false,
            manual_name: false,
            manual_name_source: None,
            manual_name_pid: None,
            manual_name_pid_raw: String::new(),
            manual_name_session: None,
            manual_name_session_raw: String::new(),
            unpin_waiting: false,
            unpin_waiting_title: None,
            unpin_waiting_pid: None,
            unpin_waiting_session: None,
            pane_pid: std::process::id(),
            current_path: PathBuf::from("/tmp"),
        }
    }

    #[test]
    fn transcript_budget_retains_recent_messages_in_chronological_order() {
        let mut chronological = RecentTranscript::default();
        chronological.push("User", &"old-one ".repeat(MAX_CONVERSATION_BYTES));
        chronological.push("Assistant", &"old-two ".repeat(MAX_CONVERSATION_BYTES));
        chronological.push("User", &format!("recent question {}", "q".repeat(4_000)));
        chronological.push("Assistant", &format!("recent answer {}", "a".repeat(4_000)));
        chronological.push("User", &format!("recent followup {}", "f".repeat(4_000)));
        let rendered = chronological.render();
        assert!(!rendered.contains("old-one"));
        assert!(!rendered.contains("old-two"));
        assert!(rendered.find("recent question") < rendered.find("recent answer"));

        let mut newest_first = RecentTranscript::default();
        newest_first
            .push_newest_first("Assistant", &format!("newest answer {}", "a".repeat(4_000)));
        newest_first.push_newest_first("User", &format!("newest question {}", "q".repeat(4_000)));
        newest_first.push_newest_first(
            "Assistant",
            &format!("older answer {}", "o".repeat(MAX_CONVERSATION_BYTES)),
        );
        let rendered = newest_first.render_reversed();
        assert!(!rendered.contains("older answer"));
        assert!(rendered.find("newest question") < rendered.find("newest answer"));
    }

    #[test]
    fn shell_snapshot_resolves_arbitrary_title_to_exact_thread() {
        let root = std::env::temp_dir().join(format!(
            "codex-mux-shell-snapshot-{}-{}",
            std::process::id(),
            unix_seconds(SystemTime::now())
        ));
        fs::create_dir_all(&root).unwrap();
        let thread_id = "01a01001-2dbb-74e2-86ab-996b31234567";
        fs::write(
            root.join(format!("{thread_id}.123.sh")),
            "export OTHER='private'\nexport TMUX_PANE='%4242'\n",
        )
        .unwrap();
        let store = ShellSnapshotStore::at(root.clone());
        let pane = snapshot_pane("c");
        assert_eq!(store.resolve(&pane).unwrap().as_deref(), Some(thread_id));
        let target =
            NamingTarget::from_verified_thread(&pane, store.resolve(&pane).unwrap().unwrap())
                .unwrap();
        assert_eq!(target.thread_hint, thread_id);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shell_snapshot_rejects_ambiguous_or_untrusted_evidence() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "codex-mux-shell-snapshot-reject-{}-{}",
            std::process::id(),
            unix_seconds(SystemTime::now())
        ));
        fs::create_dir_all(&root).unwrap();
        for thread_id in [
            "01a01001-2dbb-74e2-86ab-996b31234567",
            "01a01001-2dbb-74e2-86ab-996b37654321",
        ] {
            fs::write(
                root.join(format!("{thread_id}.123.sh")),
                "export TMUX_PANE='%4242'\n",
            )
            .unwrap();
        }
        let store = ShellSnapshotStore::at(root.clone());
        assert!(store.resolve(&snapshot_pane("c")).is_err());

        for entry in fs::read_dir(&root).unwrap() {
            fs::remove_file(entry.unwrap().path()).unwrap();
        }
        let path = root.join("01a01001-2dbb-74e2-86ab-996b31234567.123.sh");
        fs::write(&path, "export TMUX_PANE='%4242'\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(store.resolve(&snapshot_pane("c")).unwrap(), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn descendant_environment_is_only_an_untrusted_identity_claim() {
        let _serial = PROCESS_ENVIRONMENT_TEST_LOCK.lock().unwrap();
        let thread_id = "01a01001-2dbb-74e2-86ab-996b31234567";
        let mut child = Command::new("sleep")
            .arg("30")
            .env("TMUX_PANE", "%4242")
            .env("CODEX_SESSION_ID", thread_id)
            .spawn()
            .unwrap();
        let pane = snapshot_pane("c");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let resolved = loop {
            match ProcessEnvironmentStore.resolve(&pane).unwrap() {
                Some(resolved) => break Some(resolved),
                None if std::time::Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(2));
                }
                None => break None,
            }
        };
        let sleep = fs::metadata("/usr/bin/sleep").unwrap();
        let trusted = ProcessRolloutStore {
            executables: vec![(sleep.dev(), sleep.ino())],
            rollout_root: PathBuf::from("/nonexistent-codex-mux-rollouts"),
        };
        assert_eq!(trusted.resolve(&pane).unwrap(), None);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(resolved.as_deref(), Some(thread_id));
        assert!(NamingTarget::from_pane(&pane).is_none());
    }

    #[test]
    fn exact_descendant_rollout_fd_resolves_only_the_root_thread() {
        use std::os::unix::fs::PermissionsExt;

        let _serial = PROCESS_ENVIRONMENT_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "codex-mux-rollout-fd-{}-{}",
            std::process::id(),
            unix_seconds(SystemTime::now())
        ));
        fs::create_dir_all(&root).unwrap();
        let thread_id = "01a01001-2dbb-74e2-86ab-996b31234567";
        let rollout = root.join(format!("rollout-2026-08-21T00-00-00-{thread_id}.jsonl"));
        fs::write(
            &rollout,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{thread_id}\",\"source\":\"cli\"}}}}\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&rollout, fs::Permissions::from_mode(0o600)).unwrap();
        let executable = root.join("descriptor-helper");
        fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let metadata = fs::metadata(&executable).unwrap();
        let store = ProcessRolloutStore {
            executables: vec![(metadata.dev(), metadata.ino())],
            rollout_root: root.clone(),
        };
        let pane = snapshot_pane("c");
        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            store.resolve_cancellable(&pane, &cancelled),
            Err(MuxError::Cancelled)
        ));
        let mut inherited = Command::new(&executable)
            .args([
                "smart_naming::transport_tests::post_exec_descriptor_holder_helper",
                "--exact",
                "--ignored",
            ])
            .env("CODEX_MUX_DESCRIPTOR_HELPER", "none")
            .stdin(fs::File::open(&rollout).unwrap())
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_millis(20));
        assert_eq!(store.resolve(&pane).unwrap(), None);
        let _ = inherited.kill();
        let _ = inherited.wait();

        let mut child = Command::new(&executable)
            .args([
                "smart_naming::transport_tests::post_exec_descriptor_holder_helper",
                "--exact",
                "--ignored",
            ])
            .env("CODEX_MUX_DESCRIPTOR_HELPER", "rollout")
            .env("CODEX_MUX_HELPER_PATH", &rollout)
            .stdin(fs::File::open(&rollout).unwrap())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let resolved = loop {
            match store
                .resolve_with_socket_diagnostics(
                    &pane,
                    Err(protocol("sock_diag is unavailable")),
                    &AtomicBool::new(false),
                )
                .unwrap()
            {
                Some(resolved) => break Some(resolved),
                None if std::time::Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(2));
                }
                None => break None,
            }
        };
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(resolved.as_deref(), Some(thread_id));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn private_control_socket_peer_resolves_a_legacy_client_thread() {
        use std::os::unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        };

        let _serial = PROCESS_ENVIRONMENT_TEST_LOCK.lock().unwrap();
        let codex_home = std::env::temp_dir().join(format!(
            "codex-mux-control-peer-{}-{}",
            std::process::id(),
            unix_seconds(SystemTime::now())
        ));
        let root = codex_home.join("sessions");
        let control = codex_home.join("app-server-control");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&control).unwrap();
        let socket_path = control.join("app-server-control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let thread_id = "01a01001-2dbb-74e2-86ab-996b31234567";
        let rollout = root.join(format!("rollout-2026-08-21T00-00-00-{thread_id}.jsonl"));
        fs::write(
            &rollout,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{thread_id}\",\"source\":\"cli\"}}}}\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&rollout, fs::Permissions::from_mode(0o600)).unwrap();
        let held_rollout = fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&rollout)
            .unwrap();
        let executable = codex_home.join("descriptor-helper");
        fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let metadata = fs::metadata(&executable).unwrap();
        let untrusted_store = ProcessRolloutStore {
            executables: vec![(metadata.dev(), metadata.ino())],
            rollout_root: root.clone(),
        };

        // A peer that merely happens to live in the control-server process is
        // not authoritative unless this exact connection names the private
        // control socket.
        let (unrelated_client, unrelated_server) = UnixStream::pair().unwrap();
        let unrelated_client: OwnedFd = unrelated_client.into();
        let mut unrelated_child = Command::new(&executable)
            .args([
                "smart_naming::transport_tests::post_exec_descriptor_holder_helper",
                "--exact",
                "--ignored",
            ])
            .env("CODEX_MUX_DESCRIPTOR_HELPER", "none")
            .stdout(Stdio::from(unrelated_client))
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_millis(20));
        assert_eq!(untrusted_store.resolve(&snapshot_pane("c")).unwrap(), None);
        let _ = unrelated_child.kill();
        let _ = unrelated_child.wait();
        drop(unrelated_server);

        let mut child = Command::new(&executable)
            .args([
                "smart_naming::transport_tests::post_exec_descriptor_holder_helper",
                "--exact",
                "--ignored",
            ])
            .env("CODEX_MUX_DESCRIPTOR_HELPER", "socket")
            .env("CODEX_MUX_HELPER_PATH", &socket_path)
            .spawn()
            .unwrap();
        let (server, _) = listener.accept().unwrap();
        let mut pane = snapshot_pane("c");
        pane.pane_pid = child.id();
        assert_eq!(untrusted_store.resolve(&pane).unwrap(), None);
        let peer_executable = fs::metadata(std::env::current_exe().unwrap()).unwrap();
        let store = ProcessRolloutStore {
            executables: vec![
                (metadata.dev(), metadata.ino()),
                (peer_executable.dev(), peer_executable.ino()),
            ],
            rollout_root: root.clone(),
        };
        let server_inode = fs::metadata(format!("/proc/self/fd/{}", server.as_raw_fd()))
            .unwrap()
            .ino() as u32;
        let cancelled = AtomicBool::new(false);
        let mut budget = ProcessScanBudget::new(&cancelled);
        let client_inode = *process_socket_inodes(child.id(), &mut budget)
            .unwrap()
            .iter()
            .find(|inode| {
                unix_socket_diagnostics(None).unwrap().peers.get(inode) == Some(&server_inode)
            })
            .unwrap();
        let sockets = unix_socket_diagnostics(None).unwrap();
        assert_eq!(sockets.peers.get(&client_inode), Some(&server_inode));
        assert_eq!(
            sockets.names.get(&server_inode).map(Vec::as_slice),
            Some(socket_path.as_os_str().as_bytes())
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let resolved = loop {
            match store.resolve(&pane).unwrap() {
                Some(resolved) => break Some(resolved),
                None if std::time::Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(2));
                }
                None => break None,
            }
        };
        let _ = child.kill();
        let _ = child.wait();
        drop((held_rollout, listener, server));
        assert_eq!(resolved.as_deref(), Some(thread_id));
        fs::remove_dir_all(codex_home).unwrap();
    }

    #[test]
    fn descendant_environment_fails_closed_on_conflicting_sessions() {
        let _serial = PROCESS_ENVIRONMENT_TEST_LOCK.lock().unwrap();
        let mut children = [
            "01a01001-2dbb-74e2-86ab-996b31234567",
            "01a01001-2dbb-74e2-86ab-996b37654321",
        ]
        .map(|thread_id| {
            Command::new("sleep")
                .arg("30")
                .env("TMUX_PANE", "%4242")
                .env("CODEX_SESSION_ID", thread_id)
                .spawn()
                .unwrap()
        });
        for child in &children {
            for _ in 0..100 {
                if process_identity_environment(child.id()).unwrap().is_some() {
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
            assert!(process_identity_environment(child.id()).unwrap().is_some());
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let result = loop {
            let result = ProcessEnvironmentStore.resolve(&snapshot_pane("c"));
            if result.is_err() || std::time::Instant::now() >= deadline {
                break result;
            }
            thread::sleep(Duration::from_millis(2));
        };
        for child in &mut children {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(result.is_err());
    }

    #[test]
    fn diagnostics_are_private_bounded_and_contain_only_reason_codes() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("codex-mux-naming-log-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let log = NamingDiagnostics::at(root.join("smart-naming.log"));
        log.event("provider_ready");
        log.event("thread_state_db_miss");
        assert_eq!(
            log.latest().unwrap().split_whitespace().last(),
            Some("thread_state_db_miss")
        );
        assert_eq!(
            fs::metadata(log.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            !fs::read_to_string(log.path())
                .unwrap()
                .contains("01a01001-2dbb-74e2-86ab-996b31234567")
        );
        fs::write(log.path(), vec![b'x'; NAMING_LOG_MAX_BYTES as usize]).unwrap();
        log.event("provider_ready");
        assert!(log.path().with_extension("log.1").exists());
        assert!(fs::metadata(log.path()).unwrap().len() < 100);

        fs::remove_file(log.path()).unwrap();
        fs::remove_file(log.path().with_extension("log.1")).unwrap();
        fs::create_dir(log.path().with_extension("log.1")).unwrap();
        fs::write(log.path(), vec![b'x'; NAMING_LOG_MAX_BYTES as usize]).unwrap();
        log.event("provider_ready");
        assert_eq!(
            fs::metadata(log.path()).unwrap().len(),
            NAMING_LOG_MAX_BYTES
        );

        fs::remove_file(log.path()).unwrap();
        let secret = root.join("secret");
        fs::write(&secret, "sensitive-last-line\n").unwrap();
        std::os::unix::fs::symlink(&secret, log.path()).unwrap();
        assert_eq!(log.latest(), None);
        log.event("provider_ready");
        assert_eq!(fs::read_to_string(secret).unwrap(), "sensitive-last-line\n");

        fs::remove_file(log.path()).unwrap();
        assert!(
            Command::new("mkfifo")
                .arg(log.path())
                .status()
                .unwrap()
                .success()
        );
        log.event("provider_ready");
        assert_eq!(log.latest(), None);
        fs::remove_file(log.path()).unwrap();
        fs::write(log.path(), "123 provider_ready\x1b[2J\n").unwrap();
        assert_eq!(log.latest(), None);
        fs::remove_dir_all(root).unwrap();
    }
    use std::{ffi::OsStr, io::Cursor};

    #[test]
    fn app_server_uses_the_configured_launch_executable() {
        let launcher = Path::new("/opt/codex/bin/codex-launcher");
        let command = app_server_command(launcher);

        assert_eq!(command.get_program(), launcher.as_os_str());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("app-server"),
                OsStr::new("--listen"),
                OsStr::new("stdio://"),
            ]
        );
    }

    #[test]
    fn fatal_frame_marks_transport_unhealthy_before_delivery() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let healthy = AtomicBool::new(true);

        assert!(!forward_app_server_message(
            &sender,
            &healthy,
            Err("fatal framing error".to_owned()),
        ));
        assert!(receiver.recv().unwrap().is_err());
        assert!(!healthy.load(Ordering::Acquire));
    }

    #[test]
    fn unhealthy_worker_exits_after_one_cooldown() {
        struct UnhealthyNamer;
        impl ConversationNamer for UnhealthyNamer {
            fn read(&mut self, _: &NamingTarget) -> Result<NamingConversation> {
                Err(protocol("fatal transport failure"))
            }

            fn name(&mut self, _: &NamingConversation) -> Result<String> {
                unreachable!()
            }

            fn is_healthy(&self) -> bool {
                false
            }
        }

        let target = target_created_at(0);
        let worker = NamingWorker::spawn_with_intervals(
            |_| Ok(UnhealthyNamer),
            move || Ok(vec![target.clone()]),
            Duration::from_millis(1),
            Duration::from_millis(5),
            Duration::from_millis(10),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !worker.is_finished() && std::time::Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(worker.is_finished());
        worker.stop();
    }

    #[test]
    fn wake_does_not_shorten_an_unhealthy_provider_cooldown() {
        let (sender, receiver) = mpsc::channel();
        let retry_interval = Duration::from_millis(120);
        let started = std::time::Instant::now();
        let waiter = thread::spawn(move || wait_for_stop_ignoring_wakes(&receiver, retry_interval));
        thread::sleep(Duration::from_millis(20));
        sender.send(NamingCommand::Wake).unwrap();

        assert!(!waiter.join().unwrap());
        assert!(started.elapsed() >= retry_interval);
    }

    #[test]
    fn transient_failures_wait_for_the_retry_interval() {
        struct FailingNamer(Arc<Mutex<Vec<std::time::Instant>>>);
        impl ConversationNamer for FailingNamer {
            fn read(&mut self, _: &NamingTarget) -> Result<NamingConversation> {
                self.0.lock().unwrap().push(std::time::Instant::now());
                Err(protocol("transient failure"))
            }

            fn name(&mut self, _: &NamingConversation) -> Result<String> {
                unreachable!()
            }
        }

        assert_eq!(NAMING_RETRY_INTERVAL, Duration::from_secs(60 * 60));
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let observed = attempts.clone();
        let target = target_created_at(0);
        let retry_interval = Duration::from_millis(40);
        let worker = NamingWorker::spawn_with_retry_intervals(
            move |_| Ok(FailingNamer(observed)),
            move |_| Ok(vec![target.clone()]),
            Duration::from_millis(1),
            Duration::from_millis(10),
            retry_interval,
            Duration::from_millis(10),
            None,
        );
        let retry_deadline = std::time::Instant::now() + Duration::from_secs(1);
        while attempts.lock().unwrap().len() < 2 && std::time::Instant::now() < retry_deadline {
            thread::yield_now();
        }
        worker.stop();
        let attempts = attempts.lock().unwrap();
        assert!(attempts.len() >= 2);
        assert!(attempts[1].duration_since(attempts[0]) >= retry_interval);
    }

    #[test]
    fn pending_conversations_retry_before_the_failure_interval() {
        struct PendingNamer(Arc<std::sync::atomic::AtomicUsize>);
        impl ConversationNamer for PendingNamer {
            fn read(&mut self, target: &NamingTarget) -> Result<NamingConversation> {
                let attempt = self.0.fetch_add(1, Ordering::SeqCst);
                Ok(NamingConversation {
                    thread_id: target.thread_hint.clone(),
                    transcript: if attempt == 0 {
                        String::new()
                    } else {
                        "completed chat".to_owned()
                    },
                })
            }

            fn name(&mut self, conversation: &NamingConversation) -> Result<String> {
                if conversation.transcript.is_empty() {
                    return Err(protocol("conversation is not ready"));
                }
                Ok("Generated title".to_owned())
            }
        }

        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = attempts.clone();
        let target = target_created_at(0);
        let worker = NamingWorker::spawn_with_retry_intervals(
            move |_| Ok(PendingNamer(observed)),
            move |_| Ok(vec![target.clone()]),
            Duration::from_millis(1),
            Duration::from_millis(10),
            Duration::from_millis(40),
            Duration::from_millis(10),
            None,
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while worker.names().lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            thread::yield_now();
        }
        let names = worker.names();
        worker.stop();

        assert!(attempts.load(Ordering::SeqCst) >= 2);
        assert_eq!(
            names
                .lock()
                .unwrap()
                .values()
                .next()
                .map(|name| name.name.as_str()),
            Some("Generated title")
        );
    }

    #[test]
    fn trigger_discovers_a_resumed_thread_without_waiting_for_the_normal_poll() {
        struct ReadyNamer;
        impl ConversationNamer for ReadyNamer {
            fn read(&mut self, target: &NamingTarget) -> Result<NamingConversation> {
                Ok(NamingConversation {
                    thread_id: target.thread_hint.clone(),
                    transcript: "completed resumed chat".to_owned(),
                })
            }

            fn name(&mut self, _: &NamingConversation) -> Result<String> {
                Ok("Resumed work".to_owned())
            }
        }

        let target = Arc::new(Mutex::new(None));
        let discovered = target.clone();
        let worker = NamingWorker::spawn_with_intervals(
            move |_| Ok(ReadyNamer),
            move || Ok(discovered.lock().unwrap().clone().into_iter().collect()),
            Duration::from_secs(60),
            Duration::from_secs(60),
            Duration::from_secs(60),
        );
        *target.lock().unwrap() = Some(target_created_at(0));
        worker.trigger();

        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while worker.names().lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(
            !worker.names().lock().unwrap().is_empty(),
            "wake did not bypass the sixty-second discovery interval"
        );
        worker.stop();
    }

    #[test]
    fn successful_attempts_wait_for_the_refresh_interval() {
        struct SuccessfulNamer(Arc<Mutex<Vec<std::time::Instant>>>);
        impl ConversationNamer for SuccessfulNamer {
            fn read(&mut self, target: &NamingTarget) -> Result<NamingConversation> {
                self.0.lock().unwrap().push(std::time::Instant::now());
                Ok(NamingConversation {
                    thread_id: target.thread_hint.clone(),
                    transcript: "completed chat".to_owned(),
                })
            }

            fn name(&mut self, _: &NamingConversation) -> Result<String> {
                Ok("Generated title".to_owned())
            }
        }

        let attempts = Arc::new(Mutex::new(Vec::new()));
        let observed = attempts.clone();
        let target = target_created_at(0);
        let refresh_interval = Duration::from_millis(40);
        let worker = NamingWorker::spawn_with_intervals(
            move |_| Ok(SuccessfulNamer(observed)),
            move || Ok(vec![target.clone()]),
            Duration::from_millis(1),
            refresh_interval,
            Duration::from_secs(2),
        );
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while attempts.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
            thread::yield_now();
        }
        worker.stop();
        let attempts = attempts.lock().unwrap();
        assert!(attempts.len() >= 2);
        assert!(attempts[1].duration_since(attempts[0]) >= refresh_interval);
    }

    #[test]
    fn stale_resolution_waits_for_the_retry_interval() {
        struct StaleNamer(Arc<Mutex<Vec<std::time::Instant>>>);
        impl ConversationNamer for StaleNamer {
            fn resolve(&mut self, _: &NamingTarget) -> Result<String> {
                Ok("00000000-0000-7000-8000-000000000001".to_owned())
            }

            fn read(&mut self, target: &NamingTarget) -> Result<NamingConversation> {
                self.0.lock().unwrap().push(std::time::Instant::now());
                Ok(NamingConversation {
                    thread_id: target.thread_hint.clone(),
                    transcript: "completed chat".to_owned(),
                })
            }

            fn name(&mut self, _: &NamingConversation) -> Result<String> {
                Ok("Stale title".to_owned())
            }
        }

        let attempts = Arc::new(Mutex::new(Vec::new()));
        let observed = attempts.clone();
        let target = target_created_at(0);
        let retry_interval = Duration::from_millis(40);
        let worker = NamingWorker::spawn_with_intervals(
            move |_| Ok(StaleNamer(observed)),
            move || Ok(vec![target.clone()]),
            Duration::from_millis(1),
            Duration::from_millis(10),
            retry_interval,
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while attempts.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
            thread::yield_now();
        }
        worker.stop();
        let attempts = attempts.lock().unwrap();
        assert!(attempts.len() >= 2);
        assert!(attempts[1].duration_since(attempts[0]) >= retry_interval);
    }

    #[test]
    fn final_discovery_failure_waits_for_the_retry_interval() {
        struct SuccessfulNamer(Arc<Mutex<Vec<std::time::Instant>>>);
        impl ConversationNamer for SuccessfulNamer {
            fn read(&mut self, target: &NamingTarget) -> Result<NamingConversation> {
                self.0.lock().unwrap().push(std::time::Instant::now());
                Ok(NamingConversation {
                    thread_id: target.thread_hint.clone(),
                    transcript: "completed chat".to_owned(),
                })
            }

            fn name(&mut self, _: &NamingConversation) -> Result<String> {
                Ok("Generated title".to_owned())
            }
        }

        let attempts = Arc::new(Mutex::new(Vec::new()));
        let observed = attempts.clone();
        let target = target_created_at(0);
        let discovered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let discovery_calls = discovered.clone();
        let first_discovery = Arc::new(Mutex::new(None));
        let observed_discovery = first_discovery.clone();
        let retry_interval = Duration::from_millis(40);
        let worker = NamingWorker::spawn_with_intervals(
            move |_| Ok(SuccessfulNamer(observed)),
            move || {
                let call = discovery_calls.fetch_add(1, Ordering::SeqCst);
                if call % 2 == 0 {
                    if call == 0 {
                        *observed_discovery.lock().unwrap() = Some(std::time::Instant::now());
                    }
                    Ok(vec![target.clone()])
                } else {
                    Err(protocol("final discovery failed"))
                }
            },
            Duration::from_millis(1),
            Duration::from_millis(10),
            retry_interval,
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while attempts.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
            thread::yield_now();
        }
        worker.stop();
        let attempts = attempts.lock().unwrap();
        assert!(attempts.len() >= 2);
        let first_discovery = first_discovery
            .lock()
            .unwrap()
            .expect("initial discovery was not observed");
        assert!(attempts[1].duration_since(first_discovery) >= retry_interval);
    }

    fn target_created_at(created_at_unix: u64) -> NamingTarget {
        let milliseconds = created_at_unix * 1000;
        let thread = format!(
            "{:08x}-{:04x}-7000-8000-000000000000",
            milliseconds >> 16,
            milliseconds & 0xffff
        );
        NamingTarget {
            pane_id: PaneId::new("%1").unwrap(),
            pane_title: thread.clone(),
            thread_hint: thread,
            cwd: "/work/project".into(),
            generated_name: None,
            generated_at_unix: None,
            immediate_naming: false,
        }
    }

    #[test]
    fn new_titles_are_due_immediately_and_existing_titles_refresh_every_thirty_minutes() {
        let now = 1_800_000_000;
        assert!(naming_is_due(&target_created_at(now), now));

        let mut existing = target_created_at(now);
        existing.generated_name = Some("Existing title".to_owned());
        existing.generated_at_unix = Some(now - 1_799);
        assert!(!naming_is_due(&existing, now));
        existing.generated_at_unix = Some(now - 1_800);
        assert!(naming_is_due(&existing, now));
    }

    #[test]
    fn accepts_thread_read_responses_larger_than_the_old_transport_limit() {
        let payload = "x".repeat(512 * 1024);
        let encoded =
            serde_json::to_vec(&json!({"id": 7, "result": {"payload": payload}})).unwrap();
        let mut input = encoded;
        input.push(b'\n');

        let message = read_app_server_message(&mut Cursor::new(input))
            .unwrap()
            .unwrap();
        assert_eq!(message["id"], 7);
        assert_eq!(
            message["result"]["payload"].as_str().unwrap().len(),
            512 * 1024
        );
    }

    #[test]
    fn noisy_multi_turn_traffic_retains_only_future_consumers() {
        let mut pending = VecDeque::new();
        for index in 0..500 {
            let delta = json!({"method": "item/agentMessage/delta", "params": {"threadId": "naming", "delta": index}});
            assert!(!should_retain(&delta));
        }
        assert!(!should_retain(&json!({
            "id": 91,
            "method": "item/commandExecution/requestApproval",
            "params": {"threadId": "naming"}
        })));
        for message in [
            json!({"id": 7, "result": {}}),
            json!({"method": "turn/completed", "params": {"threadId": "other"}}),
        ] {
            if should_retain(&message) {
                retain_pending(&mut pending, message).unwrap();
            }
        }
        assert_eq!(pending.len(), 2);
        pending.retain(|message| {
            message.pointer("/params/threadId").and_then(Value::as_str) != Some("other")
        });
        assert_eq!(pending, VecDeque::from([json!({"id": 7, "result": {}})]));
    }

    #[test]
    fn cancelled_rollout_scan_stops_before_traversal() {
        let root = std::env::temp_dir().join(format!(
            "codex-mux-cancelled-rollout-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("2026/08/21")).unwrap();
        let cancelled = Arc::new(AtomicBool::new(true));
        let store = RolloutStore::at(&root).with_cancellation(cancelled);
        assert!(matches!(
            store.resolve_prefix("01a01001"),
            Err(MuxError::Cancelled)
        ));
        fs::remove_dir_all(root).unwrap();
    }
}
