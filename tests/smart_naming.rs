use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use codex_mux::{
    Result,
    smart_naming::{
        AppServerNamer, AppServerSession, MAX_CONVERSATION_BYTES, NAMING_MODEL, NamingConversation,
    },
};
use serde_json::{Value, json};

struct FakeSession {
    replies: VecDeque<Value>,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}

impl AppServerSession for FakeSession {
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.calls.lock().unwrap().push((method.to_owned(), params));
        Ok(self.replies.pop_front().unwrap())
    }
    fn wait_for(&mut self, method: &str, thread_id: &str) -> Result<Value> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_owned(), json!({"threadId": thread_id})));
        Ok(self.replies.pop_front().unwrap())
    }
}

#[test]
fn reads_only_completed_user_and_assistant_text_with_a_hard_bound() {
    let huge = "x".repeat(MAX_CONVERSATION_BYTES * 2);
    let session = FakeSession {
        replies: VecDeque::from([json!({"thread": {"turns": [
            {"status": "failed", "items": [{"type": "userMessage", "content": [{"type": "text", "text": "secret draft"}]}]},
            {"status": "completed", "items": [
                {"type": "userMessage", "content": [{"type": "text", "text": "make switching faster"}]},
                {"type": "commandExecution", "command": "ignored"},
                {"type": "agentMessage", "text": huge}
            ]}
        ]}})]),
        calls: Arc::default(),
    };
    let mut namer = AppServerNamer::new(session);
    let conversation = namer.read_completed("source-thread").unwrap();
    assert!(
        conversation
            .transcript
            .starts_with("User: make switching faster\nAssistant: ")
    );
    assert!(!conversation.transcript.contains("secret draft"));
    assert!(conversation.transcript.len() <= MAX_CONVERSATION_BYTES);
}

#[test]
fn starts_an_ephemeral_exact_luna_thread_and_validates_output() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let session = FakeSession {
        replies: VecDeque::from([
            json!({"thread": {"id": "naming-thread"}}),
            json!({"turn": {"id": "turn"}}),
            json!({"turn": {"items": [{"type": "agentMessage", "text": "{\"title\":\"Faster Session Switching\"}"}]}}),
        ]),
        calls: calls.clone(),
    };
    let mut namer = AppServerNamer::new(session);
    let conversation = NamingConversation {
        thread_id: "source".to_owned(),
        transcript: "User: speed up everything".to_owned(),
    };
    assert_eq!(
        namer.generate_name(&conversation).unwrap(),
        "Faster Session Switching"
    );
    assert_eq!(NAMING_MODEL, "gpt-5.6-luna");
    let calls = calls.lock().unwrap();
    assert_eq!(calls[0].0, "thread/start");
    assert_eq!(calls[0].1["model"], NAMING_MODEL);
    assert_eq!(calls[0].1["ephemeral"], true);
    assert_eq!(calls[0].1["sandbox"], "read-only");
    assert_eq!(calls[0].1["approvalPolicy"], "never");
    assert_eq!(calls[1].0, "turn/start");
    assert_eq!(calls[1].1["threadId"], "naming-thread");
    assert_eq!(calls[1].1["model"], NAMING_MODEL);
    assert_eq!(calls[1].1["input"][0]["text"], "User: speed up everything");
    assert_eq!(calls[1].1["outputSchema"]["additionalProperties"], false);
    assert_eq!(
        calls[2],
        (
            "turn/completed".to_owned(),
            json!({"threadId": "naming-thread"})
        )
    );
}
