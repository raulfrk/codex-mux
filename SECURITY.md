# Security policy

## Supported versions

Until the first stable release, only the latest tagged release receives security fixes. Users should verify every downloaded archive against the `SHA256SUMS` file attached to the same GitHub release.

## Reporting a vulnerability

Once the repository is hosted on GitHub, report vulnerabilities through its private **Security → Advisories → New draft security advisory** flow. Do not include exploits, credentials, private tmux output, or Codex data in a public issue. Include the affected version, operating system, tmux version, reproduction steps, and the security impact. If private advisories are unavailable, open a minimal public issue asking the maintainer for a private reporting channel without disclosing vulnerability details.

## Security boundaries

`codex-mux` is a local tool operating with the current user's permissions. It trusts the tmux server selected by the inherited `TMUX` environment and the explicitly configured Codex executable. Anyone who can control those inputs or replace the installed binaries already acts within the user's local trust boundary.

The project deliberately:

- uses supported tmux commands for its core discovery and control path;
- does not read Codex private session files, credentials, or internal databases;
- discovers processes through Linux `/proc` and validates executable identity conservatively;
- passes launch and targeting values as argument vectors where possible, and validates plus quotes values at the required tmux command-parser or `run-shell` boundary;
- manages only its uniquely marked tmux configuration block and refuses symlinks, ambiguous entrypoints, non-regular files, and non-owner-writable files;
- requires a second fresh confirmation key before killing the selected pane;
- stores only a theme identifier, validated launch profiles, and the opt-in Smart Naming boolean in `${XDG_CONFIG_HOME:-$HOME/.config}/codex-mux/config.toml`, normally with mode `0600`; custom binaries must be absolute, regular executable files;
- has no Setforge runtime dependency and does not let Setforge manage tmux or theme configuration.

## Optional Smart Naming data flow

Conversation-aware Smart Naming is disabled by default and requires an explicit toggle in the popup configuration. When enabled, a tmux-owned local worker launches the configured `codex app-server`, requests bounded completed conversation content for the exact discovered thread, and submits that excerpt through Codex to the exact `gpt-5.6-luna` model for a structured short title. This is a model request: conversation content leaves the local process according to the user's Codex service configuration and may consume account allowance.

The worker bounds app-server frames and transcript input, ignores in-progress turns, validates and bounds model output, and rejects stale pane/thread results before publication. `codex-mux` does not write transcripts to disk and drops its local excerpt after the request; the worker retains only an in-memory fingerprint/title cache. Naming runs in an ephemeral app-server thread. Conversation handling outside the local codex-mux process remains governed by the user's Codex service configuration and policy. Tmux stores generated titles as pane-local metadata rendered only inside Codex Mux; it does not rename tmux windows or change their automatic-rename setting. Turning the feature off stops and joins the worker, then clears the pane-local metadata without requiring a tmux or Codex restart.

Generated titles are applied only when the exact pane still carries the expected thread and working directory. The final validation and metadata mutation run through tmux against the exact pane. Startup also repairs window names owned by legacy releases, but only when legacy window-scoped markers and their captured state still prove ownership; unrelated manual window names are left untouched. Provider startup, protocol drift, malformed responses, validation failures, and network/model failures leave existing names and core popup behavior available.

Tmux clients attached to the same session share that session's active window, and clients viewing the same window share its zoom state. Selecting or zooming through `codex-mux` can therefore be visible to those same-session clients. This is expected tmux behavior and should be considered when sharing a tmux server with another person.

Release archives are produced with normalized ordering, ownership, timestamps, and gzip metadata. The release workflow publishes a sorted SHA-256 manifest. Checksums detect corruption or substitution after the manifest was produced; users must obtain the manifest from the trusted GitHub release page for that verification to be meaningful.
