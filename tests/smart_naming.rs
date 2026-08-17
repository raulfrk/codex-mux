use std::{
    collections::VecDeque,
    sync::atomic::{AtomicUsize, Ordering},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
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
fn resolves_one_truncated_thread_in_exact_cwd_across_pages() {
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
            json!({"thread": {"turns": [
                {"status": "completed", "items": [
                    {"type": "userMessage", "content": [{"type": "text", "text": "name this"}]}
                ]}
            ]}}),
        ]),
        calls: calls.clone(),
    };
    let mut namer = AppServerNamer::new(session);
    let target = NamingTarget {
        pane_id: PaneId::new("%7").unwrap(),
        pane_title: "01a01001-2dbb-74e2-86ab-996b3...".to_owned(),
        thread_hint: "01a01001-2dbb-74e2-86ab-996b3".to_owned(),
        cwd: "/work/project".into(),
        generated_name: None,
        generated_at_unix: None,
    };

    let conversation = ConversationNamer::read(&mut namer, &target).unwrap();
    assert_eq!(conversation.thread_id, full);
    assert!(conversation.transcript.contains("User: name this"));
    let calls = calls.lock().unwrap();
    assert_eq!(calls[0].0, "thread/list");
    assert_eq!(calls[0].1["cwd"], "/work/project");
    assert_eq!(calls[0].1["useStateDbOnly"], true);
    assert_eq!(calls[1].1["cursor"], "page-2");
    assert_eq!(calls[2].0, "thread/read");
    assert_eq!(calls[2].1["threadId"], full);
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
        pane_title: "01a01001-2dbb-74e2-86ab-996b3...".to_owned(),
        thread_hint: "01a01001-2dbb-74e2-86ab-996b3".to_owned(),
        cwd: "/work/project".into(),
        generated_name: None,
        generated_at_unix: None,
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
        generated_at_unix: Some(1_700_000_000),
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
        generated_at_unix: None,
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
    fn read(&mut self, target: &NamingTarget) -> Result<NamingConversation> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(NamingConversation {
            thread_id: target.thread_hint.clone(),
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
        pane_title: thread.to_owned(),
        thread_hint: thread.to_owned(),
        cwd: "/work/project".into(),
        generated_name: None,
        generated_at_unix: None,
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
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if predicate() {
            return;
        }
        thread::yield_now();
    }
    panic!("timed out waiting for {description}");
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
fn worker_schedules_reads_only_at_initial_and_hourly_deadlines() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let reads = Arc::new(AtomicUsize::new(0));
    let names = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let discoveries = Arc::new(AtomicUsize::new(0));
    let new = target("%1", &thread_created_at(now - 590, "1"));
    let mut existing = target("%2", &thread_created_at(now - 7_200, "2"));
    existing.generated_name = Some("Stable entry title".to_owned());
    existing.generated_at_unix = Some(now - 3_590);
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
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    worker.stop();

    let reads = Arc::new(AtomicUsize::new(0));
    let observed_reads = reads.clone();
    let mut hourly = target("%4", &thread_created_at(now - 7_200, "4"));
    hourly.generated_name = Some("Hourly title".to_owned());
    hourly.generated_at_unix = Some(now - 3_610);
    let initial = target("%3", &thread_created_at(now - 610, "3"));
    let worker = NamingWorker::spawn(
        move |_| {
            Ok(CountingNamer {
                reads: observed_reads,
                names: Arc::new(AtomicUsize::new(0)),
                delay: Duration::ZERO,
            })
        },
        move || Ok(vec![initial.clone(), hourly.clone()]),
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
