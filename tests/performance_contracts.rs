use std::{cell::Cell, ffi::OsString, fmt::Write, path::PathBuf, rc::Rc};

use codex_mux::{
    Result,
    domain::{
        CodexExecutable, CommandOutput, InvocationContext, Pane, PaneId, ProcessInspector,
        SessionId, TmuxCommandRunner,
    },
    tmux::{actions::TmuxActions, inventory::PaneInventory},
};

#[derive(Default)]
struct CountingRunner {
    calls: Rc<Cell<usize>>,
    output: Vec<u8>,
}

impl CountingRunner {
    fn returning(output: impl Into<Vec<u8>>) -> Self {
        Self {
            calls: Rc::new(Cell::new(0)),
            output: output.into(),
        }
    }
}

impl TmuxCommandRunner for CountingRunner {
    fn run(&self, _arguments: &[OsString]) -> Result<CommandOutput> {
        self.calls.set(self.calls.get() + 1);
        Ok(CommandOutput {
            stdout: self.output.clone(),
            stderr: Vec::new(),
            status: Some(0),
        })
    }
}

struct CountingInspector {
    pane_calls: Rc<Cell<usize>>,
    batch_calls: Rc<Cell<usize>>,
    process_visits: Rc<Cell<usize>>,
    fixture_processes: usize,
    executable: PathBuf,
}

impl ProcessInspector for CountingInspector {
    fn foreground_executable(&self, _pane_pid: u32) -> Result<Option<PathBuf>> {
        self.pane_calls.set(self.pane_calls.get() + 1);
        Ok(Some(self.executable.clone()))
    }

    fn foreground_executables(&self, pane_pids: &[u32]) -> Vec<Result<Option<PathBuf>>> {
        self.batch_calls.set(self.batch_calls.get() + 1);
        self.process_visits.set(
            self.process_visits
                .get()
                .saturating_add(self.fixture_processes),
        );
        pane_pids
            .iter()
            .map(|pane_pid| self.foreground_executable(*pane_pid))
            .collect()
    }
}

fn inventory_fixture(panes: usize, processes: usize) {
    let executable = PathBuf::from("/opt/codex/bin/codex");
    let mut rows = String::new();
    for index in 1..=panes {
        writeln!(
            rows,
            "%{index}\x1f${index}\x1f@{index}\x1fwindow\x1fthread-{index}\x1f/work/project-{index}\x1fcodex\x1f{}\x1f/dev/pts/{index}\x1f\x1f",
            index + 100
        )
        .unwrap();
    }
    let runner = CountingRunner::returning(rows.into_bytes());
    let runner_calls = Rc::clone(&runner.calls);
    let inspector = CountingInspector {
        pane_calls: Rc::new(Cell::new(0)),
        batch_calls: Rc::new(Cell::new(0)),
        process_visits: Rc::new(Cell::new(0)),
        fixture_processes: processes,
        executable: executable.clone(),
    };
    let pane_calls = Rc::clone(&inspector.pane_calls);
    let batch_calls = Rc::clone(&inspector.batch_calls);
    let process_visits = Rc::clone(&inspector.process_visits);
    let inventory =
        PaneInventory::new(runner, inspector, CodexExecutable::new(executable).unwrap());

    assert_eq!(inventory.discover().unwrap().len(), panes);
    assert_eq!(runner_calls.get(), 1, "one coherent tmux pane snapshot");
    assert_eq!(batch_calls.get(), 1, "one process-resolution batch");
    assert_eq!(pane_calls.get(), panes, "one resolution per pane");
    assert_eq!(process_visits.get(), processes, "one process snapshot");
}

#[test]
fn small_inventory_fixture_exposes_command_and_complexity_counts() {
    inventory_fixture(3, 200);
}

#[test]
fn stress_inventory_fixture_exposes_command_and_complexity_counts() {
    inventory_fixture(64, 2_000);
}

#[test]
fn inventory_rejects_a_malformed_batch_cardinality() {
    struct ShortInspector;
    impl ProcessInspector for ShortInspector {
        fn foreground_executable(&self, _pane_pid: u32) -> Result<Option<PathBuf>> {
            unreachable!("the malformed batch override is the exercised boundary")
        }

        fn foreground_executables(&self, _pane_pids: &[u32]) -> Vec<Result<Option<PathBuf>>> {
            Vec::new()
        }
    }

    let runner = CountingRunner::returning(
        b"%1\x1f$1\x1f@1\x1fwindow\x1fthread\x1f/work/project\x1fcodex\x1f101\x1f/dev/pts/1\x1f\x1f\n"
            .to_vec(),
    );
    let inventory = PaneInventory::new(
        runner,
        ShortInspector,
        CodexExecutable::new("/opt/codex/bin/codex").unwrap(),
    );

    let error = inventory.discover().unwrap_err().to_string();
    assert!(error.contains("0 results for 1 panes"), "{error}");
}

#[test]
fn unzoomed_switch_baseline_is_five_exact_tmux_requests() {
    struct SwitchRunner(Cell<usize>);
    impl TmuxCommandRunner for SwitchRunner {
        fn run(&self, _arguments: &[OsString]) -> Result<CommandOutput> {
            let call = self.0.get();
            self.0.set(call + 1);
            Ok(CommandOutput {
                stdout: if call == 0 {
                    b"0\n".to_vec()
                } else {
                    Vec::new()
                },
                stderr: Vec::new(),
                status: Some(0),
            })
        }
    }

    let runner = SwitchRunner(Cell::new(0));
    let executable = CodexExecutable::new("/opt/codex/bin/codex").unwrap();
    let actions = TmuxActions::new(&runner, &executable);
    let context = InvocationContext {
        client_id: codex_mux::domain::ClientId::new("/dev/pts/7").unwrap(),
        pane_id: PaneId::new("%1").unwrap(),
        session_id: SessionId::new("$1").unwrap(),
        current_path: PathBuf::from("/work"),
    };
    let pane = Pane {
        id: PaneId::new("%2").unwrap(),
        session_id: SessionId::new("$2").unwrap(),
        title: Some("target".to_owned()),
        generated_title: None,
        current_path: PathBuf::from("/work/target"),
    };

    actions.switch_and_zoom(&context, &pane).unwrap();
    assert_eq!(runner.0.get(), 5, "record the pre-optimization baseline");
}
