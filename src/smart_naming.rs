//! Privacy-bounded conversation extraction and Codex app-server naming contract.

use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{MuxError, Result};

/// Codex model used exclusively for background session naming.
pub const NAMING_MODEL: &str = "gpt-5.6-luna";
/// Maximum UTF-8 payload sent to the naming model.
pub const MAX_CONVERSATION_BYTES: usize = 12 * 1024;
/// Maximum accepted generated title length in Unicode scalar values.
pub const MAX_NAME_CHARS: usize = 48;

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
    messages: Option<Receiver<Value>>,
    pending: VecDeque<Value>,
    reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    next_id: u64,
}

impl AppServerProcess {
    /// Starts app-server with local stdio and the user's existing Codex authentication.
    pub fn spawn(codex: &Path) -> Result<Self> {
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
        let reader = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                let Ok(read) = reader
                    .by_ref()
                    .take(256 * 1024 + 1)
                    .read_until(b'\n', &mut line)
                else {
                    break;
                };
                if read == 0 || line.len() > 256 * 1024 || !line.ends_with(b"\n") {
                    break;
                }
                if let Ok(message) = serde_json::from_slice(&line) {
                    if sender.send(message).is_err() {
                        break;
                    }
                }
            }
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
            next_id: 1,
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
            self.receive_matching(Duration::from_secs(3), |message| message["id"] == id)?;
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
            .map_err(|source| MuxError::Filesystem {
                path: Path::new("codex app-server stdin").to_owned(),
                source,
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
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let message = self
                .messages
                .as_ref()
                .expect("receiver exists")
                .recv_timeout(remaining)
                .map_err(|_| {
                    let detail = String::from_utf8_lossy(&self.stderr.lock().unwrap())
                        .trim()
                        .to_owned();
                    protocol(if detail.is_empty() {
                        "app-server readiness timed out"
                    } else {
                        &detail
                    })
                })?;
            if matches(&message) {
                return Ok(message);
            }
            if should_retain(&message) {
                retain_pending(&mut self.pending, message)?;
            }
        }
    }
}

impl AppServerSession for AppServerProcess {
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.send_request(method, params)?;
        let response =
            self.receive_matching(Duration::from_secs(10), |message| message["id"] == id)?;
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
}

/// A completed, privacy-bounded conversation ready for naming.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamingConversation {
    /// Stable source Codex thread identifier.
    pub thread_id: String,
    /// Bounded plain user/assistant transcript; never persisted by this crate.
    pub transcript: String,
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
