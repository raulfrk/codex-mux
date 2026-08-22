use std::{
    collections::{VecDeque, hash_map::DefaultHasher},
    fs,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use codex_mux::{
    MuxError, Result,
    smart_naming::{
        AppServerNamer, AppServerProcess, AppServerSession, ConversationNamer,
        MAX_CONVERSATION_BYTES, NAMING_MODEL, NAMING_REASONING_EFFORT, NamingConversation,
        NamingTarget, NamingWorker, RolloutStore, start_if_enabled,
    },
};
use serde_json::{Value, json};

use codex_mux::domain::{Pane, PaneId, SessionId};

struct FakeSession {
    replies: VecDeque<Value>,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}

#[test]
fn immediate_app_server_exit_is_a_readiness_error_and_is_reaped() {
    let error = match AppServerProcess::spawn(std::path::Path::new("/bin/false")) {
        Ok(_) => panic!("exiting process unexpectedly became ready"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("app-server"));
}

#[test]
fn disabled_mode_does_not_construct_or_call_a_provider() {
    let starts = Arc::new(Mutex::new(0));
    let observed = starts.clone();
    let provider = start_if_enabled(false, || {
        *observed.lock().unwrap() += 1;
        Ok("provider")
    })
    .unwrap();
    assert_eq!(provider, None);
    assert_eq!(*starts.lock().unwrap(), 0);
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

struct UnavailableSession;

impl AppServerSession for UnavailableSession {
    fn request(&mut self, _: &str, _: Value) -> Result<Value> {
        Err(MuxError::Command("app-server unavailable".to_owned()))
    }

    fn wait_for(&mut self, _: &str, _: &str) -> Result<Value> {
        Err(MuxError::Command("app-server unavailable".to_owned()))
    }

    fn is_healthy(&self) -> bool {
        false
    }
}

struct ArchiveFailsSession {
    thread_id: String,
    healthy: bool,
}

struct UnhealthyReplySession {
    reply: Option<Value>,
}

impl AppServerSession for UnhealthyReplySession {
    fn request(&mut self, _: &str, _: Value) -> Result<Value> {
        Ok(self.reply.take().unwrap())
    }

    fn wait_for(&mut self, _: &str, _: &str) -> Result<Value> {
        Err(MuxError::Command("app-server exited".to_owned()))
    }

    fn is_healthy(&self) -> bool {
        false
    }
}

impl AppServerSession for ArchiveFailsSession {
    fn request(&mut self, _: &str, _: Value) -> Result<Value> {
        if self.healthy {
            self.healthy = false;
            Ok(json!({"data": [{"id": self.thread_id}], "nextCursor": null}))
        } else {
            Err(MuxError::Command("app-server exited".to_owned()))
        }
    }

    fn wait_for(&mut self, _: &str, _: &str) -> Result<Value> {
        Err(MuxError::Command("app-server exited".to_owned()))
    }

    fn is_healthy(&self) -> bool {
        self.healthy
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "codex-mux-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn verified_rollout_resolves_without_cwd_and_recovers_when_app_server_is_unavailable() {
    let scratch = Scratch::new("rollout-fallback");
    let sessions = scratch.0.join("sessions/2026/08/21");
    fs::create_dir_all(&sessions).unwrap();
    let full = "01a01001-2dbb-74e2-86ab-996b31234567";
    let rollout = sessions.join(format!("rollout-2026-08-21T00-00-00-{full}.jsonl"));
    let mut rollout_bytes = b"\xff\xfe invalid utf8 record\n".to_vec();
    rollout_bytes.extend_from_slice(
        format!(
            concat!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{}\",\"cwd\":\"/original/project\"}}}}\n",
                "this is a malformed record\n",
                "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"id\":\"context-1\",\"content\":[{{\"type\":\"input_text\",\"text\":\"secret contextual fragment\"}}]}}}}\n",
                "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"item_completed\",\"item\":{{\"id\":\"user-1\",\"type\":\"UserMessage\",\"content\":[{{\"type\":\"text\",\"text\":\"resume this elsewhere\"}}]}}}}}}\n",
                "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"id\":\"agent-1\",\"content\":[{{\"type\":\"output_text\",\"text\":\"working from the resumed directory\"}}]}}}}\n",
                "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"item_completed\",\"item\":{{\"id\":\"agent-1\",\"type\":\"AgentMessage\",\"phase\":\"final_answer\",\"content\":[{{\"type\":\"Text\",\"text\":\"working from the resumed directory\"}}]}}}}}}\n",
                "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"agent_message\",\"message\":\"legacy visible answer\"}}}}\n",
                "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"legacy visible answer\"}}]}}}}\n"
            ),
            full
        )
        .as_bytes(),
    );
    fs::write(&rollout, rollout_bytes).unwrap();
    let mut namer = AppServerNamer::new(UnavailableSession)
        .with_rollouts(RolloutStore::at(scratch.0.join("sessions")));
    let target = NamingTarget {
        pane_id: PaneId::new("%7").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        pane_pid: 77,
        pane_title: "01a01001-2dbb-74e2-86ab-996b3...".to_owned(),
        thread_hint: "01a01001-2dbb-74e2-86ab-996b3".to_owned(),
        cwd: "/different/resumed/project".into(),
        generated_name: None,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_token: None,
    };

    let conversation = ConversationNamer::read(&mut namer, &target).unwrap();
    assert_eq!(conversation.thread_id, full);
    assert!(
        conversation
            .transcript
            .contains("User: resume this elsewhere")
    );
    assert!(
        conversation
            .transcript
            .contains("Assistant: working from the resumed directory")
    );
    assert!(
        !conversation
            .transcript
            .contains("secret contextual fragment")
    );
    assert_eq!(
        conversation
            .transcript
            .matches("working from the resumed directory")
            .count(),
        1
    );
    assert_eq!(
        conversation
            .transcript
            .matches("legacy visible answer")
            .count(),
        1
    );
}

#[test]
fn rollout_fallback_derives_only_sanitized_structural_cwd_evidence() {
    let scratch = Scratch::new("rollout-activity");
    let sessions = scratch.0.join("sessions/2026/08/22");
    let component = scratch.0.join("product/services/gateway");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(scratch.0.join("product/.git")).unwrap();
    fs::create_dir_all(&component).unwrap();
    fs::write(component.join("go.mod"), "module example.invalid/gateway\n").unwrap();
    let full = "01a01001-2dbb-74e2-86ab-996b31234567";
    let rollout = sessions.join(format!("rollout-2026-08-22T00-00-00-{full}.jsonl"));
    let records = [
        json!({"type": "session_meta", "payload": {"id": full, "cwd": component}}),
        json!({"type": "turn_context", "payload": {"cwd": component, "private": "must-not-leak"}}),
        json!({"type": "event_msg", "payload": {"type": "user_message", "message": "Continue gateway reliability"}}),
    ];
    fs::write(
        &rollout,
        records
            .iter()
            .map(|record| serde_json::to_string(record).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();

    let mut namer = AppServerNamer::new(UnavailableSession)
        .with_rollouts(RolloutStore::at(scratch.0.join("sessions")));
    let target = NamingTarget {
        pane_id: PaneId::new("%7").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        pane_pid: 77,
        pane_title: full.to_owned(),
        thread_hint: full.to_owned(),
        cwd: scratch.0.clone(),
        generated_name: None,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_token: None,
    };
    let conversation = ConversationNamer::read(&mut namer, &target).unwrap();
    assert!(conversation.activity.contains("product/services/gateway"));
    assert!(!conversation.activity.contains("must-not-leak"));
    assert!(!conversation.activity.contains(scratch.0.to_str().unwrap()));
}

#[test]
fn verified_rollout_survives_provider_exit_during_archive_cross_check() {
    let scratch = Scratch::new("rollout-partial-provider-failure");
    let sessions = scratch.0.join("sessions/2026/08/21");
    fs::create_dir_all(&sessions).unwrap();
    let full = "01a01001-2dbb-74e2-86ab-996b31234567";
    fs::write(
        sessions.join(format!("rollout-2026-08-21T00-00-00-{full}.jsonl")),
        format!(
            concat!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{}\"}}}}\n",
                "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"item_completed\",\"item\":{{\"id\":\"user-1\",\"type\":\"UserMessage\",\"content\":[{{\"type\":\"text\",\"text\":\"keep naming\"}}]}}}}}}\n"
            ),
            full
        ),
    )
    .unwrap();
    let session = ArchiveFailsSession {
        thread_id: full.to_owned(),
        healthy: true,
    };
    let mut namer =
        AppServerNamer::new(session).with_rollouts(RolloutStore::at(scratch.0.join("sessions")));
    let target = NamingTarget {
        pane_id: PaneId::new("%7").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        pane_pid: 77,
        pane_title: "01a01001-2dbb-74e2-86ab-996b3...".to_owned(),
        thread_hint: "01a01001-2dbb-74e2-86ab-996b3".to_owned(),
        cwd: "/work/project".into(),
        generated_name: None,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_token: None,
    };
    let conversation = ConversationNamer::read(&mut namer, &target).unwrap();
    assert_eq!(conversation.thread_id, full);
    assert!(conversation.transcript.contains("User: keep naming"));
}

#[test]
fn verified_rollout_prefix_ambiguity_fails_closed() {
    let scratch = Scratch::new("rollout-ambiguity");
    let sessions = scratch.0.join("sessions/2026/08/21");
    fs::create_dir_all(&sessions).unwrap();
    for full in [
        "01a01001-2dbb-74e2-86ab-996b31234567",
        "01a01001-2dbb-74e2-86ab-996b3abcdef0",
    ] {
        fs::write(
            sessions.join(format!("rollout-2026-08-21T00-00-00-{full}.jsonl")),
            format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{full}\"}}}}\n"),
        )
        .unwrap();
    }
    let mut namer = AppServerNamer::new(UnavailableSession)
        .with_rollouts(RolloutStore::at(scratch.0.join("sessions")));
    let target = NamingTarget {
        pane_id: PaneId::new("%7").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        pane_pid: 77,
        pane_title: "01a01001-2dbb-74e2-86ab-996b3...".to_owned(),
        thread_hint: "01a01001-2dbb-74e2-86ab-996b3".to_owned(),
        cwd: "/work/project".into(),
        generated_name: None,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_token: None,
    };

    assert!(
        ConversationNamer::read(&mut namer, &target)
            .unwrap_err()
            .to_string()
            .contains("multiple rollouts")
    );
}

#[test]
fn response_only_rollout_uses_visible_assistant_without_contextual_user_fragments() {
    let scratch = Scratch::new("response-only-rollout");
    let sessions = scratch.0.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let full = "01a01001-2dbb-74e2-86ab-996b31234567";
    fs::write(
        sessions.join(format!("rollout-2026-08-21T00-00-00-{full}.jsonl")),
        format!(
            concat!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{}\"}}}}\n",
                "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"id\":\"context\",\"content\":[{{\"type\":\"input_text\",\"text\":\"hidden context\"}}]}}}}\n",
                "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"id\":\"answer\",\"content\":[{{\"type\":\"output_text\",\"text\":\"visible completed answer\"}}]}}}}\n"
            ),
            full
        ),
    )
    .unwrap();
    let mut namer = AppServerNamer::new(UnavailableSession)
        .with_rollouts(RolloutStore::at(scratch.0.join("sessions")));
    let target = NamingTarget {
        pane_id: PaneId::new("%7").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        pane_pid: 77,
        pane_title: "01a01001-2dbb-74e2-86ab-996b3...".to_owned(),
        thread_hint: "01a01001-2dbb-74e2-86ab-996b3".to_owned(),
        cwd: "/work/project".into(),
        generated_name: None,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_token: None,
    };
    let conversation = ConversationNamer::read(&mut namer, &target).unwrap();
    assert!(
        conversation
            .transcript
            .contains("Assistant: visible completed answer")
    );
    assert!(!conversation.transcript.contains("hidden context"));
}

#[test]
fn rollout_and_app_server_prefix_collision_fails_closed() {
    let scratch = Scratch::new("rollout-app-collision");
    let sessions = scratch.0.join("sessions/2026/08/21");
    fs::create_dir_all(&sessions).unwrap();
    let rollout_id = "01a01001-2dbb-74e2-86ab-996b31234567";
    let server_id = "01a01001-2dbb-74e2-86ab-996b3abcdef0";
    fs::write(
        sessions.join(format!("rollout-2026-08-21T00-00-00-{rollout_id}.jsonl")),
        format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{rollout_id}\"}}}}\n"),
    )
    .unwrap();
    let session = FakeSession {
        replies: VecDeque::from([json!({
            "data": [{"id": server_id}],
            "nextCursor": null
        })]),
        calls: Arc::default(),
    };
    let mut namer =
        AppServerNamer::new(session).with_rollouts(RolloutStore::at(scratch.0.join("sessions")));
    let target = NamingTarget {
        pane_id: PaneId::new("%7").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        pane_pid: 77,
        pane_title: "01a01001-2dbb-74e2-86ab-996b3...".to_owned(),
        thread_hint: "01a01001-2dbb-74e2-86ab-996b3".to_owned(),
        cwd: "/work/project".into(),
        generated_name: None,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_token: None,
    };
    assert!(
        ConversationNamer::read(&mut namer, &target)
            .unwrap_err()
            .to_string()
            .contains("multiple threads")
    );
}

#[test]
fn unhealthy_provider_cannot_hide_a_cross_source_collision() {
    let scratch = Scratch::new("rollout-unhealthy-collision");
    let sessions = scratch.0.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let rollout_id = "01a01001-2dbb-74e2-86ab-996b31234567";
    let server_id = "01a01001-2dbb-74e2-86ab-996b3abcdef0";
    fs::write(
        sessions.join(format!("rollout-2026-08-21T00-00-00-{rollout_id}.jsonl")),
        format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{rollout_id}\"}}}}\n"),
    )
    .unwrap();
    let session = UnhealthyReplySession {
        reply: Some(json!({"data": [{"id": server_id}], "nextCursor": null})),
    };
    let mut namer = AppServerNamer::new(session).with_rollouts(RolloutStore::at(sessions));
    let target = NamingTarget {
        pane_id: PaneId::new("%7").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        pane_pid: 77,
        pane_title: "01a01001-2dbb-74e2-86ab-996b3...".to_owned(),
        thread_hint: "01a01001-2dbb-74e2-86ab-996b3".to_owned(),
        cwd: "/work/project".into(),
        generated_name: None,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_token: None,
    };
    assert!(
        ConversationNamer::read(&mut namer, &target)
            .unwrap_err()
            .to_string()
            .contains("multiple threads")
    );
}

