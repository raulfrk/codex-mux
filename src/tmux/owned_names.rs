//! Pane-local metadata backing generated titles rendered only by Codex Mux.

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
};

use crate::{
    MuxError, Result,
    domain::{PaneId, TmuxCommandRunner},
    smart_naming::GeneratedName,
};

const SEPARATOR: char = '\x1f';
const OWNER_OPTION: &str = "@codex_mux_generated_name";
const THREAD_OPTION: &str = "@codex_mux_generated_thread";
const SOURCE_TITLE_OPTION: &str = "@codex_mux_generated_source_title";
const SOURCE_CWD_OPTION: &str = "@codex_mux_generated_source_cwd";
const SOURCE_PID_OPTION: &str = "@codex_mux_generated_source_pid";
const SOURCE_SESSION_OPTION: &str = "@codex_mux_generated_source_session";
const GENERATED_AT_OPTION: &str = "@codex_mux_generated_at";
/// Pane-local marker used to wake naming after Codex Resume opens its selector.
pub const IMMEDIATE_NAMING_OPTION: &str = "@codex_mux_name_now";
/// Pane-local marker preserving a title explicitly saved by the user.
pub const MANUAL_NAME_OPTION: &str = "@codex_mux_manual_name";
/// Pane-local original thread title used to safely resume Smart Naming.
pub const MANUAL_NAME_SOURCE_OPTION: &str = "@codex_mux_manual_name_source";
/// Pane leader identity retained with a manual name.
pub const MANUAL_NAME_PID_OPTION: &str = "@codex_mux_manual_name_pid";
/// Tmux session retained with a manual name.
pub const MANUAL_NAME_SESSION_OPTION: &str = "@codex_mux_manual_name_session";
/// Pane-local marker used after source-less unpin until Codex exposes a new exact title.
pub const UNPIN_WAITING_OPTION: &str = "@codex_mux_unpin_waiting";
/// User title that must change before a source-less unpin can be adopted again.
pub const UNPIN_WAITING_TITLE_OPTION: &str = "@codex_mux_unpin_waiting_title";
/// Pane leader retained with a source-less unpin wait.
pub const UNPIN_WAITING_PID_OPTION: &str = "@codex_mux_unpin_waiting_pid";
/// Tmux session retained with a source-less unpin wait.
pub const UNPIN_WAITING_SESSION_OPTION: &str = "@codex_mux_unpin_waiting_session";
/// Transient marker proving that an unpin title restore completed.
pub const UNPIN_READY_OPTION: &str = "@codex_mux_unpin_ready";
/// Transient marker proving a guarded unpin completed.
pub const UNPIN_COMPLETE_OPTION: &str = "@codex_mux_unpin_complete";
/// Transient marker proving a guarded rename completed.
pub const RENAME_COMPLETE_OPTION: &str = "@codex_mux_rename_complete";
const STATE_FORMAT: &str = "#{pane_id}\x1f#{pane_title}\x1f#{pane_current_path}\x1f#{pane_pid}\x1f#{session_id}\x1f#{@codex_mux_generated_thread}\x1f#{@codex_mux_generated_name}\x1f#{@codex_mux_generated_source_title}\x1f#{@codex_mux_generated_source_cwd}\x1f#{@codex_mux_generated_source_pid}\x1f#{@codex_mux_generated_source_session}\x1f#{@codex_mux_generated_at}\x1f#{@codex_mux_name_now}\x1f#{@codex_mux_manual_name}";
const LEGACY_STATE_FORMAT: &str = "#{pane_id}\x1f#{window_id}\x1f#{pane_title}\x1f#{window_name}\x1f#{automatic-rename}\x1f#{window_panes}\x1f#{@codex_mux_generated_thread}\x1f#{@codex_mux_generated_name}\x1f#{@codex_mux_generated_source_title}\x1f#{@codex_mux_generated_source_cwd}\x1f#{@codex_mux_generated_at}\x1f#{pane_current_path}";

