# Responsiveness contract

`codex-mux` treats latency as a product contract. Environment-sensitive wall-clock
measurements are reported as distributions; deterministic tests gate command counts,
work complexity, stale-result handling, and whether slow work blocks input.

## User-visible budgets

Measure optimized release builds on an otherwise idle local tmux server. Record at
least 30 samples and report p50, p95, maximum, fixture size, tmux version, process
count, and CPU architecture. These budgets include codex-mux and tmux work but exclude
the time required for a newly launched Codex process to become ready.

| Interaction | Small fixture p95 | Stress fixture p95 |
| --- | ---: | ---: |
| Prefix binding to useful first popup frame | 100 ms | 200 ms |
| Smart Left at a confirmed boundary to useful first frame | 100 ms | 200 ms |
| Smart Left ordinary-Left forwarding | 50 ms | 75 ms |
| Navigation/config/theme key to rendered frame | 16 ms | 16 ms |
| Inventory publication stall on input/rendering | 16 ms | 16 ms |
| Switch, zoom, or close dispatch | 100 ms | 150 ms |

The small fixture has 3 tmux panes and 200 host processes. The stress fixture has
64 panes and 2,000 host processes, with at least 32 matching Codex panes spread over
multiple sessions. Fixture process evidence includes disappearing PIDs, inaccessible
entries, wrappers, duplicate executable basenames, and PID-reuse snapshots.

## Deterministic gates

- One inventory refresh performs one `tmux list-panes` request and work proportional
  to processes plus panes, never a full process scan per pane.
- At most one refresh and one naming request per unchanged conversation may be in
  flight. Slow refresh/model work never runs on the terminal input/render thread.
- Smart Left retains its 30 ms guarded observation, forwards Left exactly once, and
  reduces tmux client launches from the recorded baseline without removing any
  fail-through check.
- Switch/zoom/launch/close paths reduce tmux client launches from their recorded
  baselines while retaining exact targets and equivalent partial-failure reporting.
- Environment-sensitive timing does not fail CI. CI gates the injectable clock,
  command counts, complexity counters, and deterministic slow-boundary fixtures.

## Baseline evidence

Before the optimization wave, the interactive path has these structural baselines:

- Startup calls inventory synchronously before terminal entry and first draw.
- Idle refresh calls inventory synchronously after each one-second poll timeout.
- Inventory asks the process inspector once per pane; a nontrivial Linux lookup scans
  all of `/proc`, producing approximately panes × processes work.
- A successful Smart Left boundary path launches separate tmux clients for the
  initial state, Left forwarding, six guarded observations, screen capture, client
  dimensions, and popup display.
- Switching an unzoomed pane launches five tmux clients: zoom query, window select,
  pane select/zoom, exact-client switch, and the conditional resize toggle.

The initial checked-in harness records inventory and action baselines through shared
counters and exercises both documented fixture sizes. Later optimization leaves
tighten those assertions to target command counts, add the Linux process-snapshot
complexity gate, and add slow-boundary and stale-publication coverage. For local
timing, capture the exact release binary and tmux fixture used, warm it once, then
collect the distribution rather than reporting a single best run.