#[test]
fn multi_cursor_turn_cycle_fails_closed() {
    let session = FakeSession {
        replies: VecDeque::from([
            json!({"data": [], "nextCursor": "a"}),
            json!({"data": [], "nextCursor": "b"}),
            json!({"data": [], "nextCursor": "a"}),
        ]),
        calls: Arc::default(),
    };
    let error = AppServerNamer::new(session)
        .read_completed("source-thread")
        .unwrap_err();
    assert!(error.to_string().contains("repeated its pagination cursor"));
}

#[test]
fn multi_cursor_item_cycle_cannot_succeed_by_filling_the_transcript() {
    let huge = "x".repeat(MAX_CONVERSATION_BYTES * 2);
    let session = FakeSession {
        replies: VecDeque::from([
            json!({"data": [{"id": "turn-1", "status": "completed"}], "nextCursor": null}),
            json!({"data": [], "nextCursor": "a"}),
            json!({"data": [], "nextCursor": "b"}),
            json!({"data": [{"turnId": "turn-1", "item": {"type": "agentMessage", "text": huge}}], "nextCursor": "a"}),
        ]),
        calls: Arc::default(),
    };
    let error = AppServerNamer::new(session)
        .read_completed("source-thread")
        .unwrap_err();
    assert!(error.to_string().contains("repeated its pagination cursor"));
}

#[test]
fn conversation_history_uses_one_shared_request_budget() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut replies = VecDeque::from([json!({
        "data": [{"id": "turn-1", "status": "completed"}],
        "nextCursor": null
    })]);
    for index in 0..MAX_CONVERSATION_BYTES {
        replies.push_back(json!({"data": [], "nextCursor": format!("page-{index}")}));
    }
    let session = FakeSession {
        replies,
        calls: calls.clone(),
    };
    let error = AppServerNamer::new(session)
        .read_completed("source-thread")
        .unwrap_err();
    assert!(error.to_string().contains("request budget"));
    assert_eq!(calls.lock().unwrap().len(), 64);
}