/// Applies generated titles as pane-local metadata without mutating tmux window names.
pub struct OwnedTmuxNames<R> {
    runner: R,
}

impl<R: TmuxCommandRunner> OwnedTmuxNames<R> {
    /// Creates an owned-name reconciler over an injectable tmux runner.
    #[must_use]
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    /// Reconciles a worker snapshot against live, exact pane targets.
    pub fn reconcile(&self, names: &HashMap<PaneId, GeneratedName>) -> bool {
        self.reconcile_with_verified_volatile(names, &HashSet::new())
    }

    /// Reconciles names after exact rollout revalidation of volatile-title panes.
    pub fn reconcile_with_verified_volatile(
        &self,
        names: &HashMap<PaneId, GeneratedName>,
        verified_volatile: &HashSet<PaneId>,
    ) -> bool {
        let Ok(output) = self.run(["list-panes", "-a", "-F", STATE_FORMAT]) else {
            return false;
        };
        let states = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let fields = split_state_fields(line);
                (fields.len() == 14).then(|| (fields[0].to_owned(), fields[1..].join("\x1f")))
            })
            .collect::<HashMap<_, _>>();
        let immediate_pending = states.values().any(|state| {
            state
                .split(SEPARATOR)
                .nth(11)
                .is_some_and(|marker| marker == "1")
                && state
                    .split(SEPARATOR)
                    .nth(12)
                    .is_none_or(|marker| marker != "1")
        });
        for (pane_id, generated) in names {
            if !generated.stable_source_title && !verified_volatile.contains(pane_id) {
                continue;
            }
            if let Some(state) = states.get(pane_id.as_str()) {
                let _ = self.reconcile_one(pane_id, generated, state);
            }
        }
        immediate_pending
    }

    fn reconcile_one(
        &self,
        pane_id: &PaneId,
        generated: &GeneratedName,
        state: &str,
    ) -> Result<()> {
        let fields = state.split(SEPARATOR).collect::<Vec<_>>();
        if fields.len() != 13
            || fields[12] == "1"
            || fields[1] != generated.source_cwd.to_string_lossy()
            || fields[2] != generated.source_pane_pid.to_string()
            || fields[3] != generated.source_session.as_str()
            || (generated.stable_source_title && fields[0] != generated.source_title)
        {
            return Ok(());
        }
        if fields[4] == generated.thread_id
            && fields[5] == generated.name
            && fields[6] == generated.source_title
            && fields[7] == generated.source_cwd.to_string_lossy()
            && fields[8] == generated.source_pane_pid.to_string()
            && fields[9] == generated.source_session.as_str()
            && fields[10] == generated.generated_at_unix.to_string()
            && fields[11].is_empty()
            && fields[12].is_empty()
        {
            return Ok(());
        }

        let mutation = format!(
            "set-option -p -t {} {} {}; set-option -p -t {} {} {}; set-option -p -t {} {} {}; set-option -p -t {} {} {}; set-option -p -t {} {} {}; set-option -p -t {} {} {}; set-option -p -t {} {} {}; set-option -pu -t {} {}",
            tmux_quote(pane_id.as_str()),
            SOURCE_TITLE_OPTION,
            tmux_quote(&generated.source_title),
            tmux_quote(pane_id.as_str()),
            SOURCE_CWD_OPTION,
            tmux_quote(generated.source_cwd.to_string_lossy().as_ref()),
            tmux_quote(pane_id.as_str()),
            THREAD_OPTION,
            tmux_quote(&generated.thread_id),
            tmux_quote(pane_id.as_str()),
            OWNER_OPTION,
            tmux_quote(&generated.name),
            tmux_quote(pane_id.as_str()),
            GENERATED_AT_OPTION,
            generated.generated_at_unix,
            tmux_quote(pane_id.as_str()),
            SOURCE_PID_OPTION,
            generated.source_pane_pid,
            tmux_quote(pane_id.as_str()),
            SOURCE_SESSION_OPTION,
            tmux_quote(generated.source_session.as_str()),
            tmux_quote(pane_id.as_str()),
            IMMEDIATE_NAMING_OPTION,
        );
        let captured_cwd = tmux_format_literal(generated.source_cwd.to_string_lossy().as_ref());
        let title_condition = if generated.stable_source_title {
            format!(
                "#{{==:#{{pane_title}},{}}}",
                tmux_format_literal(&generated.source_title)
            )
        } else {
            "1".to_owned()
        };
        let captured_session = tmux_format_literal(generated.source_session.as_str());
        let identity_condition = format!(
            "#{{&&:#{{==:#{{pane_pid}},{}}},#{{==:#{{session_id}},{captured_session}}}}}",
            generated.source_pane_pid
        );
        let condition = format!(
            "#{{&&:#{{==:#{{{MANUAL_NAME_OPTION}}},}},#{{&&:{title_condition},#{{&&:#{{==:#{{pane_current_path}},{captured_cwd}}},{identity_condition}}}}}}}"
        );
        self.run([
            "if-shell",
            "-F",
            "-t",
            pane_id.as_str(),
            &condition,
            &mutation,
        ])?;
        Ok(())
    }

    /// Migrates titles created by versions that renamed one-pane tmux windows.
    pub fn migrate_legacy_window_names(&self, generated_at_unix: u64) {
        let Ok(output) = self.run(["list-panes", "-a", "-F", LEGACY_STATE_FORMAT]) else {
            return;
        };
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let fields = split_state_fields(line);
            if fields.len() != 12
                || fields[4] != "0"
                || fields[5] != "1"
                || fields[3] != fields[7]
                || fields[6].trim().is_empty()
                || fields[7].trim().is_empty()
                || !fields[10].trim().is_empty()
            {
                continue;
            }
            let pane_id = fields[0];
            let window_id = fields[1];
            let pane_condition = format!(
                "#{{&&:#{{==:#{{window_id}},{window_id}}},#{{&&:#{{==:#{{pane_title}},#{{{SOURCE_TITLE_OPTION}}}}},#{{==:#{{pane_current_path}},#{{{SOURCE_CWD_OPTION}}}}}}}}}"
            );
            let pane_mutation = format!(
                "set-option -p -t {} {} {}; set-option -p -t {} {} {}; set-option -p -t {} {} {}",
                tmux_quote(pane_id),
                THREAD_OPTION,
                tmux_quote(fields[6]),
                tmux_quote(pane_id),
                OWNER_OPTION,
                tmux_quote(fields[7]),
                tmux_quote(pane_id),
                GENERATED_AT_OPTION,
                generated_at_unix,
            );
            let captured_thread = tmux_format_literal(fields[6]);
            let captured_name = tmux_format_literal(fields[7]);
            let condition = format!(
                "#{{&&:#{{==:#{{window_id}},{window_id}}},#{{&&:#{{==:#{{window_panes}},1}},#{{&&:#{{==:#{{automatic-rename}},0}},#{{&&:#{{==:#{{window_name}},#{{{OWNER_OPTION}}}}},#{{&&:#{{==:#{{{THREAD_OPTION}}},{captured_thread}}},#{{&&:#{{==:#{{{OWNER_OPTION}}},{captured_name}}},#{{==:#{{{GENERATED_AT_OPTION}}},}}}}}}}}}}}}}}"
            );
            let mutation = format!(
                "set-option -wu -t {} {}; set-option -wu -t {} {}; set-option -w -t {} automatic-rename on; if-shell -F -t {} {} {}",
                tmux_quote(window_id),
                THREAD_OPTION,
                tmux_quote(window_id),
                OWNER_OPTION,
                tmux_quote(window_id),
                tmux_quote(pane_id),
                tmux_quote(&pane_condition),
                tmux_quote(&pane_mutation),
            );
            let _ = self.run(["if-shell", "-F", "-t", window_id, &condition, &mutation]);
        }
    }

    /// Removes every pane-local Codex Mux title when the feature is disabled.
    pub fn clear_all(&self) {
        let Ok(output) = self.run(["list-panes", "-a", "-F", "#{pane_id}"]) else {
            return;
        };
        for pane_id in String::from_utf8_lossy(&output.stdout).lines() {
            let _ = self.unset_marker(pane_id);
        }
    }

    /// Removes volatile metadata whose exact current rollout identity failed revalidation.
    pub fn clear_generated(&self, panes: &HashMap<PaneId, GeneratedName>) {
        for (pane, generated) in panes {
            let captured_thread = tmux_format_literal(&generated.thread_id);
            let captured_name = tmux_format_literal(&generated.name);
            let captured_session = tmux_format_literal(generated.source_session.as_str());
            let thread = format!("#{{==:#{{{THREAD_OPTION}}},{captured_thread}}}");
            let name = format!("#{{==:#{{{OWNER_OPTION}}},{captured_name}}}");
            let pid = format!(
                "#{{==:#{{{SOURCE_PID_OPTION}}},{}}}",
                generated.source_pane_pid
            );
            let session = format!("#{{==:#{{{SOURCE_SESSION_OPTION}}},{captured_session}}}");
            let pid_session = format!("#{{&&:{pid},{session}}}");
            let name_and_identity = format!("#{{&&:{name},{pid_session}}}");
            let condition = format!("#{{&&:{thread},{name_and_identity}}}");
            let mutation = clear_marker_command(pane.as_str());
            let _ = self.run(["if-shell", "-F", "-t", pane.as_str(), &condition, &mutation]);
        }
    }

    fn unset_marker(&self, pane_id: &str) -> Result<()> {
        self.run_arguments(clear_marker_arguments(pane_id))?;
        Ok(())
    }

    fn run<const N: usize>(&self, arguments: [&str; N]) -> Result<crate::domain::CommandOutput> {
        self.run_arguments(arguments.into_iter().map(OsString::from).collect())
    }

    fn run_arguments(&self, arguments: Vec<OsString>) -> Result<crate::domain::CommandOutput> {
        let output = self.runner.run(&arguments)?;
        if output.status == Some(0) {
            Ok(output)
        } else {
            Err(MuxError::Command(
                "tmux smart naming command failed".to_owned(),
            ))
        }
    }
}

