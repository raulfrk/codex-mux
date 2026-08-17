use std::{
    collections::VecDeque,
    sync::atomic::{AtomicUsize, Ordering},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use codex_mux::{
    Result,
    smart_naming::{
        AppServerNamer, AppServerProcess, AppServerSession, ConversationNamer,
        MAX_CONVERSATION_BYTES, NAMING_MODEL, NamingConversation, NamingTarget, NamingWorker,
        start_if_enabled,
    },
};
use serde_json::{Value, json};

use codex_mux::domain::PaneId;

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

struct CountingNamer {
    reads: Arc<AtomicUsize>,
    names: Arc<AtomicUsize>,
    delay: Duration,
}

impl ConversationNamer for CountingNamer {
    fn read(&mut self, thread_id: &str) -> Result<NamingConversation> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(NamingConversation {
            thread_id: thread_id.to_owned(),
            transcript: "completed chat".to_owned(),
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
        thread_id: thread.to_owned(),
    }
}

#[test]
fn worker_is_non_blocking_deduplicates_and_joins_on_stop() {
    let reads = Arc::new(AtomicUsize::new(0));
    let names = Arc::new(AtomicUsize::new(0));
    let current = Arc::new(Mutex::new(vec![
        target("%1", "01999999-1111-7777-8888-123456789abc"),
        target("%4", "01999999-1111-7777-8888-123456789abc"),
    ]));
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
    assert!(
        worker
            .names()
            .lock()
            .unwrap()
            .contains_key(&PaneId::new("%4").unwrap())
    );
    worker.stop();
}

#[test]
fn worker_discovers_future_targets_and_rejects_stale_results() {
    let reads = Arc::new(AtomicUsize::new(0));
    let names = Arc::new(AtomicUsize::new(0));
    let current = Arc::new(Mutex::new(Vec::new()));
    let discovered = current.clone();
    let provider_names = names.clone();
    let worker = NamingWorker::spawn(
        move |_| {
            Ok(CountingNamer {
                reads,
                names: provider_names,
                delay: Duration::from_millis(50),
            })
        },
        move || Ok(discovered.lock().unwrap().clone()),
        Duration::from_millis(10),
    );
    *current.lock().unwrap() = vec![target("%2", "01999999-2222-7777-8888-123456789abc")];
    thread::sleep(Duration::from_millis(20));
    current.lock().unwrap().clear();
    thread::sleep(Duration::from_millis(100));
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
fn stop_cancels_active_provider_work_before_joining() {
    use std::sync::atomic::AtomicBool;

    struct BlockingNamer {
        cancelled: Arc<AtomicBool>,
        entered: Arc<AtomicBool>,
    }
    impl ConversationNamer for BlockingNamer {
        fn read(&mut self, thread_id: &str) -> Result<NamingConversation> {
            Ok(NamingConversation {
                thread_id: thread_id.to_owned(),
                transcript: "chat".to_owned(),
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