#[test]
fn conversation_history_rejects_oversized_pages_ids_and_cursors() {
    let oversized_page = (0..101)
        .map(|index| json!({"id": format!("turn-{index}"), "status": "completed"}))
        .collect::<Vec<_>>();
    for reply in [
        json!({"data": oversized_page, "nextCursor": null}),
        json!({"data": [{"id": "x".repeat(129), "status": "completed"}], "nextCursor": null}),
        json!({"data": [], "nextCursor": "x".repeat(4097)}),
    ] {
        let session = FakeSession {
            replies: VecDeque::from([reply]),
            calls: Arc::default(),
        };
        assert!(
            AppServerNamer::new(session)
                .read_completed("source-thread")
                .is_err()
        );
    }
}

#[test]
fn multi_cursor_thread_list_cycle_fails_closed() {
    let session = FakeSession {
        replies: VecDeque::from([
            json!({"data": [], "nextCursor": "a"}),
            json!({"data": [], "nextCursor": "b"}),
            json!({"data": [], "nextCursor": "a"}),
        ]),
        calls: Arc::default(),
    };
    let target = NamingTarget {
        pane_id: PaneId::new("%7").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        pane_pid: 77,
        pane_title: "01a01001-2dbb-74e2-86ab-996b3...".to_owned(),
        thread_hint: "01a01001-2dbb-74e2-86ab-996b3".to_owned(),
        cwd: "/work/project".into(),
        generated_name: None,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_token: None,
    };
    let error = ConversationNamer::read(&mut AppServerNamer::new(session), &target).unwrap_err();
    assert!(error.to_string().contains("repeated its pagination cursor"));
}

#[cfg(unix)]
#[test]
fn rollout_store_rejects_symlinked_and_writable_trees() {
    use std::os::unix::{fs::PermissionsExt, fs::symlink};

    let scratch = Scratch::new("rollout-tree-safety");
    let private = scratch.0.join("private");
    fs::create_dir_all(&private).unwrap();
    let linked = scratch.0.join("linked");
    symlink(&private, &linked).unwrap();
    let target = NamingTarget {
        pane_id: PaneId::new("%7").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        pane_pid: 77,
        pane_title: "01a01001-2dbb-74e2-86ab-996b3...".to_owned(),
        thread_hint: "01a01001-2dbb-74e2-86ab-996b3".to_owned(),
        cwd: "/work/project".into(),
        generated_name: None,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_token: None,
    };
    let mut linked_namer =
        AppServerNamer::new(UnavailableSession).with_rollouts(RolloutStore::at(linked));
    assert!(ConversationNamer::read(&mut linked_namer, &target).is_err());

    fs::set_permissions(&private, fs::Permissions::from_mode(0o777)).unwrap();
    let mut writable_namer =
        AppServerNamer::new(UnavailableSession).with_rollouts(RolloutStore::at(private));
    assert!(ConversationNamer::read(&mut writable_namer, &target).is_err());
}

#[cfg(unix)]
#[test]
fn wide_rollout_tree_still_detects_prefix_ambiguity() {
    let scratch = Scratch::new("wide-rollout-tree");
    let sessions = scratch.0.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    for index in 0..256 {
        fs::create_dir(sessions.join(format!("branch-{index}"))).unwrap();
    }
    for (branch, full) in [
        ("branch-0", "01a01001-2dbb-74e2-86ab-996b31234567"),
        ("branch-255", "01a01001-2dbb-74e2-86ab-996b3abcdef0"),
    ] {
        fs::write(
            sessions
                .join(branch)
                .join(format!("rollout-2026-08-21T00-00-00-{full}.jsonl")),
            format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{full}\"}}}}\n"),
        )
        .unwrap();
    }
    let mut namer =
        AppServerNamer::new(UnavailableSession).with_rollouts(RolloutStore::at(sessions));
    let target = NamingTarget {
        pane_id: PaneId::new("%7").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        pane_pid: 77,
        pane_title: "01a01001-2dbb-74e2-86ab-996b3...".to_owned(),
        thread_hint: "01a01001-2dbb-74e2-86ab-996b3".to_owned(),
        cwd: "/work/project".into(),
        generated_name: None,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_token: None,
    };
    assert!(
        ConversationNamer::read(&mut namer, &target)
            .unwrap_err()
            .to_string()
            .contains("multiple rollouts")
    );
}

#[test]
fn reads_only_completed_user_and_assistant_text_with_a_hard_bound() {
    let huge = "x".repeat(MAX_CONVERSATION_BYTES * 2);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let session = FakeSession {
        replies: VecDeque::from([
            json!({"data": [
                {"id": "failed-turn", "status": "failed"},
                {"id": "completed-turn", "status": "completed"}
            ], "nextCursor": null}),
            json!({"data": [
                {"turnId": "completed-turn", "item": {"type": "agentMessage", "text": huge}},
                {"turnId": "completed-turn", "item": {"type": "commandExecution", "command": "ignored"}},
                {"turnId": "completed-turn", "item": {"type": "userMessage", "content": [{"type": "text", "text": "make switching faster"}]}}
            ], "nextCursor": null}),
        ]),
        calls: calls.clone(),
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
    let calls = calls.lock().unwrap();
    assert_eq!(calls[0].1["sortDirection"], "desc");
    assert_eq!(calls[1].1["sortDirection"], "desc");
}

#[test]
fn structured_activity_covers_common_repository_and_monorepo_styles_without_extra_requests() {
    let scratch = Scratch::new("activity-repository-styles");
    let cases = [
        ("standalone-rust", "crates/terminal-ui", "Cargo.toml"),
        ("npm-workspace", "packages/session-picker", "package.json"),
        ("python-monorepo", "services/naming", "pyproject.toml"),
        ("go-workspace", "cmd/mux-daemon", "go.mod"),
        ("java-multiproject", "modules/process-match", "pom.xml"),
    ];

    for (repository_name, component, manifest) in cases {
        let repository = scratch.0.join(repository_name);
        let component_root = repository.join(component);
        fs::create_dir_all(repository.join(".git")).unwrap();
        fs::create_dir_all(component_root.join("src")).unwrap();
        fs::write(component_root.join(manifest), "eval fixture").unwrap();
        let source = component_root.join("src/lib.rs");
        fs::write(&source, "// fixture").unwrap();

        let calls = Arc::new(Mutex::new(Vec::new()));
        let session = FakeSession {
            replies: VecDeque::from([
                json!({"data": [{"id": "turn-1", "status": "completed"}], "nextCursor": null}),
                json!({"data": [
                    {"turnId": "turn-1", "item": {
                        "type": "commandExecution",
                        "cwd": component_root,
                        "command": "do-not-retain --token secret-value",
                        "aggregatedOutput": "secret-output",
                        "commandActions": [{"type": "read", "path": "src/lib.rs", "command": "ignored", "name": "lib.rs"}]
                    }},
                    {"turnId": "turn-1", "item": {
                        "type": "fileChange",
                        "changes": [{"path": source, "kind": "update", "diff": "credential=secret"}]
                    }},
                    {"turnId": "turn-1", "item": {"type": "userMessage", "content": [{"type": "text", "text": "Improve durable session naming"}]}}
                ], "nextCursor": null}),
            ]),
            calls: calls.clone(),
        };

        let conversation = AppServerNamer::new(session)
            .read_completed("source-thread")
            .unwrap();
        let expected = format!("{repository_name}/{component}");
        assert!(
            conversation.activity.contains(&expected),
            "{conversation:?}"
        );
        assert!(!conversation.activity.contains("secret"));
        assert_eq!(
            calls.lock().unwrap().len(),
            2,
            "activity added an API round"
        );
    }
}

#[test]
fn relative_structured_paths_never_resolve_against_the_mux_daemon_cwd() {
    let session = FakeSession {
        replies: VecDeque::from([
            json!({"data": [{"id": "turn-1", "status": "completed"}], "nextCursor": null}),
            json!({"data": [
                {"turnId": "turn-1", "item": {
                    "type": "commandExecution",
                    "cwd": ".",
                    "command": "ignored",
                    "commandActions": [{"type": "read", "path": "src/smart_naming.rs", "command": "ignored", "name": "smart_naming.rs"}]
                }},
                {"turnId": "turn-1", "item": {
                    "type": "fileChange",
                    "changes": [{"path": "src/lib.rs", "kind": "update", "diff": "ignored"}]
                }},
                {"turnId": "turn-1", "item": {"type": "userMessage", "content": [{"type": "text", "text": "Keep naming private"}]}}
            ], "nextCursor": null}),
        ]),
        calls: Arc::default(),
    };

    let conversation = AppServerNamer::new(session)
        .read_completed("source-thread")
        .unwrap();
    assert!(conversation.activity.is_empty());
}

#[cfg(unix)]
#[test]
fn structured_parent_traversal_uses_filesystem_symlink_semantics() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new("activity-symlink-parent");
    let containing = scratch.0.join("containing-product");
    let target = scratch.0.join("actual-product/services");
    fs::create_dir_all(containing.join(".git")).unwrap();
    fs::create_dir_all(target.join("api")).unwrap();
    fs::create_dir_all(target.join("shared")).unwrap();
    fs::create_dir_all(scratch.0.join("actual-product/.git")).unwrap();
    fs::write(target.join("shared/package.json"), "{}").unwrap();
    symlink(target.join("api"), containing.join("linked-api")).unwrap();

    let session = FakeSession {
        replies: VecDeque::from([
            json!({"data": [{"id": "turn-1", "status": "completed"}], "nextCursor": null}),
            json!({"data": [
                {"turnId": "turn-1", "item": {
                    "type": "commandExecution",
                    "cwd": containing,
                    "command": "ignored",
                    "commandActions": [{"type": "listFiles", "path": "linked-api/../shared", "command": "ignored"}]
                }},
                {"turnId": "turn-1", "item": {"type": "userMessage", "content": [{"type": "text", "text": "Work on shared API support"}]}}
            ], "nextCursor": null}),
        ]),
        calls: Arc::default(),
    };

    let conversation = AppServerNamer::new(session)
        .read_completed("source-thread")
        .unwrap();
    assert!(
        conversation
            .activity
            .contains("actual-product/services/shared")
    );
    assert!(!conversation.activity.contains("containing-product/shared"));
}