/// Builds the exact pane-local tmux operations that relinquish generated-name ownership.
///
/// The caller may prepend or append another command in the same tmux invocation. Every
/// user-supplied value remains a distinct argv item; this helper never builds shell input.
pub(crate) fn clear_marker_arguments(pane_id: &str) -> Vec<OsString> {
    [
        "set-option",
        "-pu",
        "-t",
        pane_id,
        THREAD_OPTION,
        ";",
        "set-option",
        "-pu",
        "-t",
        pane_id,
        OWNER_OPTION,
        ";",
        "set-option",
        "-pu",
        "-t",
        pane_id,
        SOURCE_TITLE_OPTION,
        ";",
        "set-option",
        "-pu",
        "-t",
        pane_id,
        SOURCE_CWD_OPTION,
        ";",
        "set-option",
        "-pu",
        "-t",
        pane_id,
        SOURCE_PID_OPTION,
        ";",
        "set-option",
        "-pu",
        "-t",
        pane_id,
        SOURCE_SESSION_OPTION,
        ";",
        "set-option",
        "-pu",
        "-t",
        pane_id,
        GENERATED_AT_OPTION,
        ";",
        "set-option",
        "-pu",
        "-t",
        pane_id,
        IMMEDIATE_NAMING_OPTION,
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

fn clear_marker_command(pane_id: &str) -> String {
    [
        THREAD_OPTION,
        OWNER_OPTION,
        SOURCE_TITLE_OPTION,
        SOURCE_CWD_OPTION,
        SOURCE_PID_OPTION,
        SOURCE_SESSION_OPTION,
        GENERATED_AT_OPTION,
        IMMEDIATE_NAMING_OPTION,
    ]
    .into_iter()
    .map(|option| format!("set-option -pu -t {} {}", tmux_quote(pane_id), option))
    .collect::<Vec<_>>()
    .join("; ")
}

fn split_state_fields(line: &str) -> Vec<&str> {
    if line.contains(SEPARATOR) {
        line.split(SEPARATOR).collect()
    } else {
        line.split("\\037").collect()
    }
}

fn tmux_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn tmux_format_literal(value: &str) -> String {
    value
        .replace('#', "##")
        .replace(',', "#,")
        .replace('}', "#}")
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, VecDeque},
        ffi::OsString,
        sync::{Arc, Mutex},
    };

    use crate::domain::{CommandOutput, PaneId, TmuxCommandRunner};

    use super::*;

    const THREAD: &str = "12345678-1234-1234-1234-123456789abc";
    const GENERATED_AT: u64 = 1_700_000_000;

    #[derive(Clone)]
    struct FakeRunner {
        outputs: Arc<Mutex<VecDeque<CommandOutput>>>,
        calls: Arc<Mutex<Vec<Vec<OsString>>>>,
    }

    impl FakeRunner {
        fn with_states(states: &[&str]) -> Self {
            Self {
                outputs: Arc::new(Mutex::new(
                    states
                        .iter()
                        .map(|state| CommandOutput {
                            stdout: state.as_bytes().to_vec(),
                            stderr: Vec::new(),
                            status: Some(0),
                        })
                        .collect(),
                )),
                calls: Arc::default(),
            }
        }
    }

    impl TmuxCommandRunner for FakeRunner {
        fn run(&self, arguments: &[OsString]) -> Result<CommandOutput> {
            self.calls.lock().unwrap().push(arguments.to_vec());
            Ok(self
                .outputs
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(CommandOutput {
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    status: Some(0),
                }))
        }
    }

    fn names(name: &str) -> HashMap<PaneId, GeneratedName> {
        HashMap::from([(
            PaneId::new("%7").unwrap(),
            GeneratedName {
                thread_id: THREAD.to_owned(),
                source_session: crate::domain::SessionId::new("$1").unwrap(),
                source_pane_pid: 77,
                stable_source_title: true,
                source_title: THREAD.to_owned(),
                source_cwd: "/work/project".into(),
                name: name.to_owned(),
                generated_at_unix: GENERATED_AT,
            },
        )])
    }

    fn state(
        title: &str,
        cwd: &str,
        thread: &str,
        name: &str,
        source_title: &str,
        source_cwd: &str,
        generated_at: &str,
    ) -> String {
        format!(
            "%7\x1f{title}\x1f{cwd}\x1f77\x1f$1\x1f{thread}\x1f{name}\x1f{source_title}\x1f{source_cwd}\x1f77\x1f$1\x1f{generated_at}\x1f\x1f\n"
        )
    }

    fn calls(runner: &FakeRunner) -> Vec<Vec<String>> {
        runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| {
                call.iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect()
            })
            .collect()
    }

    fn if_shell(calls: &[Vec<String>]) -> (&str, &str) {
        let call = calls
            .iter()
            .find(|call| call.first().is_some_and(|command| command == "if-shell"))
            .expect("an atomic conditional mutation");
        (&call[4], &call[5])
    }

    #[test]
    fn stores_entry_title_as_pane_metadata_without_touching_the_window() {
        let current = state(THREAD, "/work/project", "", "", "", "", "");
        let runner = FakeRunner::with_states(&[&current]);
        OwnedTmuxNames::new(runner.clone()).reconcile(&names("Fast inventory"));

        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        let (condition, mutation) = if_shell(&calls);
        assert!(condition.contains(MANUAL_NAME_OPTION));
        assert!(condition.contains("#{pane_title}"));
        assert!(condition.contains("#{pane_current_path}"));
        assert!(mutation.contains("set-option -p"));
        assert!(mutation.contains(THREAD_OPTION));
        assert!(mutation.contains(OWNER_OPTION));
        assert!(mutation.contains(GENERATED_AT_OPTION));
        assert!(!mutation.contains("rename-window"));
        assert!(!mutation.contains("automatic-rename"));
        assert!(!calls.iter().flatten().any(|argument| argument == "-w"));
    }

    #[test]
    fn unchanged_entry_metadata_requires_no_writes() {
        let current = state(
            THREAD,
            "/work/project",
            THREAD,
            "Stable title",
            THREAD,
            "/work/project",
            &GENERATED_AT.to_string(),
        );
        let runner = FakeRunner::with_states(&[&current]);
        OwnedTmuxNames::new(runner.clone()).reconcile(&names("Stable title"));
        assert_eq!(calls(&runner).len(), 1);
    }

    #[test]
    fn stale_title_or_cwd_is_never_updated() {
        for current in [
            state(
                "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                "/work/project",
                "",
                "",
                "",
                "",
                "",
            ),
            state(THREAD, "/work/other", "", "", "", "", ""),
        ] {
            let runner = FakeRunner::with_states(&[&current]);
            OwnedTmuxNames::new(runner.clone()).reconcile(&names("Wrong target"));
            assert_eq!(calls(&runner).len(), 1);
        }
    }

    #[test]
    fn authoritative_recovery_accepts_a_volatile_title_for_the_same_pane_process() {
        let current = state("⠹ volatile title", "/work/project", "", "", "", "", "");
        let runner = FakeRunner::with_states(&[&current]);
        let mut generated = names("Recovered conversation");
        let value = generated.get_mut(&PaneId::new("%7").unwrap()).unwrap();
        value.source_title = "⠸ earlier volatile title".to_owned();
        value.stable_source_title = false;

        OwnedTmuxNames::new(runner.clone()).reconcile_with_verified_volatile(
            &generated,
            &HashSet::from([PaneId::new("%7").unwrap()]),
        );

        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        let (condition, mutation) = if_shell(&calls);
        assert!(!condition.contains("earlier volatile title"));
        assert!(condition.contains("#{pane_pid}"));
        assert!(condition.contains("#{session_id}"));
        assert!(mutation.contains("Recovered conversation"));
    }

    #[test]
    fn volatile_title_recovery_rejects_a_reused_pane_process_or_session() {
        for current in [
            "%7\x1f⠹ volatile title\x1f/work/project\x1f78\x1f$1\x1f\x1f\x1f\x1f\x1f\x1f\x1f\x1f\x1f\n",
            "%7\x1f⠹ volatile title\x1f/work/project\x1f77\x1f$2\x1f\x1f\x1f\x1f\x1f\x1f\x1f\x1f\x1f\n",
        ] {
            let runner = FakeRunner::with_states(&[current]);
            let mut generated = names("Wrong conversation");
            generated
                .get_mut(&PaneId::new("%7").unwrap())
                .unwrap()
                .stable_source_title = false;

            OwnedTmuxNames::new(runner.clone()).reconcile_with_verified_volatile(
                &generated,
                &HashSet::from([PaneId::new("%7").unwrap()]),
            );
            assert_eq!(calls(&runner).len(), 1);
        }
    }

    #[test]
    fn truncated_source_title_can_store_a_full_thread_marker() {
        let title = "12345678-1234-1234-1234-12345...";
        let cwd = "/work/#{hostile},path";
        let current = state(title, cwd, "", "", "", "", "");
        let runner = FakeRunner::with_states(&[&current]);
        let mut generated = names("Resolved title");
        let value = generated.get_mut(&PaneId::new("%7").unwrap()).unwrap();
        value.source_title = title.to_owned();
        value.source_cwd = cwd.into();

        OwnedTmuxNames::new(runner.clone()).reconcile(&generated);

        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        let (_, mutation) = if_shell(&calls);
        assert!(mutation.contains(THREAD));
        assert!(!mutation.contains("rename-window"));
    }

    #[test]
    fn tmux_34_escaped_state_fields_are_supported() {
        let current =
            state(THREAD, "/work/project", "", "", "", "", "").replace(SEPARATOR, "\\037");
        let runner = FakeRunner::with_states(&[&current]);
        OwnedTmuxNames::new(runner.clone()).reconcile(&names("Escaped fields"));
        assert_eq!(calls(&runner).len(), 2);
    }

    #[test]
    fn pending_resume_marker_wakes_reconciliation_and_clears_after_safe_name_write() {
        let current = format!(
            "%7\x1f{THREAD}\x1f/work/project\x1f77\x1f$1\x1f\x1f\x1f\x1f\x1f\x1f\x1f\x1f1\x1f\n"
        );
        let runner = FakeRunner::with_states(&[&current]);

        assert!(OwnedTmuxNames::new(runner.clone()).reconcile(&names("Resumed work")));

        let calls = calls(&runner);
        let (_, mutation) = if_shell(&calls);
        assert!(mutation.contains("set-option -pu"));
        assert!(mutation.contains(IMMEDIATE_NAMING_OPTION));
    }

    #[test]
    fn manual_marker_rejects_stale_generated_name_writes() {
        let current = format!(
            "%7\x1f{THREAD}\x1f/work/project\x1f77\x1f$1\x1f\x1f\x1f\x1f\x1f\x1f\x1f\x1f\x1f1\n"
        );
        let runner = FakeRunner::with_states(&[&current]);

        assert!(!OwnedTmuxNames::new(runner.clone()).reconcile(&names("Stale title")));
        assert_eq!(calls(&runner).len(), 1, "manual pane must not be mutated");
    }

    #[test]
    fn legacy_owned_window_is_restored_despite_source_drift() {
        let legacy = format!(
            "%7\x1f@7\x1fchanged title\x1fOld smart title\x1f0\x1f1\x1f{THREAD}\x1fOld smart title\x1f{THREAD}\x1f/work/project\x1f\x1f/work/changed\n"
        );
        let runner = FakeRunner::with_states(&[&legacy]);
        OwnedTmuxNames::new(runner.clone()).migrate_legacy_window_names(GENERATED_AT);

        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        let mutation = calls[1].join(" ");
        assert!(mutation.contains("if-shell -F -t @7"));
        assert!(mutation.contains("set-option -wu -t '@7'"));
        assert!(mutation.contains("set-option -p -t"));
        assert!(mutation.contains("%7"));
        assert!(mutation.contains(GENERATED_AT_OPTION));
        assert!(mutation.contains("automatic-rename on"));
        assert!(calls[1].join(" ").contains(THREAD));
    }

    #[test]
    fn disabling_clears_only_pane_local_codex_mux_metadata() {
        let runner = FakeRunner::with_states(&["%7\n"]);
        OwnedTmuxNames::new(runner.clone()).clear_all();

        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        for option in [
            THREAD_OPTION,
            OWNER_OPTION,
            SOURCE_TITLE_OPTION,
            SOURCE_CWD_OPTION,
            GENERATED_AT_OPTION,
        ] {
            assert!(calls[1].iter().any(|argument| argument == option));
        }
        assert!(!calls[1].iter().any(|argument| argument == "-w"));
    }

    #[test]
    fn command_values_are_quoted_for_tmux_parser() {
        assert_eq!(tmux_quote("don't"), "'don'\\''t'");
        assert_eq!(tmux_format_literal("a#,}b"), "a###,#}b");
    }
}
