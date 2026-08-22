//! Pane-local metadata backing generated titles rendered only by Codex Mux.

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
};

use crate::{
    MuxError, Result,
    domain::{AutoNameStatus, Pane, PaneId, TmuxCommandRunner},
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
/// Privacy-safe stage for an explicit forced automatic-name request.
pub const AUTO_NAME_STATUS_OPTION: &str = "@codex_mux_auto_name_status";
/// Unix timestamp bounding an explicit forced automatic-name request.
pub const AUTO_NAME_STARTED_OPTION: &str = "@codex_mux_auto_name_started";
/// Opaque operation token distinguishing superseded automatic-name requests.
pub const AUTO_NAME_TOKEN_OPTION: &str = "@codex_mux_auto_name_token";
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
const STATE_FORMAT: &str = "#{pane_id}\x1f#{pane_title}\x1f#{pane_current_path}\x1f#{pane_pid}\x1f#{session_id}\x1f#{@codex_mux_generated_thread}\x1f#{@codex_mux_generated_name}\x1f#{@codex_mux_generated_source_title}\x1f#{@codex_mux_generated_source_cwd}\x1f#{@codex_mux_generated_source_pid}\x1f#{@codex_mux_generated_source_session}\x1f#{@codex_mux_generated_at}\x1f#{@codex_mux_name_now}\x1f#{@codex_mux_manual_name}\x1f#{@codex_mux_auto_name_status}\x1f#{@codex_mux_auto_name_started}\x1f#{@codex_mux_auto_name_token}";
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
                matches!(fields.len(), 14..=17)
                    .then(|| (fields[0].to_owned(), fields[1..].join("\x1f")))
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
        if !matches!(fields.len(), 13..=16)
            || fields[12] == "1"
            || fields[1] != generated.source_cwd.to_string_lossy()
            || fields[2] != generated.source_pane_pid.to_string()
            || fields[3] != generated.source_session.as_str()
            || (generated.stable_source_title && fields[0] != generated.source_title)
        {
            return Ok(());
        }
        let forced_pending = fields[11] == "1"
            && fields
                .get(13)
                .is_some_and(|status| matches!(*status, "recovering" | "queued" | "generating"));
        let request_token = fields.get(15).filter(|token| !token.is_empty()).copied();
        if forced_pending
            && (request_token.is_none() || generated.auto_name_token.as_deref() != request_token)
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

        let mut mutation = format!(
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
        if forced_pending {
            mutation.push_str(&format!(
                "; set-option -p -t {pane} {status} success; set-option -pu -t {pane} {token}; set-option -pu -t {pane} {waiting}; set-option -pu -t {pane} {waiting_title}; set-option -pu -t {pane} {waiting_pid}; set-option -pu -t {pane} {waiting_session}",
                pane = tmux_quote(pane_id.as_str()),
                status = AUTO_NAME_STATUS_OPTION,
                token = AUTO_NAME_TOKEN_OPTION,
                waiting = UNPIN_WAITING_OPTION,
                waiting_title = UNPIN_WAITING_TITLE_OPTION,
                waiting_pid = UNPIN_WAITING_PID_OPTION,
                waiting_session = UNPIN_WAITING_SESSION_OPTION,
            ));
        }
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
        let mut condition = format!(
            "#{{&&:#{{==:#{{{MANUAL_NAME_OPTION}}},}},#{{&&:{title_condition},#{{&&:#{{==:#{{pane_current_path}},{captured_cwd}}},{identity_condition}}}}}}}"
        );
        if forced_pending {
            let status = tmux_format_literal(fields[13]);
            let started = tmux_format_literal(fields.get(14).copied().unwrap_or_default());
            let token = tmux_format_literal(request_token.expect("forced request token"));
            condition = format!(
                "#{{&&:{condition},#{{&&:#{{==:#{{{IMMEDIATE_NAMING_OPTION}}},1}},#{{&&:#{{==:#{{{AUTO_NAME_STATUS_OPTION}}},{status}}},#{{&&:#{{==:#{{{AUTO_NAME_STARTED_OPTION}}},{started}}},#{{==:#{{{AUTO_NAME_TOKEN_OPTION}}},{token}}}}}}}}}}}"
            );
        }
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

    /// Advances exact explicit requests once a trustworthy naming target exists.
    pub fn mark_auto_name_generating(&self, panes: &[Pane]) {
        for pane in panes.iter().filter(|pane| {
            pane.immediate_naming
                && !pane.manual_name
                && (!pane.unpin_waiting
                    || (pane.unpin_waiting_title.as_deref() != pane.title.as_deref()
                        && pane.unpin_waiting_pid == Some(pane.pane_pid)
                        && pane.unpin_waiting_session.as_ref() == Some(&pane.session_id)))
                && matches!(
                    pane.auto_name_status,
                    Some(AutoNameStatus::RecoveringIdentity | AutoNameStatus::Queued)
                )
        }) {
            let (Some(token), Some(started), Some(status)) = (
                pane.auto_name_token.as_deref(),
                pane.auto_name_started_at_unix_nanos,
                pane.auto_name_status,
            ) else {
                continue;
            };
            let status = match status {
                AutoNameStatus::RecoveringIdentity => "recovering",
                AutoNameStatus::Queued => "queued",
                AutoNameStatus::Generating | AutoNameStatus::Succeeded => continue,
            };
            let condition = [
                format!("#{{==:#{{pane_pid}},{}}}", pane.pane_pid),
                format!(
                    "#{{==:#{{session_id}},{}}}",
                    tmux_format_literal(pane.session_id.as_str())
                ),
                format!("#{{==:#{{{IMMEDIATE_NAMING_OPTION}}},1}}"),
                format!(
                    "#{{==:#{{{AUTO_NAME_STATUS_OPTION}}},{}}}",
                    tmux_format_literal(status)
                ),
                format!("#{{==:#{{{AUTO_NAME_STARTED_OPTION}}},{started}}}"),
                format!(
                    "#{{==:#{{{AUTO_NAME_TOKEN_OPTION}}},{}}}",
                    tmux_format_literal(token)
                ),
            ]
            .into_iter()
            .reduce(|left, right| format!("#{{&&:{left},{right}}}"))
            .expect("auto-name condition has clauses");
            let mutation = format!(
                "set-option -p -t {} {} generating",
                tmux_quote(pane.id.as_str()),
                AUTO_NAME_STATUS_OPTION,
            );
            let _ = self.run([
                "if-shell",
                "-F",
                "-t",
                pane.id.as_str(),
                &condition,
                &mutation,
            ]);
        }
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
        ";",
        "set-option",
        "-pu",
        "-t",
        pane_id,
        AUTO_NAME_STATUS_OPTION,
        ";",
        "set-option",
        "-pu",
        "-t",
        pane_id,
        AUTO_NAME_STARTED_OPTION,
        ";",
        "set-option",
        "-pu",
        "-t",
        pane_id,
        AUTO_NAME_TOKEN_OPTION,
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
        AUTO_NAME_STATUS_OPTION,
        AUTO_NAME_STARTED_OPTION,
        AUTO_NAME_TOKEN_OPTION,
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
        names_with_token(name, None)
    }

    fn names_with_token(
        name: &str,
        auto_name_token: Option<&str>,
    ) -> HashMap<PaneId, GeneratedName> {
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
                auto_name_token: auto_name_token.map(ToOwned::to_owned),
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
    fn successful_forced_request_records_a_terminal_status() {
        let base = state(THREAD, "/work/project", "", "", "", "", "");
        let prefix = base.strip_suffix("\x1f\x1f\n").unwrap();
        let current = format!("{prefix}\x1f1\x1f\x1fgenerating\x1f1000000000\x1frequest-token\n");
        let runner = FakeRunner::with_states(&[&current]);
        OwnedTmuxNames::new(runner.clone()).reconcile(&names_with_token(
            "Project workspace",
            Some("request-token"),
        ));

        let calls = calls(&runner);
        let (condition, mutation) = if_shell(&calls);
        assert!(condition.contains(IMMEDIATE_NAMING_OPTION));
        assert!(condition.contains(AUTO_NAME_STATUS_OPTION));
        assert!(condition.contains("generating"));
        assert!(condition.contains(AUTO_NAME_STARTED_OPTION));
        assert!(condition.contains("1000000000"));
        assert!(condition.contains(AUTO_NAME_TOKEN_OPTION));
        assert!(condition.contains("request-token"));
        assert!(mutation.contains(AUTO_NAME_STATUS_OPTION));
        assert!(mutation.contains("success"));
        assert!(mutation.contains(&format!("set-option -pu -t '%7' {AUTO_NAME_TOKEN_OPTION}")));
        assert!(mutation.contains(UNPIN_WAITING_OPTION));
        assert!(mutation.contains(UNPIN_WAITING_TITLE_OPTION));
    }

    #[test]
    fn mismatched_request_token_cannot_complete_a_forced_request() {
        let base = state(THREAD, "/work/project", "", "", "", "", "");
        let prefix = base.strip_suffix("\x1f\x1f\n").unwrap();
        let current = format!("{prefix}\x1f1\x1f\x1fgenerating\x1f3000000000\x1fnew-token\n");
        let runner = FakeRunner::with_states(&[&current]);

        assert!(
            OwnedTmuxNames::new(runner.clone())
                .reconcile(&names_with_token("Cached title", Some("old-token")))
        );

        assert_eq!(calls(&runner).len(), 1);
    }

    #[test]
    fn source_less_request_advances_after_exact_title_change() {
        let runner = FakeRunner::with_states(&[]);
        let pane = Pane {
            id: PaneId::new("%7").unwrap(),
            session_id: crate::domain::SessionId::new("$1").unwrap(),
            title: Some(THREAD.to_owned()),
            generated_title: None,
            generated_thread_id: None,
            generated_source_stable: false,
            generated_at_unix: None,
            immediate_naming: true,
            auto_name_status: Some(AutoNameStatus::RecoveringIdentity),
            auto_name_started_at_unix_nanos: Some(GENERATED_AT),
            auto_name_token: Some("request-token".to_owned()),
            manual_name: false,
            manual_name_source: None,
            manual_name_pid: None,
            manual_name_pid_raw: String::new(),
            manual_name_session: None,
            manual_name_session_raw: String::new(),
            unpin_waiting: true,
            unpin_waiting_title: Some("Pinned title".to_owned()),
            unpin_waiting_pid: Some(77),
            unpin_waiting_session: Some(crate::domain::SessionId::new("$1").unwrap()),
            pane_pid: 77,
            current_path: "/work/project".into(),
        };

        OwnedTmuxNames::new(runner.clone()).mark_auto_name_generating(std::slice::from_ref(&pane));

        let calls = calls(&runner);
        let (condition, mutation) = if_shell(&calls);
        assert!(condition.contains(IMMEDIATE_NAMING_OPTION));
        assert!(condition.contains(AUTO_NAME_STATUS_OPTION));
        assert!(condition.contains("recovering"));
        assert!(condition.contains(AUTO_NAME_STARTED_OPTION));
        assert!(condition.contains(&GENERATED_AT.to_string()));
        assert!(condition.contains(AUTO_NAME_TOKEN_OPTION));
        assert!(condition.contains("request-token"));
        assert!(mutation.contains("generating"));
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