#[test]
fn resolves_one_truncated_thread_across_pages_after_cwd_changes() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let full = "01a01001-2dbb-74e2-86ab-996b31234567";
    let session = FakeSession {
        replies: VecDeque::from([
            json!({"data": [
                {"id": full, "cwd": "/other/project"}
            ], "nextCursor": "page-2"}),
            json!({"data": [
                {"id": full, "cwd": "/work/project"}
            ], "nextCursor": null}),
            json!({"data": [], "nextCursor": null}),
            json!({"data": [{"id": "turn-1", "status": "completed"}], "nextCursor": null}),
            json!({"data": [
                {"turnId": "turn-1", "item": {"type": "userMessage", "content": [{"type": "text", "text": "name this"}]}}
            ], "nextCursor": null}),
        ]),
        calls: calls.clone(),
    };
    let mut namer = AppServerNamer::new(session);
    let target = NamingTarget {
        pane_id: PaneId::new("%7").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        pane_pid: 77,
        pane_title: "01a01001-2dbb-74e2-86ab-996b3...".to_owned(),
        thread_hint: "01a01001-2dbb-74e2-86ab-996b3".to_owned(),
        cwd: "/work/project".into(),
        generated_name: None,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_token: None,
    };

    let conversation = ConversationNamer::read(&mut namer, &target).unwrap();
    assert_eq!(conversation.thread_id, full);
    assert!(conversation.transcript.contains("User: name this"));
    let calls = calls.lock().unwrap();
    assert_eq!(calls[0].0, "thread/list");
    assert!(calls[0].1.get("cwd").is_none());
    assert_eq!(calls[0].1["useStateDbOnly"], true);
    assert_eq!(calls[1].1["cursor"], "page-2");
    assert_eq!(calls[2].1["useStateDbOnly"], false);
    assert_eq!(calls[3].0, "thread/turns/list");
    assert_eq!(calls[3].1["threadId"], full);
    assert_eq!(calls[3].1["itemsView"], "notLoaded");
    assert_eq!(calls[4].0, "thread/items/list");
    assert_eq!(calls[4].1["threadId"], full);
    assert!(calls[4].1.get("turnId").is_none());
}

#[test]
fn resolves_external_truncated_thread_after_state_db_miss() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let full = "01a01001-2dbb-74e2-86ab-996b31234567";
    let session = FakeSession {
        replies: VecDeque::from([
            json!({"data": [], "nextCursor": null}),
            json!({"data": [{"id": full, "cwd": "/work/project"}], "nextCursor": null}),
            json!({"data": [{"id": "turn-1", "status": "completed"}], "nextCursor": null}),
            json!({"data": [
                {"turnId": "turn-1", "item": {"type": "userMessage", "content": [{"type": "text", "text": "name this"}]}}
            ], "nextCursor": null}),
        ]),
        calls: calls.clone(),
    };
    let mut namer = AppServerNamer::new(session);
    let target = NamingTarget {
        pane_id: PaneId::new("%7").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        pane_pid: 77,
        pane_title: "01a01001-2dbb-74e2-86ab-996b3...".to_owned(),
        thread_hint: "01a01001-2dbb-74e2-86ab-996b3".to_owned(),
        cwd: "/work/project".into(),
        generated_name: None,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_token: None,
    };

    let conversation = ConversationNamer::read(&mut namer, &target).unwrap();
    assert_eq!(conversation.thread_id, full);
    let calls = calls.lock().unwrap();
    assert_eq!(calls[0].1["useStateDbOnly"], true);
    assert_eq!(calls[1].1["useStateDbOnly"], false);
}

#[test]
fn external_fallback_rejects_an_ambiguous_uuid_prefix() {
    let session = FakeSession {
        replies: VecDeque::from([
            json!({"data": [], "nextCursor": null}),
            json!({"data": [
                {"id": "01a01001-2dbb-74e2-86ab-996b31234567", "cwd": "/work/project"},
                {"id": "01a01001-2dbb-74e2-86ab-996b3abcdef0", "cwd": "/work/project"}
            ], "nextCursor": null}),
        ]),
        calls: Arc::default(),
    };
    let mut namer = AppServerNamer::new(session);
    let target = NamingTarget {
        pane_id: PaneId::new("%7").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        pane_pid: 77,
        pane_title: "01a01001-2dbb-74e2-86ab-996b3...".to_owned(),
        thread_hint: "01a01001-2dbb-74e2-86ab-996b3".to_owned(),
        cwd: "/work/project".into(),
        generated_name: None,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_token: None,
    };

    assert!(
        ConversationNamer::read(&mut namer, &target)
            .unwrap_err()
            .to_string()
            .contains("multiple threads")
    );
}

#[test]
fn external_fallback_rejects_a_state_and_archive_prefix_collision() {
    let session = FakeSession {
        replies: VecDeque::from([
            json!({"data": [
                {"id": "01a01001-2dbb-74e2-86ab-996b31234567", "cwd": "/work/project"}
            ], "nextCursor": null}),
            json!({"data": [
                {"id": "01a01001-2dbb-74e2-86ab-996b31234567", "cwd": "/work/project"},
                {"id": "01a01001-2dbb-74e2-86ab-996b3abcdef0", "cwd": "/work/project"}
            ], "nextCursor": null}),
        ]),
        calls: Arc::default(),
    };
    let mut namer = AppServerNamer::new(session);
    let target = NamingTarget {
        pane_id: PaneId::new("%7").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        pane_pid: 77,
        pane_title: "01a01001-2dbb-74e2-86ab-996b3...".to_owned(),
        thread_hint: "01a01001-2dbb-74e2-86ab-996b3".to_owned(),
        cwd: "/work/project".into(),
        generated_name: None,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_token: None,
    };

    assert!(
        ConversationNamer::read(&mut namer, &target)
            .unwrap_err()
            .to_string()
            .contains("multiple threads")
    );
}

