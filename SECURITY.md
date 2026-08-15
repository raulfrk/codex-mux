# Security policy

## Supported versions

Until the first stable release, only the latest tagged release receives security fixes. Users should verify every downloaded archive against the `SHA256SUMS` file attached to the same GitHub release.

## Reporting a vulnerability

Once the repository is hosted on GitHub, report vulnerabilities through its private **Security → Advisories → New draft security advisory** flow. Do not include exploits, credentials, private tmux output, or Codex data in a public issue. Include the affected version, operating system, tmux version, reproduction steps, and the security impact. If private advisories are unavailable, open a minimal public issue asking the maintainer for a private reporting channel without disclosing vulnerability details.

## Security boundaries

`codex-mux` is a local tool operating with the current user's permissions. It trusts the tmux server selected by the inherited `TMUX` environment and the explicitly configured Codex executable. Anyone who can control those inputs or replace the installed binaries already acts within the user's local trust boundary.

The project deliberately:

- uses supported tmux commands instead of the Codex app server;
- does not read Codex private session files, prompts, transcripts, credentials, or internal databases;
- discovers processes through Linux `/proc` and validates executable identity conservatively;
- passes launch and targeting values as argument vectors rather than interpolating them through a shell;
- manages only its uniquely marked tmux configuration block and refuses symlinks, ambiguous entrypoints, non-regular files, and non-owner-writable files;
- requires a second fresh confirmation key before killing the selected pane;
- stores only a theme identifier in `${XDG_CONFIG_HOME:-$HOME/.config}/codex-mux/config.toml`, normally with mode `0600`;
- has no Setforge runtime dependency and does not let Setforge manage tmux or theme configuration.

Tmux clients attached to the same session share that session's active window, and clients viewing the same window share its zoom state. Selecting or zooming through `codex-mux` can therefore be visible to those same-session clients. This is expected tmux behavior and should be considered when sharing a tmux server with another person.

Release archives are produced with normalized ordering, ownership, timestamps, and gzip metadata. The release workflow publishes a sorted SHA-256 manifest. Checksums detect corruption or substitution after the manifest was produced; users must obtain the manifest from the trusted GitHub release page for that verification to be meaningful.
