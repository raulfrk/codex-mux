# Changelog

All notable changes are documented here. This project follows Semantic Versioning.

## [Unreleased]

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