#[test]
fn ambiguous_truncated_thread_fails_closed() {
    let session = FakeSession {
        replies: VecDeque::from([json!({"data": [
            {"id": "01a01001-2dbb-74e2-86ab-996b31234567", "cwd": "/work/project"},
            {"id": "01a01001-2dbb-74e2-86ab-996b3abcdef0", "cwd": "/work/project"}
        ], "nextCursor": null})]),
        calls: Arc::default(),
    };
    let mut namer = AppServerNamer::new(session);
    let target = NamingTarget {
        pane_id: PaneId::new("%7").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        pane_pid: 77,
        pane_title: "01a01001-2dbb-74e2-86ab-996b3...".to_owned(),
        thread_hint: "01a01001-2dbb-74e2-86ab-996b3".to_owned(),
        cwd: "/work/project".into(),
        generated_name: None,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_token: None,
    };

    assert!(
        ConversationNamer::read(&mut namer, &target)
            .unwrap_err()
            .to_string()
            .contains("multiple threads")
    );
}

#[test]
fn pane_target_retains_truncated_title_and_exact_cwd() {
    let pane = Pane {
        id: PaneId::new("%9").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        title: Some("01a01001-2dbb-74e2-86ab-996b3...".to_owned()),
        generated_title: Some("Existing entry title".to_owned()),
        generated_thread_id: Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_owned()),
        generated_source_stable: true,
        generated_at_unix: Some(1_700_000_000),
        immediate_naming: false,
        auto_name_status: None,
        auto_name_started_at_unix_nanos: None,
        auto_name_token: None,
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

        pane_pid: 100,
        current_path: "/work/project".into(),
    };

    let target = NamingTarget::from_pane(&pane).unwrap();
    assert_eq!(target.pane_title, pane.title.unwrap());
    assert_eq!(target.thread_hint, "01a01001-2dbb-74e2-86ab-996b3");
    assert_eq!(target.cwd, std::path::Path::new("/work/project"));
    assert_eq!(
        target.generated_name.as_deref(),
        Some("Existing entry title")
    );
    assert_eq!(target.generated_at_unix, Some(1_700_000_000));
}

#[test]
fn pane_target_rejects_prefixes_too_short_for_uuid_timestamp() {
    let pane = Pane {
        id: PaneId::new("%9").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        title: Some("12345678...".to_owned()),
        generated_title: None,
        generated_thread_id: None,
        generated_source_stable: false,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_status: None,
        auto_name_started_at_unix_nanos: None,
        auto_name_token: None,
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

        pane_pid: 100,
        current_path: "/work/project".into(),
    };

    assert_eq!(NamingTarget::from_pane(&pane), None);

    let hyphenated = Pane {
        title: Some("12345678-123...".to_owned()),
        ..pane
    };
    assert_eq!(NamingTarget::from_pane(&hyphenated), None);
}

#[test]
fn pane_target_rejects_a_manual_uuid_shaped_title() {
    let pane = Pane {
        id: PaneId::new("%9").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        title: Some("12345678-1234-1234-1234-123456789abc".to_owned()),
        generated_title: None,
        generated_thread_id: None,
        generated_source_stable: false,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_status: None,
        auto_name_started_at_unix_nanos: None,
        auto_name_token: None,
        manual_name: true,

        manual_name_source: None,

        manual_name_pid: None,

        manual_name_pid_raw: String::new(),

        manual_name_session: None,

        manual_name_session_raw: String::new(),

        unpin_waiting: false,
        unpin_waiting_title: None,
        unpin_waiting_pid: None,
        unpin_waiting_session: None,

        pane_pid: 100,
        current_path: "/work/project".into(),
    };

    assert_eq!(NamingTarget::from_pane(&pane), None);
}

#[test]
fn source_less_unpin_waits_for_a_changed_exact_thread_title() {
    let mut pane = Pane {
        id: PaneId::new("%9").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        title: Some("Manual project title".to_owned()),
        generated_title: None,
        generated_thread_id: None,
        generated_source_stable: false,
        generated_at_unix: None,
        immediate_naming: true,
        auto_name_status: None,
        auto_name_started_at_unix_nanos: None,
        auto_name_token: None,
        manual_name: false,
        manual_name_source: None,
        manual_name_pid: None,
        manual_name_pid_raw: String::new(),
        manual_name_session: None,
        manual_name_session_raw: String::new(),
        unpin_waiting: true,
        unpin_waiting_title: Some("Manual project title".to_owned()),
        unpin_waiting_pid: Some(100),
        unpin_waiting_session: Some(SessionId::new("$1").unwrap()),
        pane_pid: 100,
        current_path: "/work/project".into(),
    };
    assert_eq!(NamingTarget::from_pane(&pane), None);
    pane.title = Some("01a01001-2dbb-74e2-86ab-996b3...".to_owned());
    assert!(NamingTarget::from_pane(&pane).is_some());
    pane.unpin_waiting_pid = Some(101);
    assert_eq!(NamingTarget::from_pane(&pane), None);
}

#[test]
fn source_less_unpin_does_not_trim_its_unchanged_manual_title_into_an_identity() {
    let mut pane = Pane {
        id: PaneId::new("%9").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        title: Some(" 01a01001-2dbb-74e2-86ab-996b31234567 ".to_owned()),
        generated_title: None,
        generated_thread_id: None,
        generated_source_stable: false,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_status: None,
        auto_name_started_at_unix_nanos: None,
        auto_name_token: None,
        manual_name: false,
        manual_name_source: None,
        manual_name_pid: None,
        manual_name_pid_raw: String::new(),
        manual_name_session: None,
        manual_name_session_raw: String::new(),
        unpin_waiting: true,
        unpin_waiting_title: Some(" 01a01001-2dbb-74e2-86ab-996b31234567 ".to_owned()),
        unpin_waiting_pid: Some(100),
        unpin_waiting_session: Some(SessionId::new("$1").unwrap()),
        pane_pid: 100,
        current_path: "/work/project".into(),
    };

    assert!(NamingTarget::from_pane(&pane).is_none());
    pane.title = Some("01a01001-2dbb-74e2-86ab-996b31234567".to_owned());
    assert!(NamingTarget::from_pane(&pane).is_some());
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
        activity: String::new(),
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
    let instructions = calls[0].1["baseInstructions"].as_str().unwrap();
    assert!(instructions.contains("sustained project, component, or durable conversation theme"));
    assert!(instructions.contains("do not name a transient"));
    assert!(instructions.contains("privacy-sanitized, frequency-ranked"));
    assert_eq!(calls[1].0, "turn/start");
    assert_eq!(calls[1].1["threadId"], "naming-thread");
    assert_eq!(calls[1].1["model"], NAMING_MODEL);
    assert_eq!(calls[1].1["effort"], NAMING_REASONING_EFFORT);
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

struct CountingNamer {
    reads: Arc<AtomicUsize>,
    names: Arc<AtomicUsize>,
    delay: Duration,
}

struct ParallelNamer {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    gate: Arc<(Mutex<usize>, std::sync::Condvar)>,
}

impl ConversationNamer for ParallelNamer {
    fn read(&mut self, target: &NamingTarget) -> Result<NamingConversation> {
        Ok(NamingConversation {
            thread_id: target.thread_hint.clone(),
            transcript: "recent completed exchange".to_owned(),
            activity: String::new(),
        })
    }

    fn name(&mut self, _: &NamingConversation) -> Result<String> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        let (entered, ready) = &*self.gate;
        let mut entered = entered.lock().unwrap();
        *entered += 1;
        if *entered == 4 {
            ready.notify_all();
        }
        let (entered, _) = ready
            .wait_timeout_while(entered, Duration::from_secs(2), |entered| *entered < 4)
            .unwrap();
        if *entered < 4 {
            self.active.fetch_sub(1, Ordering::SeqCst);
            return Err(codex_mux::MuxError::Command(
                "parallel naming lanes did not overlap".to_owned(),
            ));
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok("Parallel generated name".to_owned())
    }
}

impl ConversationNamer for CountingNamer {
    fn read(&mut self, target: &NamingTarget) -> Result<NamingConversation> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(NamingConversation {
            thread_id: target.thread_hint.clone(),
            transcript: "completed chat".to_owned(),
            activity: String::new(),
        })
    }
    fn name(&mut self, _: &NamingConversation) -> Result<String> {
        self.names.fetch_add(1, Ordering::SeqCst);
        thread::sleep(self.delay);
        Ok("Useful generated name".to_owned())
    }
}

