//! Privacy-bounded conversation extraction and Codex app-server naming contract.

use std::{
    collections::{HashMap, VecDeque, hash_map::DefaultHasher},
    env, fs,
    hash::{Hash, Hasher},
    io::{BufRead, BufReader, Read, Write},
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

use serde::Deserialize;
use serde_json::{Value, json};

use crate::domain::{Pane, PaneId};
use crate::{MuxError, Result};

/// Codex model used exclusively for background session naming.
pub const NAMING_MODEL: &str = "gpt-5.6-luna";
/// Maximum UTF-8 payload sent to the naming model.
pub const MAX_CONVERSATION_BYTES: usize = 12 * 1024;
/// Maximum accepted generated title length in Unicode scalar values.
pub const MAX_NAME_CHARS: usize = 48;
const THREAD_LIST_PAGE_SIZE: u32 = 100;
const MAX_THREAD_LIST_PAGES: usize = 20;
const MAX_APP_SERVER_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
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
    "identity_changed",
    "name_published",
    "process_rejected",
    "pane_command_rejected",
    "pane_mode_active",
    "cursor_hidden",
    "state_read_failed",
    "composer_shape_rejected",
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
            "capabilities": {"experimentalApi": false}
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
                return Err(protocol("app-server request cancelled"));
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
        let pane_title = pane.title.as_deref()?.trim();
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
        self.read_completed(&thread_id)
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
}

enum NamingCommand {
    Stop,
    Wake,
}

impl NamingWorker {
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
        discover: D,
        poll_interval: Duration,
        diagnostics: Option<NamingDiagnostics>,
    ) -> Self
    where
        N: ConversationNamer,
        F: FnOnce(Arc<AtomicBool>) -> Result<N> + Send + 'static,
        D: FnMut() -> Result<Vec<NamingTarget>> + Send + 'static,
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
        discover: D,
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
            discover,
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
        D: FnMut() -> Result<Vec<NamingTarget>> + Send + 'static,
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
                let Ok(targets) = discover() else {
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
                    let Ok(current) = discover() else {
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
        }
    }

    /// Returns the shared generated-name snapshot.
    #[must_use]
    pub fn names(&self) -> GeneratedNames {
        self.names.clone()
    }

    /// Wakes discovery early after a pane acquires an exact resumed thread.
    pub fn trigger(&self) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(NamingCommand::Wake);
        }
    }

    /// Reports whether the worker lane ended, including provider startup failure.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Stops and joins the worker before returning.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.cancelled.store(true, Ordering::Release);
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

/// Reads completed turns and asks an ephemeral Luna thread for a short title.
pub struct AppServerNamer<S> {
    session: S,
}

impl<S: AppServerSession> AppServerNamer<S> {
    /// Wraps an initialized, version-compatible app-server session.
    #[must_use]
    pub const fn new(session: S) -> Self {
        Self { session }
    }

    fn resolve_thread_id(&mut self, target: &NamingTarget) -> Result<String> {
        if looks_like_thread_id(&target.thread_hint) {
            return Ok(target.thread_hint.clone());
        }

        let mut cursor: Option<String> = None;
        let mut matched: Option<String> = None;
        for _ in 0..MAX_THREAD_LIST_PAGES {
            let response = self.session.request(
                "thread/list",
                json!({
                    "cwd": target.cwd,
                    "cursor": cursor,
                    "limit": THREAD_LIST_PAGE_SIZE,
                    "sortKey": "updated_at",
                    "sortDirection": "desc",
                    "useStateDbOnly": true
                }),
            )?;
            let threads = response["data"]
                .as_array()
                .ok_or_else(|| protocol("thread/list did not return thread data"))?;
            for thread in threads {
                let Some(id) = thread["id"].as_str() else {
                    continue;
                };
                let same_cwd = thread["cwd"]
                    .as_str()
                    .is_some_and(|cwd| Path::new(cwd) == target.cwd);
                if same_cwd && looks_like_thread_id(id) && id.starts_with(&target.thread_hint) {
                    if matched.as_deref().is_some_and(|existing| existing != id) {
                        return Err(protocol("truncated pane title matches multiple threads"));
                    }
                    matched = Some(id.to_owned());
                }
            }

            let next = response["nextCursor"].as_str().map(ToOwned::to_owned);
            if next.is_none() {
                return matched.ok_or_else(|| {
                    protocol("truncated pane title did not match a thread in its working directory")
                });
            }
            if next == cursor {
                return Err(protocol("thread/list repeated its pagination cursor"));
            }
            cursor = next;
        }
        Err(protocol(
            "thread/list exceeded the bounded pagination limit",
        ))
    }

    /// Reads only structured, completed turns through `thread/read`.
    pub fn read_completed(&mut self, thread_id: &str) -> Result<NamingConversation> {
        let response = self.session.request(
            "thread/read",
            json!({"threadId": thread_id, "includeTurns": true}),
        )?;
        let turns = response
            .pointer("/thread/turns")
            .and_then(Value::as_array)
            .ok_or_else(|| protocol("thread/read did not return full turns"))?;
        let mut transcript = String::new();
        for turn in turns.iter().filter(|turn| turn["status"] == "completed") {
            let Some(items) = turn["items"].as_array() else {
                continue;
            };
            for item in items {
                match item["type"].as_str() {
                    Some("userMessage") => {
                        if let Some(content) = item["content"].as_array() {
                            for input in content {
                                if let Some(text) = input["text"].as_str() {
                                    append_bounded(&mut transcript, "User", text);
                                }
                            }
                        }
                    }
                    Some("agentMessage") => {
                        if let Some(text) = item["text"].as_str() {
                            append_bounded(&mut transcript, "Assistant", text);
                        }
                    }
                    _ => {}
                }
                if transcript.len() >= MAX_CONVERSATION_BYTES {
                    break;
                }
            }
        }
        Ok(NamingConversation {
            thread_id: thread_id.to_owned(),
            transcript,
        })
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

fn append_bounded(target: &mut String, role: &str, text: &str) {
    if target.len() >= MAX_CONVERSATION_BYTES {
        return;
    }
    let prefix = format!("{role}: ");
    if target.len() + prefix.len() + 1 >= MAX_CONVERSATION_BYTES {
        return;
    }
    target.push_str(&prefix);
    for character in text.chars() {
        if target.len() + character.len_utf8() > MAX_CONVERSATION_BYTES - 1 {
            break;
        }
        target.push(character);
    }
    target.push('\n');
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

    #[test]
    fn diagnostics_are_private_bounded_and_contain_only_reason_codes() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("codex-mux-naming-log-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let log = NamingDiagnostics::at(root.join("smart-naming.log"));
        log.event("provider_ready");
        assert_eq!(
            log.latest().unwrap().split_whitespace().last(),
            Some("provider_ready")
        );
        assert_eq!(
            fs::metadata(log.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!fs::read_to_string(log.path()).unwrap().contains("thread"));
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
            move || Ok(vec![target.clone()]),
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
            move || Ok(vec![target.clone()]),
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
}
