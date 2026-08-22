# Changelog

All notable changes are documented here. This project follows Semantic Versioning.

## [Unreleased]

### Added

- Add `Ctrl+R` in the Rename flow to request an immediate automatic name with guarded ownership release and recoverable progress, success, and timeout indicators.

### Fixed

- Retry a bounded, state-bracketed Smart Left composer snapshot when live Max/Ultra redraws move the cursor row during probing, while continuing to fail through on persistent instability.
- Stop Smart Naming promptly after disable, using a short cooperative grace period followed by PID/start-time-authenticated pidfd termination of only the owned daemon and its snapshotted descendants when the worker is blocked.
- Prefer durable project, component, or conversation-theme titles using recent chronological context plus a bounded privacy-safe activity digest from existing structured history; recognize common monorepo manifests and Git worktrees without trusting pane cwd or adding another model round.
- Add deterministic production-prompt evals across common repository layouts and an opt-in parallel live Luna eval corpus for naming accuracy, transient-topic rejection, formatting, and latency.

## [0.9.5] - 2026-08-21

### Fixed

- Apply and retain an authoritative recovered Smart Name when Codex continuously redraws an arbitrary spinner title, while fencing the ownership to the same tmux session, pane leader process, working directory, and manual-name state.
- Let Smart Left tolerate one bounded transient Ultra/Max redraw before its unchanged-state and exact-process recheck, while still forwarding a genuine cursor move.
- Target the exact invoking tmux window and insert after it when New or Resume launches a selected profile, avoiding both an occupied index and nested-shell expansion of tmux's `$`-prefixed session ID; keep the picker open with the exact tmux error if creation still fails.
- Create new Codex title-ownership state safely on NFSv3 filesystems that reject `renameat2(RENAME_NOREPLACE)` with `EINVAL`, using an atomic same-directory link fallback without overwriting an existing file.

## [0.9.4] - 2026-08-21

### Fixed

- Configure Codex's exact `thread-id` terminal title transactionally during setup, including outside-Mux sessions, and restore the user's prior setting on removal.
- Recover arbitrary legacy titles from the exact configured Codex process's open root rollout, including older clients delegated through their private kernel-verified app-server control-socket peer; descendant environment claims are never accepted as conversation identity.
- Name independent conversations across four bounded workers and use the most recent completed user/assistant messages, reducing multi-pane naming latency without changing provider cooldowns.

## [0.9.3] - 2026-08-21

### Fixed

- Request Codex's non-truncated `thread` terminal title for new Mux sessions, resolve legacy prefixes against verified rollout identity without requiring the original working directory, and fail closed on ambiguity.
- Read completed conversations through bounded `thread/turns/list` and `thread/items/list` pagination instead of the hanging monolithic `thread/read`, with a descriptor-pinned private rollout fallback for unloaded and externally started sessions.
- Tolerate malformed individual rollout records without logging conversation text, and retain privacy-safe diagnostics for the selected identity and retrieval path.

## [0.9.2] - 2026-08-21

### Fixed

- Resolve externally resumed, truncated Codex thread titles through bounded state and historical app-server listings, accepting only one combined exact UUID-prefix and working-directory match; Smart Naming logs fixed safe reason codes for the state miss and cross-check without recording conversation data.
- Let empty-Enter Manual Rename unpin every current pane. When no exact source survived the pin, it safely waits for Codex to publish a changed thread title instead of guessing a conversation.
- Recognize the post-prompt-glyph no-op composer boundary used by Ultra and Max layouts while retaining process verification and the unchanged-post-Left fence.

## [0.9.1] - 2026-08-20

### Fixed

- Let Manual Rename save sessions whose Codex-owned terminal title redraws while the popup is open; retain an unpin source only when that thread identity is still proven, and explain when unpin is unavailable.
- Recognize both no-op Codex composer boundaries used by Ultra/Max reasoning layouts while keeping Smart Left's exact process and post-Left safety fences.

## [0.9.0] - 2026-08-20

### Added

- Add persistent manual-name unpin from the capital-`R` prompt and bounded privacy-safe Smart Naming diagnostics in XDG state.

### Fixed

- Recognize indented Codex composer boundaries used by Ultra/Max reasoning modes without weakening the unchanged-state Smart Left fence.
- Let initial `c` clear the rename prefill and preserve the original thread identity so Smart Naming can resume immediately after unpin.

## [0.8.0] - 2026-08-20

### Added

- Press capital `R` in the session switcher to manually rename the selected pane. This permanently relinquishes Smart Naming ownership of that pane, so generated names cannot overwrite the manual title.

## [0.7.0] - 2026-08-20

### Changed

- Name eligible new and resumed Codex conversations as soon as completed content is available, and wake Smart Naming immediately after Resume without blocking the popup.
- Preserve exact thread identity and pane-local tmux metadata while applying the generated name to every matching pane across the tmux server.

## [0.6.0] - 2026-08-20

### Added

- Add shared foreground, pane-tree, and pane-TTY wrapper matching with command and pane-command regex configuration.

## [0.5.0] - 2026-08-20

### Added

- Add `codex-mux update [VERSION]` with official-release discovery, checksum verification, and atomic in-place replacement.

## [0.4.0] - 2026-08-20

### Added

- Separate Codex launch, exact wrapper-aware process matching, and Smart Left pane-command configuration while retaining `--codex PATH` compatibility.

## [0.2.1] - 2026-08-17

### Fixed

- Restore the Codex pane that was active when codex-mux was opened.

## [0.2.0] - 2026-08-16

### Added

- Optional marker-managed Smart Left activation opens the mux from the absolute beginning of a directly verified Codex composer while preserving ordinary Left behavior everywhere else.
- Prompt-aware Smart Left support for plain interactive Bash and Zsh primary prompts, with fail-closed handling for commands, reads, secondary prompts, wrappers, and uncertain process state.
- Zero-argument `codex-mux setup` and `codex-mux remove` commands transactionally manage owned tmux, Bash, and Zsh configuration blocks using safe standard defaults.

## [0.1.1] - 2026-08-16

### Fixed

- Build native GNU/Linux release binaries against glibc 2.31 so they run on supported Debian hosts instead of requiring Ubuntu 24.04's newer runtime.

## [0.1.0] - 2026-08-16

### Added

- Tmux-native, server-wide Codex pane discovery and exact pane actions.
- Responsive desktop, compact, phone, and tiny terminal layouts.
- New-session, resume-all, confirmed-close, and theme-picker controls.
- Marker-managed tmux install, status, and uninstall commands.
- Deterministic Linux release archives for x86_64 and aarch64 with SHA-256 manifests.