fn target(pane: &str, thread: &str) -> NamingTarget {
    NamingTarget {
        pane_id: PaneId::new(pane).unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        pane_pid: 77,
        pane_title: thread.to_owned(),
        thread_hint: thread.to_owned(),
        cwd: "/work/project".into(),
        generated_name: None,
        generated_at_unix: None,
        immediate_naming: false,
        auto_name_token: None,
    }
}

fn thread_created_at(created_at_unix: u64, suffix: &str) -> String {
    let milliseconds = created_at_unix * 1000;
    format!(
        "{:08x}-{:04x}-7000-8000-{suffix:0>12}",
        milliseconds >> 16,
        milliseconds & 0xffff
    )
}

fn wait_until(description: &str, predicate: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if predicate() {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!("timed out waiting for {description}");
}

#[test]
fn parallel_worker_overlaps_independent_conversations() {
    let mut by_lane = [None, None, None, None];
    for suffix in 0_u64..10_000 {
        let thread = format!("{suffix:08x}-1111-7777-8888-123456789abc");
        let mut hasher = DefaultHasher::new();
        thread
            .bytes()
            .filter(|byte| *byte != b'-')
            .take(12)
            .for_each(|byte| byte.hash(&mut hasher));
        let lane = hasher.finish() as usize % by_lane.len();
        by_lane[lane].get_or_insert(thread);
        if by_lane.iter().all(Option::is_some) {
            break;
        }
    }
    let targets = by_lane
        .into_iter()
        .enumerate()
        .map(|(index, thread)| target(&format!("%{}", index + 1), &thread.unwrap()))
        .collect::<Vec<_>>();
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(0), std::sync::Condvar::new()));
    let provider_active = active.clone();
    let provider_peak = peak.clone();
    let provider_gate = gate.clone();
    let discoveries = Arc::new(AtomicUsize::new(0));
    let observed_discoveries = discoveries.clone();
    let worker = NamingWorker::spawn_parallel_logged(
        4,
        move |_| {
            Ok(ParallelNamer {
                active: provider_active.clone(),
                peak: provider_peak.clone(),
                gate: provider_gate.clone(),
            })
        },
        move |_| {
            observed_discoveries.fetch_add(1, Ordering::SeqCst);
            Ok(targets.clone())
        },
        Duration::from_secs(1),
        None,
    );
    wait_until("four parallel names", || {
        worker.names().lock().unwrap().len() == 4
    });
    worker.stop();
    assert_eq!(peak.load(Ordering::SeqCst), 4);
    assert!(
        discoveries.load(Ordering::SeqCst) <= 5,
        "parallel lanes repeated the shared initial discovery or performed extra revalidations"
    );
}

#[test]
fn parallel_worker_forces_fresh_discovery_before_publication() {
    struct RemovingNamer {
        current: Arc<Mutex<Vec<NamingTarget>>>,
        named: Arc<AtomicUsize>,
    }
    impl ConversationNamer for RemovingNamer {
        fn read(&mut self, target: &NamingTarget) -> Result<NamingConversation> {
            Ok(NamingConversation {
                thread_id: target.thread_hint.clone(),
                transcript: "completed chat".to_owned(),
                activity: String::new(),
            })
        }
        fn name(&mut self, _: &NamingConversation) -> Result<String> {
            self.current.lock().unwrap().clear();
            self.named.fetch_add(1, Ordering::SeqCst);
            Ok("Stale generated name".to_owned())
        }
    }

    let current = Arc::new(Mutex::new(vec![target(
        "%1",
        "01999999-1111-7777-8888-123456789abc",
    )]));
    let named = Arc::new(AtomicUsize::new(0));
    let provider_current = current.clone();
    let provider_named = named.clone();
    let discovered = current.clone();
    let worker = NamingWorker::spawn_parallel_logged(
        4,
        move |_| {
            Ok(RemovingNamer {
                current: provider_current.clone(),
                named: provider_named.clone(),
            })
        },
        move |_| Ok(discovered.lock().unwrap().clone()),
        Duration::from_secs(1),
        None,
    );
    wait_until("a naming attempt", || named.load(Ordering::SeqCst) > 0);
    thread::sleep(Duration::from_millis(50));
    assert!(worker.names().lock().unwrap().is_empty());
    worker.stop();
}

#[test]
fn worker_is_non_blocking_deduplicates_and_joins_on_stop() {
    let reads = Arc::new(AtomicUsize::new(0));
    let names = Arc::new(AtomicUsize::new(0));
    let mut first = target("%1", "01999999-1111-7777-8888-123456789abc");
    first.auto_name_token = Some("request-one".to_owned());
    let mut second = target("%4", "01999999-1111-7777-8888-123456789abc");
    second.auto_name_token = Some("request-two".to_owned());
    let current = Arc::new(Mutex::new(vec![first, second]));
    let discovered = current.clone();
    let provider_names = names.clone();
    let worker = NamingWorker::spawn(
        move |_| {
            Ok(CountingNamer {
                reads: reads.clone(),
                names: provider_names,
                delay: Duration::from_millis(60),
            })
        },
        move || Ok(discovered.lock().unwrap().clone()),
        Duration::from_millis(10),
    );
    assert_eq!(
        names.load(Ordering::SeqCst),
        0,
        "spawn blocked on the provider"
    );
    for _ in 0..30 {
        if worker
            .names()
            .lock()
            .unwrap()
            .contains_key(&PaneId::new("%1").unwrap())
        {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        names.load(Ordering::SeqCst),
        1,
        "unchanged transcript was renamed twice"
    );
    assert_eq!(
        worker
            .names()
            .lock()
            .unwrap()
            .get(&PaneId::new("%1").unwrap())
            .map(|generated| generated.name.as_str()),
        Some("Useful generated name")
    );
    assert_eq!(
        worker
            .names()
            .lock()
            .unwrap()
            .get(&PaneId::new("%1").unwrap())
            .and_then(|generated| generated.auto_name_token.as_deref()),
        Some("request-one")
    );
    assert!(
        worker
            .names()
            .lock()
            .unwrap()
            .contains_key(&PaneId::new("%4").unwrap())
    );
    assert_eq!(
        worker
            .names()
            .lock()
            .unwrap()
            .get(&PaneId::new("%4").unwrap())
            .and_then(|generated| generated.auto_name_token.as_deref()),
        Some("request-two")
    );
    worker.stop();
}

#[test]
fn worker_reads_new_threads_immediately_and_existing_titles_at_refresh_deadlines() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let reads = Arc::new(AtomicUsize::new(0));
    let names = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let discoveries = Arc::new(AtomicUsize::new(0));
    let new = target("%1", &thread_created_at(now - 290, "1"));
    let mut existing = target("%2", &thread_created_at(now - 7_200, "2"));
    existing.generated_name = Some("Stable entry title".to_owned());
    existing.generated_at_unix = Some(now - 1_790);
    let observed_reads = reads.clone();
    let provider_started = started.clone();
    let observed_discoveries = discoveries.clone();
    let worker = NamingWorker::spawn(
        move |_| {
            provider_started.store(true, Ordering::Release);
            Ok(CountingNamer {
                reads: observed_reads,
                names,
                delay: Duration::ZERO,
            })
        },
        move || {
            observed_discoveries.fetch_add(1, Ordering::SeqCst);
            Ok(vec![new.clone(), existing.clone()])
        },
        Duration::from_millis(10),
    );

    wait_until("provider startup and repeated discovery", || {
        started.load(Ordering::Acquire) && discoveries.load(Ordering::SeqCst) >= 3
    });
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    worker.stop();

    let reads = Arc::new(AtomicUsize::new(0));
    let observed_reads = reads.clone();
    let mut refresh_due = target("%4", &thread_created_at(now - 7_200, "4"));
    refresh_due.generated_name = Some("Refreshable title".to_owned());
    refresh_due.generated_at_unix = Some(now - 1_810);
    let initial = target("%3", &thread_created_at(now - 310, "3"));
    let worker = NamingWorker::spawn(
        move |_| {
            Ok(CountingNamer {
                reads: observed_reads,
                names: Arc::new(AtomicUsize::new(0)),
                delay: Duration::ZERO,
            })
        },
        move || Ok(vec![initial.clone(), refresh_due.clone()]),
        Duration::from_millis(10),
    );
    wait_until("both due conversations to be read", || {
        reads.load(Ordering::SeqCst) >= 2
    });
    worker.stop();
    assert_eq!(reads.load(Ordering::SeqCst), 2);
}

