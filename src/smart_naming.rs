//! Privacy-bounded conversation extraction and Codex app-server naming contract.

use std::{
    collections::{HashMap, VecDeque, hash_map::DefaultHasher},
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
/// Minimum age of a new Codex thread before its first smart title.
pub const INITIAL_NAMING_DELAY: Duration = Duration::from_secs(5 * 60);
/// Interval between reconsidering an existing smart title.
pub const NAMING_REFRESH_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Cooldown before retrying a failed naming attempt or restarting an unhealthy provider.
const NAMING_RETRY_INTERVAL: Duration = Duration::from_secs(60 * 60);
type AppServerMessage = std::result::Result<Value, String>;

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
        let mut child = Command::new(codex)
            .args(["app-server", "--listen", "stdio://"])
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
}

impl NamingTarget {
    /// Extracts a target only from the supported UUID thread-title shape.
    #[must_use]
    pub fn from_pane(pane: &Pane) -> Option<Self> {
        let pane_title = pane.title.as_deref()?.trim();
        let thread_hint = thread_hint(pane_title)?;
        Some(Self {
            pane_id: pane.id.clone(),
            pane_title: pane_title.to_owned(),
            thread_hint: thread_hint.to_owned(),
            cwd: pane.current_path.clone(),
            generated_name: pane.generated_title.clone(),
            generated_at_unix: pane.generated_at_unix,
        })
    }
}

fn thread_hint(title: &str) -> Option<&str> {
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
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
    names: GeneratedNames,
    cancelled: Arc<AtomicBool>,
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
        let (stop, stopped) = mpsc::channel();
        let names: GeneratedNames = Arc::new(Mutex::new(HashMap::new()));
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let published = names.clone();
        let thread = thread::spawn(move || {
            let Ok(mut namer) = start_namer(worker_cancelled.clone()) else {
                return;
            };
            let mut cache = HashMap::<String, (u64, String)>::new();
            let mut last_attempt =
                HashMap::<(String, PathBuf), (std::time::Instant, Duration)>::new();
            loop {
                if stopped.try_recv().is_ok() {
                    break;
                }
                let Ok(targets) = discover() else {
                    if stopped.recv_timeout(poll_interval).is_ok() {
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
                for (identity, targets) in by_thread {
                    if stopped.try_recv().is_ok() {
                        return;
                    }
                    let now = unix_seconds(SystemTime::now());
                    let Some(due_target) = targets.iter().find(|target| naming_is_due(target, now))
                    else {
                        continue;
                    };
                    if last_attempt
                        .get(&identity)
                        .is_some_and(|(attempted, cooldown)| attempted.elapsed() < *cooldown)
                    {
                        continue;
                    }
                    last_attempt.insert(
                        identity.clone(),
                        (std::time::Instant::now(), retry_interval),
                    );
                    let conversation = match namer.read(due_target) {
                        Ok(conversation) => conversation,
                        Err(_) if !namer.is_healthy() => {
                            let _ = stopped.recv_timeout(retry_interval);
                            return;
                        }
                        Err(_) => continue,
                    };
                    let thread_id = conversation.thread_id.clone();
                    let fingerprint = transcript_fingerprint(&conversation.transcript);
                    let name = if let Some((cached, name)) = cache.get(&thread_id) {
                        if *cached == fingerprint {
                            name.clone()
                        } else {
                            let name = match namer.name(&conversation) {
                                Ok(name) => name,
                                Err(_) if !namer.is_healthy() => {
                                    let _ = stopped.recv_timeout(retry_interval);
                                    return;
                                }
                                Err(_) => continue,
                            };
                            cache.insert(thread_id.clone(), (fingerprint, name.clone()));
                            name
                        }
                    } else {
                        let name = match namer.name(&conversation) {
                            Ok(name) => name,
                            Err(_) if !namer.is_healthy() => {
                                let _ = stopped.recv_timeout(retry_interval);
                                return;
                            }
                            Err(_) => continue,
                        };
                        cache.insert(thread_id.clone(), (fingerprint, name.clone()));
                        name
                    };
                    let still_resolved = match namer.resolve(due_target) {
                        Ok(thread_id) => thread_id,
                        Err(_) if !namer.is_healthy() => {
                            let _ = stopped.recv_timeout(retry_interval);
                            return;
                        }
                        Err(_) => continue,
                    };
                    if still_resolved != thread_id {
                        continue;
                    }
                    let Ok(current) = discover() else {
                        continue;
                    };
                    let mut published = published.lock().unwrap();
                    let mut published_any = false;
                    for target in targets {
                        if current.contains(&target) {
                            published_any = true;
                            published.insert(
                                target.pane_id,
                                GeneratedName {
                                    thread_id: thread_id.clone(),
                                    source_title: target.pane_title,
                                    source_cwd: target.cwd,
                                    name: name.clone(),
                                    generated_at_unix: now,
                                },
                            );
                        }
                    }
                    if published_any {
                        last_attempt
                            .insert(identity, (std::time::Instant::now(), refresh_interval));
                    }
                }
                if stopped.recv_timeout(poll_interval).is_ok() {
                    break;
                }
            }
        });
        Self {
            stop: Some(stop),
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
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
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
    if target.generated_name.is_some() {
        return target.generated_at_unix.is_none_or(|generated_at| {
            now_unix.saturating_sub(generated_at) >= NAMING_REFRESH_INTERVAL.as_secs()
        });
    }
    thread_created_at_unix(&target.thread_hint).is_some_and(|created_at| {
        now_unix.saturating_sub(created_at) >= INITIAL_NAMING_DELAY.as_secs()
    })
}

fn thread_created_at_unix(thread_hint: &str) -> Option<u64> {
    let timestamp = thread_hint
        .chars()
        .filter(|character| *character != '-')
        .take(12)
        .collect::<String>();
    (timestamp.len() == 12)
        .then(|| u64::from_str_radix(&timestamp, 16).ok())
        .flatten()
        .map(|milliseconds| milliseconds / 1000)
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

fn looks_like_thread_id(value: &str) -> bool {
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
    use std::io::Cursor;

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
        let worker = NamingWorker::spawn_with_intervals(
            move |_| Ok(FailingNamer(observed)),
            move || Ok(vec![target.clone()]),
            Duration::from_millis(1),
            Duration::from_millis(10),
            retry_interval,
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
        }
    }

    #[test]
    fn new_titles_wait_five_minutes_and_existing_titles_refresh_every_thirty_minutes() {
        let now = 1_800_000_000;
        assert!(!naming_is_due(&target_created_at(now - 299), now));
        assert!(naming_is_due(&target_created_at(now - 300), now));

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
