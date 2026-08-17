//! Conservative ownership and application of generated tmux window names.

use std::{collections::HashMap, ffi::OsString};

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
const STATE_FORMAT: &str = "#{pane_id}\x1f#{pane_title}\x1f#{window_name}\x1f#{automatic-rename}\x1f#{window_panes}\x1f#{@codex_mux_generated_thread}\x1f#{@codex_mux_generated_name}\x1f#{pane_current_path}";

/// Applies generated names only while codex-mux can prove it owns the visible name.
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
    pub fn reconcile(&self, names: &HashMap<PaneId, GeneratedName>) {
        if names.is_empty() {
            return;
        }
        let Ok(output) = self.run(["list-panes", "-a", "-F", STATE_FORMAT]) else {
            return;
        };
        let states = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let fields = line.split(SEPARATOR).collect::<Vec<_>>();
                (fields.len() == 8).then(|| (fields[0].to_owned(), fields[1..].join("\x1f")))
            })
            .collect::<HashMap<_, _>>();
        for (pane_id, generated) in names {
            if let Some(state) = states.get(pane_id.as_str()) {
                let _ = self.reconcile_one(pane_id, generated, state);
            }
        }
    }

    fn reconcile_one(
        &self,
        pane_id: &PaneId,
        generated: &GeneratedName,
        state: &str,
    ) -> Result<()> {
        let fields = state.split(SEPARATOR).collect::<Vec<_>>();
        if fields.len() != 7
            || fields[0] != generated.source_title
            || fields[3] != "1"
            || fields[6] != generated.source_cwd.to_string_lossy()
        {
            return Ok(());
        }
        let current_name = fields[1];
        let automatic = fields[2] == "1" || fields[2] == "on";
        let owner_thread = fields[4];
        let owner_name = fields[5];

        let condition = if !owner_thread.is_empty() || !owner_name.is_empty() {
            if owner_name != current_name {
                self.unset_marker(pane_id)?;
                return Ok(());
            }
            if current_name == generated.name && owner_thread == generated.thread_id {
                return Ok(());
            }
            format!(
                "#{{&&:#{{==:#{{pane_title}},#{{{SOURCE_TITLE_OPTION}}}}},#{{&&:#{{==:#{{pane_current_path}},#{{{SOURCE_CWD_OPTION}}}}},#{{&&:#{{==:#{{window_panes}},1}},#{{==:#{{window_name}},#{{@codex_mux_generated_name}}}}}}}}}}"
            )
        } else if !automatic {
            return Ok(());
        } else {
            format!(
                "#{{&&:#{{==:#{{pane_title}},#{{{SOURCE_TITLE_OPTION}}}}},#{{&&:#{{==:#{{pane_current_path}},#{{{SOURCE_CWD_OPTION}}}}},#{{&&:#{{==:#{{window_panes}},1}},#{{&&:#{{automatic-rename}},#{{&&:#{{==:#{{@codex_mux_generated_thread}},}},#{{==:#{{@codex_mux_generated_name}},}}}}}}}}}}}}"
            )
        };

        let mutation = format!(
            "rename-window -t {} {}; set-option -w -t {} {} {}; set-option -w -t {} {} {}",
            tmux_quote(pane_id.as_str()),
            tmux_quote(&generated.name),
            tmux_quote(pane_id.as_str()),
            THREAD_OPTION,
            tmux_quote(&generated.thread_id),
            tmux_quote(pane_id.as_str()),
            OWNER_OPTION,
            tmux_quote(&generated.name),
        );
        let source_cwd = generated.source_cwd.to_string_lossy();
        self.run_arguments(
            [
                "set-option",
                "-p",
                "-t",
                pane_id.as_str(),
                SOURCE_TITLE_OPTION,
                &generated.source_title,
                ";",
                "set-option",
                "-p",
                "-t",
                pane_id.as_str(),
                SOURCE_CWD_OPTION,
                source_cwd.as_ref(),
                ";",
                "if-shell",
                "-F",
                "-t",
                pane_id.as_str(),
                &condition,
                &mutation,
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
        )?;
        Ok(())
    }

    fn unset_marker(&self, pane_id: &PaneId) -> Result<()> {
        self.run_arguments(
            [
                "set-option",
                "-wu",
                "-t",
                pane_id.as_str(),
                THREAD_OPTION,
                ";",
                "set-option",
                "-wu",
                "-t",
                pane_id.as_str(),
                OWNER_OPTION,
                ";",
                "set-option",
                "-pu",
                "-t",
                pane_id.as_str(),
                SOURCE_TITLE_OPTION,
                ";",
                "set-option",
                "-pu",
                "-t",
                pane_id.as_str(),
                SOURCE_CWD_OPTION,
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
        )?;
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

fn tmux_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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
                source_title: THREAD.to_owned(),
                source_cwd: "/work/project".into(),
                name: name.to_owned(),
            },
        )])
    }

    fn calls(runner: &FakeRunner) -> Vec<Vec<String>> {
        runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| {
                call.iter()
                    .map(|arg| arg.to_string_lossy().into())
                    .collect()
            })
            .collect()
    }

    fn if_shell(calls: &[Vec<String>]) -> (&str, &str) {
        let batch = calls.get(1).expect("one batched reconciliation command");
        let index = batch
            .iter()
            .position(|argument| argument == "if-shell")
            .expect("an atomic conditional mutation");
        (&batch[index + 4], &batch[index + 5])
    }

    #[test]
    fn takes_ownership_with_one_server_side_conditional() {
        let state = format!("%7\x1f{THREAD}\x1fdefault\x1f1\x1f1\x1f\x1f\x1f/work/project\n");
        let runner = FakeRunner::with_states(&[&state]);
        OwnedTmuxNames::new(runner.clone()).reconcile(&names("Fast inventory"));

        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        let (_, mutation) = if_shell(&calls);
        assert!(mutation.contains("rename-window -t '%7' 'Fast inventory'"));
        assert!(mutation.contains(THREAD_OPTION));
        assert!(mutation.contains(OWNER_OPTION));
    }

    #[test]
    fn truncated_source_title_can_own_full_thread_marker() {
        let title = "12345678-1234-1234-1234-12345...";
        let cwd = "/work/#{hostile},path";
        let state = format!("%7\x1f{title}\x1fdefault\x1f1\x1f1\x1f\x1f\x1f{cwd}\n");
        let runner = FakeRunner::with_states(&[&state]);
        let mut generated = names("Resolved title");
        generated
            .get_mut(&PaneId::new("%7").unwrap())
            .unwrap()
            .source_title = title.to_owned();
        generated
            .get_mut(&PaneId::new("%7").unwrap())
            .unwrap()
            .source_cwd = cwd.into();

        OwnedTmuxNames::new(runner.clone()).reconcile(&generated);

        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        let (condition, mutation) = if_shell(&calls);
        assert!(condition.contains(SOURCE_TITLE_OPTION));
        assert!(!condition.contains(title));
        assert!(!condition.contains(cwd));
        assert!(calls[1].iter().any(|argument| argument == cwd));
        assert!(mutation.contains(THREAD));
    }

    #[test]
    fn updates_owned_value_with_live_marker_predicates() {
        let state = format!(
            "%7\x1f{THREAD}\x1fOld title\x1f0\x1f1\x1f{THREAD}\x1fOld title\x1f/work/project\n"
        );
        let runner = FakeRunner::with_states(&[&state]);
        OwnedTmuxNames::new(runner.clone()).reconcile(&names("New title"));

        let calls = calls(&runner);
        let (condition, mutation) = if_shell(&calls);
        assert!(condition.contains("#{window_name}"));
        assert!(condition.contains("#{@codex_mux_generated_name}"));
        assert!(mutation.contains("'New title'"));
    }

    #[test]
    fn manual_override_releases_ownership_without_renaming() {
        let state = format!(
            "%7\x1f{THREAD}\x1fMy manual title\x1f0\x1f1\x1f{THREAD}\x1fOld title\x1f/work/project\n"
        );
        let runner = FakeRunner::with_states(&[&state]);
        OwnedTmuxNames::new(runner.clone()).reconcile(&names("New title"));

        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        for option in [
            THREAD_OPTION,
            OWNER_OPTION,
            SOURCE_TITLE_OPTION,
            SOURCE_CWD_OPTION,
        ] {
            assert!(calls[1].iter().any(|argument| argument == option));
        }
    }

    #[test]
    fn stale_or_multi_pane_target_is_never_renamed() {
        for state in [
            "%7\x1faaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\x1fdefault\x1f1\x1f1\x1f\x1f\x1f/work/project\n",
            &format!("%7\x1f{THREAD}\x1fdefault\x1f1\x1f2\x1f\x1f\x1f/work/project\n"),
            &format!("%7\x1f{THREAD}\x1fdefault\x1f1\x1f1\x1f\x1f\x1f/work/other\n"),
        ] {
            let runner = FakeRunner::with_states(&[state]);
            OwnedTmuxNames::new(runner.clone()).reconcile(&names("Wrong target"));
            assert_eq!(calls(&runner).len(), 1);
        }
    }

    #[test]
    fn command_values_are_quoted_for_tmux_parser() {
        assert_eq!(tmux_quote("don't"), "'don'\\''t'");
    }

    #[test]
    fn unchanged_owned_name_can_transition_to_a_replacement_thread() {
        let old_thread = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let state = format!(
            "%7\x1f{THREAD}\x1fOld title\x1f0\x1f1\x1f{old_thread}\x1fOld title\x1f/work/project\n"
        );
        let runner = FakeRunner::with_states(&[&state]);
        OwnedTmuxNames::new(runner.clone()).reconcile(&names("Replacement thread"));

        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        let (_, mutation) = if_shell(&calls);
        assert!(mutation.contains("'Replacement thread'"));
        assert!(mutation.contains(THREAD));
    }

    #[test]
    fn same_title_replacement_thread_refreshes_ownership_marker() {
        let old_thread = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let state = format!(
            "%7\x1f{THREAD}\x1fSame title\x1f0\x1f1\x1f{old_thread}\x1fSame title\x1f/work/project\n"
        );
        let runner = FakeRunner::with_states(&[&state]);
        OwnedTmuxNames::new(runner.clone()).reconcile(&names("Same title"));

        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        assert!(if_shell(&calls).1.contains(THREAD));
    }
}