#[test]
fn duplicate_panes_run_when_any_member_is_due() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let thread_id = thread_created_at(now - 7_200, "5");
    let mut fresh = target("%1", &thread_id);
    fresh.generated_name = Some("Fresh title".to_owned());
    fresh.generated_at_unix = Some(now);
    let untitled = target("%2", &thread_id);
    let reads = Arc::new(AtomicUsize::new(0));
    let observed_reads = reads.clone();
    let worker = NamingWorker::spawn(
        move |_| {
            Ok(CountingNamer {
                reads: observed_reads,
                names: Arc::new(AtomicUsize::new(0)),
                delay: Duration::ZERO,
            })
        },
        move || Ok(vec![fresh.clone(), untitled.clone()]),
        Duration::from_millis(10),
    );

    wait_until("the due duplicate pane to trigger naming", || {
        worker.names().lock().unwrap().len() == 2
    });
    worker.stop();
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

#[test]
fn one_naming_cycle_fans_out_to_same_thread_in_different_working_directories() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let thread_id = thread_created_at(now - 90, "f00d");
    let first = target("%1", &thread_id);
    let mut second = target("%2", &thread_id);
    second.cwd = "/other/project".into();
    let reads = Arc::new(AtomicUsize::new(0));
    let names = Arc::new(AtomicUsize::new(0));
    let observed_reads = reads.clone();
    let observed_names = names.clone();
    let worker = NamingWorker::spawn(
        move |_| {
            Ok(CountingNamer {
                reads: observed_reads,
                names: observed_names,
                delay: Duration::ZERO,
            })
        },
        move || Ok(vec![first.clone(), second.clone()]),
        Duration::from_millis(10),
    );

    wait_until("both panes for one thread to be named", || {
        worker.names().lock().unwrap().len() == 2
    });
    worker.stop();
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert_eq!(names.load(Ordering::SeqCst), 1);
}

#[test]
fn immediate_resume_marker_bypasses_a_fresh_generated_title_cooldown() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let reads = Arc::new(AtomicUsize::new(0));
    let observed_reads = reads.clone();
    let mut resumed = target("%1", &thread_created_at(now - 90, "a11ce"));
    resumed.generated_name = Some("Earlier generated title".to_owned());
    resumed.generated_at_unix = Some(now);
    resumed.immediate_naming = true;
    let worker = NamingWorker::spawn(
        move |_| {
            Ok(CountingNamer {
                reads: observed_reads,
                names: Arc::new(AtomicUsize::new(0)),
                delay: Duration::ZERO,
            })
        },
        move || Ok(vec![resumed.clone()]),
        Duration::from_millis(10),
    );

    wait_until("the resumed pane to bypass the refresh cooldown", || {
        reads.load(Ordering::SeqCst) == 1
    });
    worker.stop();
}

#[test]
fn explicit_force_token_bypasses_attempt_and_name_caches() {
    let thread_id = "01999999-1111-7777-8888-123456789abc";
    let reads = Arc::new(AtomicUsize::new(0));
    let names = Arc::new(AtomicUsize::new(0));
    let current = Arc::new(Mutex::new(vec![target("%1", thread_id)]));
    let discovered = current.clone();
    let observed_names = names.clone();
    let observed_reads = reads.clone();
    let worker = NamingWorker::spawn(
        move |_| {
            Ok(CountingNamer {
                reads: observed_reads,
                names: observed_names,
                delay: Duration::ZERO,
            })
        },
        move || Ok(discovered.lock().unwrap().clone()),
        Duration::from_millis(10),
    );
    wait_until("the initial cached naming attempt", || {
        names.load(Ordering::SeqCst) == 1
    });

    let mut forced = target("%1", thread_id);
    forced.generated_name = Some("Useful generated name".to_owned());
    forced.generated_at_unix = Some(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    );
    forced.immediate_naming = true;
    forced.auto_name_token = Some("new-request".to_owned());
    *current.lock().unwrap() = vec![forced];
    worker.trigger();

    wait_until("the explicit request to publish its causal token", || {
        worker
            .names()
            .lock()
            .unwrap()
            .get(&PaneId::new("%1").unwrap())
            .and_then(|generated| generated.auto_name_token.as_deref())
            == Some("new-request")
    });
    assert_eq!(names.load(Ordering::SeqCst), 2);
    assert_eq!(
        worker
            .names()
            .lock()
            .unwrap()
            .get(&PaneId::new("%1").unwrap())
            .and_then(|generated| generated.auto_name_token.as_deref()),
        Some("new-request")
    );

    let mut scheduled = target("%1", thread_id);
    scheduled.generated_name = Some("Useful generated name".to_owned());
    scheduled.generated_at_unix = Some(0);
    *current.lock().unwrap() = vec![scheduled];
    worker.trigger();
    wait_until("the subsequent ordinary refresh", || {
        reads.load(Ordering::SeqCst) == 3
    });
    assert_eq!(
        names.load(Ordering::SeqCst),
        2,
        "a completed force token leaked into an ordinary refresh"
    );
    worker.stop();
}

#[test]
fn wake_arriving_during_naming_restarts_discovery_without_waiting_for_polling() {
    use std::sync::atomic::AtomicBool;

    struct GatedNamer {
        entered: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
    }

    impl ConversationNamer for GatedNamer {
        fn read(&mut self, target: &NamingTarget) -> Result<NamingConversation> {
            Ok(NamingConversation {
                thread_id: target.thread_hint.clone(),
                transcript: "completed chat".to_owned(),
                activity: String::new(),
            })
        }

        fn name(&mut self, _: &NamingConversation) -> Result<String> {
            self.entered.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                thread::yield_now();
            }
            Ok("Useful generated name".to_owned())
        }
    }

    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let current = Arc::new(Mutex::new(vec![target(
        "%1",
        "01999999-1111-7777-8888-123456789abc",
    )]));
    let discovered = current.clone();
    let worker = NamingWorker::spawn(
        {
            let entered = entered.clone();
            let release = release.clone();
            move |_| Ok(GatedNamer { entered, release })
        },
        move || Ok(discovered.lock().unwrap().clone()),
        Duration::from_secs(60),
    );

    wait_until("the initial naming call", || {
        entered.load(Ordering::Acquire)
    });
    *current.lock().unwrap() = vec![target("%2", "01999999-2222-7777-8888-123456789abc")];
    worker.trigger();
    release.store(true, Ordering::Release);

    wait_until("the wake to rediscover the resumed pane", || {
        worker
            .names()
            .lock()
            .unwrap()
            .contains_key(&PaneId::new("%2").unwrap())
    });
    worker.stop();
}

#[test]
fn fanout_resolution_does_not_hold_the_generated_names_lock() {
    use std::sync::atomic::AtomicBool;

    let thread_id = "01999999-1111-7777-8888-123456789abc";
    struct BlockingResolver {
        entered: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
    }
    impl ConversationNamer for BlockingResolver {
        fn resolve(&mut self, target: &NamingTarget) -> Result<String> {
            if target.pane_id.as_str() == "%2" {
                self.entered.store(true, Ordering::Release);
                while !self.release.load(Ordering::Acquire) {
                    thread::yield_now();
                }
            }
            Ok("01999999-1111-7777-8888-123456789abc".to_owned())
        }

        fn read(&mut self, _: &NamingTarget) -> Result<NamingConversation> {
            Ok(NamingConversation {
                thread_id: "01999999-1111-7777-8888-123456789abc".to_owned(),
                transcript: "completed chat".to_owned(),
                activity: String::new(),
            })
        }

        fn name(&mut self, _: &NamingConversation) -> Result<String> {
            Ok("Useful generated name".to_owned())
        }
    }

    let source = target("%1", thread_id);
    let mut resolved_elsewhere = target("%2", "01999999-1111-7777-8888-12345...");
    resolved_elsewhere.pane_title = resolved_elsewhere.thread_hint.clone();
    let discoveries = Arc::new(AtomicUsize::new(0));
    let observed_discoveries = discoveries.clone();
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let worker = NamingWorker::spawn(
        {
            let entered = entered.clone();
            let release = release.clone();
            move |_| Ok(BlockingResolver { entered, release })
        },
        move || {
            let call = observed_discoveries.fetch_add(1, Ordering::SeqCst);
            Ok(if call == 0 {
                vec![source.clone()]
            } else {
                vec![source.clone(), resolved_elsewhere.clone()]
            })
        },
        Duration::from_secs(60),
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !entered.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
        thread::yield_now();
    }
    if !entered.load(Ordering::Acquire) {
        release.store(true, Ordering::Release);
        worker.stop();
        panic!("timed out waiting for fanout resolution to block");
    }
    let names = worker.names();
    let (sender, receiver) = std::sync::mpsc::channel();
    let lock_attempt = thread::spawn(move || {
        let _guard = names.lock().unwrap();
        sender.send(()).unwrap();
    });
    assert!(
        receiver.recv_timeout(Duration::from_millis(100)).is_ok(),
        "fanout resolution held the shared generated-name lock"
    );
    release.store(true, Ordering::Release);
    lock_attempt.join().unwrap();
    worker.stop();
}

#[test]
fn failed_refresh_is_not_retried_on_each_poll() {
    struct FailingNamer(Arc<AtomicUsize>);
    impl ConversationNamer for FailingNamer {
        fn read(&mut self, _: &NamingTarget) -> Result<NamingConversation> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(codex_mux::MuxError::Command("transient failure".to_owned()))
        }
        fn name(&mut self, _: &NamingConversation) -> Result<String> {
            unreachable!()
        }
    }

    let reads = Arc::new(AtomicUsize::new(0));
    let observed_reads = reads.clone();
    let current = target("%1", "01999999-1111-7777-8888-123456789abc");
    let worker = NamingWorker::spawn(
        move |_| Ok(FailingNamer(observed_reads)),
        move || Ok(vec![current.clone()]),
        Duration::from_millis(5),
    );
    wait_until("first failed refresh", || reads.load(Ordering::SeqCst) == 1);
    thread::sleep(Duration::from_millis(40));
    worker.stop();
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

#[test]
fn worker_discovers_future_targets_and_rejects_stale_results() {
    struct GatedNamer {
        reads: Arc<AtomicUsize>,
        names: Arc<AtomicUsize>,
        entered: Arc<std::sync::atomic::AtomicBool>,
        release: Arc<std::sync::atomic::AtomicBool>,
        finished: Arc<std::sync::atomic::AtomicBool>,
    }
    impl ConversationNamer for GatedNamer {
        fn read(&mut self, target: &NamingTarget) -> Result<NamingConversation> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(NamingConversation {
                thread_id: target.thread_hint.clone(),
                transcript: "completed chat".to_owned(),
                activity: String::new(),
            })
        }
        fn name(&mut self, _: &NamingConversation) -> Result<String> {
            let call = self.names.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                self.entered.store(true, Ordering::Release);
                while !self.release.load(Ordering::Acquire) {
                    thread::yield_now();
                }
                self.finished.store(true, Ordering::Release);
            }
            Ok("Useful generated name".to_owned())
        }
    }

    let reads = Arc::new(AtomicUsize::new(0));
    let names = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let current = Arc::new(Mutex::new(Vec::new()));
    let discovered = current.clone();
    let discovery_calls = Arc::new(AtomicUsize::new(0));
    let observed_discovery_calls = discovery_calls.clone();
    let provider_names = names.clone();
    let provider_entered = entered.clone();
    let provider_release = release.clone();
    let provider_finished = finished.clone();
    let worker = NamingWorker::spawn(
        move |_| {
            Ok(GatedNamer {
                reads,
                names: provider_names,
                entered: provider_entered,
                release: provider_release,
                finished: provider_finished,
            })
        },
        move || {
            observed_discovery_calls.fetch_add(1, Ordering::SeqCst);
            Ok(discovered.lock().unwrap().clone())
        },
        Duration::from_millis(10),
    );
    *current.lock().unwrap() = vec![target("%2", "01999999-2222-7777-8888-123456789abc")];
    wait_until("the naming provider to start", || {
        entered.load(Ordering::Acquire)
    });
    current.lock().unwrap().clear();
    let before_revalidation = discovery_calls.load(Ordering::SeqCst);
    release.store(true, Ordering::Release);
    wait_until("the stale result revalidation", || {
        finished.load(Ordering::Acquire)
            && discovery_calls.load(Ordering::SeqCst) != before_revalidation
    });
    assert!(
        worker.names().lock().unwrap().is_empty(),
        "stale result was published"
    );
    *current.lock().unwrap() = vec![target("%2", "01999999-3333-7777-8888-123456789abc")];
    for _ in 0..30 {
        if worker
            .names()
            .lock()
            .unwrap()
            .get(&PaneId::new("%2").unwrap())
            .is_some_and(|generated| generated.thread_id == "01999999-3333-7777-8888-123456789abc")
        {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(names.load(Ordering::SeqCst) >= 1);
    assert!(
        worker
            .names()
            .lock()
            .unwrap()
            .get(&PaneId::new("%2").unwrap())
            .is_some_and(|generated| {
                generated.thread_id == "01999999-3333-7777-8888-123456789abc"
            })
    );
    worker.stop();
}

#[test]
fn worker_re_resolves_truncated_identity_before_publishing() {
    struct ChangingResolver {
        resolutions: usize,
        names: Arc<AtomicUsize>,
    }
    impl ConversationNamer for ChangingResolver {
        fn resolve(&mut self, _: &NamingTarget) -> Result<String> {
            self.resolutions += 1;
            Ok(if self.resolutions % 2 == 1 {
                "01999999-4444-7777-8888-123456789abc"
            } else {
                "01999999-4444-7777-8888-abcdef012345"
            }
            .to_owned())
        }

        fn read(&mut self, target: &NamingTarget) -> Result<NamingConversation> {
            Ok(NamingConversation {
                thread_id: self.resolve(target)?,
                transcript: "completed chat".to_owned(),
                activity: String::new(),
            })
        }

        fn name(&mut self, _: &NamingConversation) -> Result<String> {
            self.names.fetch_add(1, Ordering::SeqCst);
            Ok("Must not publish".to_owned())
        }
    }

    let names = Arc::new(AtomicUsize::new(0));
    let observed_names = names.clone();
    let worker = NamingWorker::spawn(
        move |_| {
            Ok(ChangingResolver {
                resolutions: 0,
                names,
            })
        },
        move || Ok(vec![target("%8", "01999999-4444-7777-8888-12345...")]),
        Duration::from_millis(10),
    );
    wait_until("a generated name before identity revalidation", || {
        observed_names.load(Ordering::SeqCst) > 0
    });
    thread::sleep(Duration::from_millis(20));
    assert!(worker.names().lock().unwrap().is_empty());
    worker.stop();
}

#[test]
fn stop_cancels_active_provider_work_before_joining() {
    use std::sync::atomic::AtomicBool;

    struct BlockingNamer {
        cancelled: Arc<AtomicBool>,
        entered: Arc<AtomicBool>,
    }
    impl ConversationNamer for BlockingNamer {
        fn read(&mut self, target: &NamingTarget) -> Result<NamingConversation> {
            Ok(NamingConversation {
                thread_id: target.thread_hint.clone(),
                transcript: "chat".to_owned(),
                activity: String::new(),
            })
        }
        fn name(&mut self, _: &NamingConversation) -> Result<String> {
            self.entered.store(true, Ordering::Release);
            while !self.cancelled.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(5));
            }
            Err(codex_mux::MuxError::Command("cancelled".to_owned()))
        }
    }

    let entered = Arc::new(AtomicBool::new(false));
    let provider_entered = entered.clone();
    let worker = NamingWorker::spawn(
        move |cancelled| {
            Ok(BlockingNamer {
                cancelled,
                entered: provider_entered,
            })
        },
        || Ok(vec![target("%1", "01999999-1111-7777-8888-123456789abc")]),
        Duration::from_secs(1),
    );
    while !entered.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(5));
    }
    let started = std::time::Instant::now();
    worker.stop();
    assert!(started.elapsed() < Duration::from_millis(250));
}
