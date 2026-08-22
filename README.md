# codex-mux

`codex-mux` is a tmux-native agent view for Codex. Press one tmux prefix binding to open a responsive popup, see Codex processes across the current tmux server, switch to one, start a new session, resume an existing thread, or close a pane deliberately.

Its core discovery and control path talks to public tmux commands and Linux `/proc`; it does not read Codex private session files or require a shell framework. The optional, default-off Smart Naming feature uses the local Codex app-server protocol and may read a bounded verified rollout as described below. Setforge may install a released binary in the future, but `codex-mux` has no Setforge runtime dependency and Setforge does not own its tmux or theme configuration.

## Quick start

With `codex` and `codex-mux` on `PATH`, configure the default `prefix + a` binding and prompt-aware Smart Left for tmux, Bash, and Zsh:

```sh
codex-mux setup
```

Open a new Bash/Zsh shell (or source its startup file) after setup. To remove every codex-mux-owned tmux and shell block without deleting any host configuration file:

```sh
codex-mux remove
```

The zero-argument commands discover one safe tmux entrypoint and use `$HOME/.bashrc` plus `${ZDOTDIR:-$HOME}/.zshrc`. Use `--tmux-config`, `--bash-config`, or `--zsh-config` when those locations differ, and global `--codex /absolute/path` when Codex is not discoverable on `PATH`.

### Launcher wrappers

When the command used to start Codex differs from the foreground process that tmux exposes, configure the identities separately:

```toml
[process]
launch_executable = "/opt/codex/bin/codex-launcher"
match_executables = [
  "/opt/codex/bin/codex-launcher",
  "/opt/codex/runtime/codex",
]
pane_commands = ["codex"]
# Defaults to "foreground" for existing configurations.
match_scope = "pane-tree"
match_command_regexes = ['(^|/)codex-launcher(\s|$)']
pane_command_regexes = ['^supervisor(-[a-z0-9]+)?$']
```

The table belongs in `${XDG_CONFIG_HOME:-$HOME/.config}/codex-mux/config.toml`. Every path must be absolute and executable. `launch_executable` alone starts new sessions and the Smart Naming app-server; matching an underlying binary never changes what is launched. `foreground` preserves the original foreground-process-group behavior, `pane-tree` searches readable descendants of the pane process, and `pane-tty` searches readable processes attached to tmux's exact pane TTY. Inaccessible or racing `/proc` entries are skipped.

Exact native executable and interpreted-script identities remain preferred. `match_command_regexes` are a controlled fallback for versioned or generated launchers: each readable argv argument must be valid UTF-8, arguments are joined with one ASCII space, and regexes operate on that normalized full command only. No shell parsing, expansion, or quoting occurs. If any argument is invalid UTF-8, only exact identity matching is considered. Codex Mux never logs matched command lines because arguments can contain secrets. Smart Left uses this same matcher after its exact-or-regex `pane_current_command` prefilter.

The equivalent one-shot overrides are `--launch-executable PATH`, repeatable `--match-executable PATH`, `--match-scope foreground|pane-tree|pane-tty`, repeatable `--match-command-regex REGEX`, repeatable `--pane-command COMMAND`, and repeatable `--pane-command-regex REGEX`. `--codex PATH` remains the compatible shorthand for all three legacy values: launch and match use `PATH`, while the pane command is derived from its file name.

## Requirements

- Linux on `x86_64` or `aarch64`
- tmux 3.2 or newer with popup support
- Codex running inside tmux panes
- A terminal with UTF-8 support; colors are optional

## Install a release

Each tagged release contains these assets:

```text
codex-mux-VERSION-x86_64-unknown-linux-gnu.tar.gz
codex-mux-VERSION-aarch64-unknown-linux-gnu.tar.gz
SHA256SUMS
```

Download the archive for `uname -m` and `SHA256SUMS` from the same GitHub release. Verify before extracting:

```sh
sha256sum -c SHA256SUMS --ignore-missing
tar -xzf codex-mux-VERSION-TARGET.tar.gz
install -m 0755 codex-mux-VERSION-TARGET/codex-mux "$HOME/.local/bin/codex-mux"
```

The checksum command must report the selected archive as `OK`. Stop if it fails. Releases currently target glibc-based Linux.

Once Codex Mux is installed, update to the latest stable release with:

```sh
codex-mux update
```

Select an exact stable release, including an intentional reinstall or downgrade, with `codex-mux update VERSION`. The command accepts `0.5.0` or `v0.5.0`, downloads only the matching official GitHub release assets, verifies `SHA256SUMS`, and atomically replaces the currently running executable. Existing Codex Mux processes continue running their prior binary; later invocations use the replacement. The updater does not invoke `sudo`, so the containing directory must be writable by the current user.

Building from source requires Rust 1.85 or newer:

```sh
cargo build --locked --release
install -m 0755 target/release/codex-mux "$HOME/.local/bin/codex-mux"
```

## Add the tmux binding

Create the tmux configuration file yourself if it does not exist. Then install the default `prefix + a` binding using explicit absolute paths:

```sh
codex_path=$(command -v codex)
codex-mux --codex "$codex_path" tmux install --key a --config "$HOME/.tmux.conf"
```

For an XDG tmux entrypoint, use `--config "$HOME/.config/tmux/tmux.conf"`. An explicit path is recommended because it is predictable. Without `--config`, `codex-mux` proceeds only when it can identify exactly one safe, writable, regular user configuration from the running server or standard locations. It refuses symlinks, ambiguous choices, and unsafe files.

Only the block between `# >>> codex-mux >>>` and `# <<< codex-mux <<<` is managed. The installer preserves everything outside it, creates a backup on first install, updates idempotently, and reloads the file only when the active tmux server actually loaded that entrypoint.

If Codex is installed at a custom path, always use the same absolute path when installing or checking status:

```sh
/home/me/.local/bin/codex-mux \
  --codex /opt/codex/bin/codex \
  tmux install --key g --config /home/me/.tmux.conf
```

Re-run `tmux install` after moving either executable so the managed block records the new paths.

### Smart Left activation

Add `--smart-left` to the install command to make plain `Left` open the popup when the cursor is already at the absolute beginning of the focused Codex composer:

```sh
codex-mux --codex "$(command -v codex)" \
  tmux install --smart-left --key a --config "$HOME/.tmux.conf"
```

Smart Left verifies the exact foreground process and rendered boundary, sends the requested `Left` once, then confirms that the exact pane and cursor did not change. The Codex composer path has no fixed sampling delay. Bash and Zsh retain a short guarded settle because Readline/ZLE can redraw after tmux returns; they open only when `Left` leaves the cursor unchanged while the prompt lifecycle hook marks the pane as waiting at its primary prompt. During command execution, nested interactive programs, shell editing away from the boundary, and every uncertain state, the key remains ordinary `Left`.

`codex-mux setup` installs Smart Left and marker-managed prompt hooks by default. Bash prepends one `PROMPT_COMMAND` entry and wraps `PS0` and `PS2`; when Bash's `promptvars` option is disabled, shell Smart Left fails closed and ordinary `Left` remains active. Zsh installs `precmd`/`preexec` plus `line-init`, `line-pre-redraw`, and `line-finish` ZLE hook widgets. No shell framework is required, and existing hook chains are preserved.

The process check additionally requires the foreground Bash or Zsh executable to be the same file as `bash` or `zsh` resolved on the tmux server's `PATH`. Wrapper-launched shells and custom shell binaries unavailable on that `PATH` deliberately retain ordinary `Left` and `prefix + a` as the fallback. Setup is transactional for managed configuration bytes and live tmux state; collision-safe safety backups created before a later failure may remain for manual inspection.

This option owns the root-table `Left` binding inside the same marker block. Installation refuses to enable it if the selected config or running tmux server already binds root-table `Left`. Reinstall without `--smart-left` to disable it. The normal prefix binding remains installed either way.

## Use the popup

Press your normal tmux prefix, then the configured key. For example, with tmux's default prefix and the default `codex-mux` key, press `Ctrl-b`, release it, then press `a`.

| Key | Action |
| --- | --- |
| `j` / `Down`, `k` / `Up` | Move selection |
| `Enter` | Open the selected Codex pane and make its window full-screen |
| `n` | Open the launch-profile picker for a new Codex session |
| profile key (default `s` or `y`) | After `n` or `r`, immediately use that profile; `s` is standard and `y` adds Codex's `--yolo` flag |
| `j` / `Down`, `k` / `Up`, then `Enter` in profile picker | Choose and start a profile |
| `a` / `e` in profile picker | Add a profile or edit the selected profile |
| `r` | Open the launch-profile picker, then start `codex resume --all` with the chosen profile and the same directory rule |
| `R` | Edit the selected pane's manual title; initial `c` clears the prefill, empty `Enter` unpins, and `Ctrl+R` requests an immediate automatic name |
| `x` | Ask to close the selected pane |
| second fresh `x` or `Enter` | Confirm close |
| `q` or `Esc` during confirmation | Cancel close |
| `t` | Open and cycle the theme picker |
| `c` | Open configuration |
| `n` in configuration | Toggle conversation-aware Smart Naming |
| `Enter` in theme picker | Save the previewed theme |
| `q` or `Esc` in theme picker | Revert the preview |
| `q` or `Esc` | Close the popup without changing tmux state |

Discovery is server-wide, not limited to the current session or window. A pane is included only when its foreground process matches the configured Codex executable. The visible row uses the supported pane title and current path; internal tmux IDs are retained only for exact targeting.

### tmux sharing semantics

tmux makes the active window and zoom state properties of a session/window, not private properties of a client. `codex-mux` targets the invoking client explicitly, but selecting or zooming a pane in a session can be visible to other clients attached to that same tmux session. Clients attached to other sessions are not switched. This is standard tmux behavior, not a separate synchronization feature.

### Mobile and narrow terminals

When the invoking client is narrower than 90 columns or shorter than 28 rows, the installer opens a `100%` by `100%` popup with the compact layout. Larger clients use an `80%` by `70%` popup. The layout is calculated from the invoking client, so phone SSH sessions do not inherit a desktop client's geometry.

### Themes and color

The built-in themes are adaptive cyan, blue command palette, amber operator, ember orange, and monochrome. The profile picker and editor use the active theme too. The saved theme and launch profiles live at `${XDG_CONFIG_HOME:-$HOME/.config}/codex-mux/config.toml` with user-only file permissions. Existing theme-only files remain valid and receive the default Standard (`s`) and YOLO (`y`) profiles. A profile may override Codex with an absolute executable path; otherwise it uses the configured Codex binary. Setting a non-empty `NO_COLOR` uses monochrome for that invocation without overwriting the saved preference.

### Conversation-aware Smart Naming

Smart Naming is off by default. Open configuration with `c`, then press `n` to enable or disable it. The preference is saved atomically as `smart_naming` in the same config file; older files without that field remain valid and stay off. Enabling or disabling takes effect without restarting tmux, Codex sessions, or the popup. While shutdown is finishing, configuration shows `STOPPING` and prevents a second toggle.

When enabled, four bounded tmux-owned background lanes discover eligible new, resumed, and already-running Codex threads across the tmux server. Independent conversations are named concurrently, while panes for the same exact UUID stay in one lane for deduplication and server-wide fanout. An eligible pane is named as soon as its thread has a completed, non-empty conversation; the Resume action wakes this work in the background as soon as its exact new pane is created. An existing generated title is reconsidered at most once every thirty minutes, with no conversation read or model request between deadlines. Setup transactionally configures Codex's global `tui.terminal_title=["thread-id"]`, so sessions started inside or outside Mux expose their exact UUID; removal restores the prior value and status reports drift. New Mux launches also pass that exact supported setting directly. For a pre-instrumentation arbitrary title, the worker inspects only exact configured Codex processes in the pane tree and reads root `session_meta` from their already-open rollout descriptor. Older clients that delegate state are followed through the kernel's Unix-socket peer identity only when the peer owns Codex's private control socket; subagent rollouts and ambiguous roots are rejected. This works through launch wrappers when the underlying Codex binary is included in `match_executables`. Child-controlled environment claims are never accepted as conversation identity, and no prompt or process-memory inspection is used. For a legacy truncated UUID title, the worker cross-checks bounded local rollout metadata and app-server listings and accepts only one verified exact UUID-prefix match. It never uses a recent-session or same-directory guess.

When due, each lane reads bounded completed turns and items newest-first through the experimental paginated app-server APIs and retains only the most recent completed user/assistant excerpt within the byte limit. If that API cannot read an unloaded or externally started thread, it falls back to the exact verified local rollout file and likewise retains the newest bounded messages, tolerating malformed individual records and accepting only supported user and assistant items. Rollout traversal is descriptor-relative, rejects symlinked or group/world-writable trees, and parses identity and content from one verified open file. The local fallback discovers `${CODEX_HOME:-$HOME/.codex}/sessions` from the mux daemon environment; a launcher that changes `CODEX_HOME` only for its child still uses the launched app-server as its authoritative path, and should export the same value before starting the mux daemon if local fallback is required. The excerpt is sent through Codex to the exact `gpt-5.6-luna` model for a short structured title. Naming remains asynchronous, so popup discovery and input do not wait for the model. Four slow independent generations can overlap and normally complete in one provider round (the 10–15 second target depends on service latency); provider failures retain the existing one-hour cooldown and do not create retry amplification. This can use your Codex account's model allowance and has the same network/data handling implications as other Codex model requests.

`codex-mux` keeps only a bounded conversation excerpt in worker memory for the request and does not write prompts or transcripts to disk. It stores the generated thread, title, source identity, and last-generation timestamp as pane-local tmux metadata so the Codex Mux entry survives a worker restart. This metadata is consumed only when rendering Codex Mux and never changes the tmux window name or its `automatic-rename` setting. Pressing capital `R` saves a user title, clears generated-title metadata, and writes a pane-local manual-ownership marker plus the original thread title; Smart Naming therefore relinquishes that pane across restarts. In the same prompt, press `c` before editing to clear the prefilled title, then press `Enter` while empty to remove the override. Press `Ctrl+R` to explicitly relinquish a manual override and request an immediate automatic name. The modal and session row show animated `recovering identity`, `queued`, and `generating` stages plus success or a bounded timeout; closing the modal does not cancel tmux-owned work, and reopening it recovers the same status. When a retained exact source is still safe, Codex Mux restores it and requests immediate Smart Naming. Otherwise it releases the manual ownership and waits for Codex to expose a different exact thread title; it never guesses from a recent or same-directory conversation. Disabling joins the worker and removes generated-title metadata while retaining user-owned titles and their ownership markers.

Legacy titles created by older Smart Naming builds are migrated out of tmux window names when their ownership signature can be proven: normal tmux automatic naming is restored, while a still-matching title is retained only as Codex Mux pane metadata. If an eligible thread has no completed conversation content yet, the worker retries promptly without contacting the naming model. If app-server startup, protocol compatibility, model generation, or validation fails, existing panes and names continue working; failed refreshes are rate-limited and an unhealthy provider is restarted after its cooldown while the current/fallback entry title remains unchanged.

Privacy-safe Smart Naming reason codes are written to `${XDG_STATE_HOME:-$HOME/.local/state}/codex-mux/smart-naming.log`, rotated at 256 KiB, and summarized by `codex-mux tmux status`. Prompts, transcripts, titles, paths, thread IDs, raw command lines, app-server JSON, and stderr bodies are never logged.

Smart Left writes the same fixed-code-only diagnostics to the adjacent `smart-left.log`. It records which safety boundary rejected a gesture (tmux state, pane command, process identity, composer shape, or post-key recheck) without storing captured pane text, process arguments, paths, or identifiers.

## Inspect or remove configuration

The preferred removal command removes the owned tmux, Bash, and Zsh blocks together:

```sh
codex-mux remove
```

It preserves all bytes outside the marker blocks and never deletes the host files. It also restores the Codex terminal-title value captured by setup; if that managed value drifted, removal refuses to overwrite the user's newer setting. Existing shells may retain their already-loaded hook functions until restarted, but removing the tmux root binding disables interception immediately.

The lower-level tmux-only commands remain available:

Use the same executable paths and tmux entrypoint used during installation:

```sh
codex-mux --codex "$(command -v codex)" tmux status --config "$HOME/.tmux.conf"
codex-mux tmux uninstall --config "$HOME/.tmux.conf"
```

`status` is read-only and reports the configured key, Smart Left state, launch executable, every match executable and pane command, Codex exact-title ownership, and drift. `tmux uninstall` removes only the marked tmux block and unbinds the recorded prefix key and owned Smart Left key from a running server; it does not delete the host tmux file or theme preference. Use the top-level `remove` command to also restore the captured Codex terminal-title value and remove shell integration.

## Troubleshooting

- **No panes appear:** confirm Codex is running in tmux and list the exact absolute launcher script and/or underlying executable in `match_executables`. Interpreted scripts must remain visible as an exact command-line path in `/proc`.
- **Configuration discovery is ambiguous:** pass `--config` with the exact host-owned tmux entrypoint. `codex-mux` intentionally does not guess among multiple files.
- **Status reports drift:** re-run `tmux install` with the intended `--codex`, binary location, key, and config path.
- **The key does nothing:** run `tmux list-keys -T prefix KEY`, inspect `tmux status`, and verify the configured binary still exists and is executable.
- **Smart Left stays an ordinary Left:** run `tmux list-keys -T root Left`, confirm `tmux status` reports `smart-left: enabled`, and verify both the exact process path and `pane_current_command` are configured. Reasoning modes may indent or redraw the composer; Smart Left takes a bounded state-bracketed snapshot before delivering Left and still requires an unchanged post-Left pane state. The feature intentionally fails through when process or cursor identity remains uncertain.
- **A phone shows a large layout:** make sure the binding was invoked by that phone's tmux client; the popup uses `client_width` and `client_height` from the invoking client.
- **Colors are unreadable:** set `NO_COLOR=1` or select the monochrome theme.
- **Another client saw the selected window change:** clients sharing one tmux session also share that session's active window and window zoom state. Attach the clients to separate sessions when independent views are required.
- **Smart Naming stays off or a title does not update:** open configuration with `c` and confirm it shows `ON`, then inspect `smart-naming-last` and the sanitized log path from `codex-mux tmux status`. Completed conversation content is required. Provider or protocol failures leave the existing title unchanged and retry in the background. A title saved through capital `R` remains pinned until it is cleared and unpinned from the same prompt.
- **A command fails:** the popup restores the terminal before printing `codex-mux: ...` to standard error. No fallback action is attempted after an exact tmux command fails.

## Security and privacy

See [SECURITY.md](SECURITY.md). In short, configuration values are validated, the installer refuses unsafe config paths, and close/switch/name actions use exact tmux targets. Core operation does not access conversation data. The opt-in Smart Naming worker reads a bounded completed transcript through the local Codex app server, with a bounded owned rollout-file fallback, and sends it to GPT-5.6 Luna in an ephemeral naming thread. Codex service handling follows the user's service configuration and policy; codex-mux itself does not persist the transcript or read credentials.

## Verify from source

The ordinary Rust checks exercise unit and integration contracts. The packaged matrix additionally builds an optimized archive, verifies its checksum, extracts it, and drives that exact binary through isolated tmux servers and pseudo-terminals inside a read-only-root bubblewrap sandbox:

```sh
cargo +1.85 fmt --all -- --check
cargo +1.85 clippy --locked --all-targets --all-features -- -D warnings
cargo +1.85 test --locked --all-features
tests/release_packaging.sh
scripts/e2e.sh
```

The packaged matrix requires `bwrap`, `tmux`, util-linux `script`, GNU tar/gzip, and Rust 1.85. It fails when a prerequisite is absent and asserts that disposable tmux sockets and scratch roots are removed.

## License

MIT. See [LICENSE](LICENSE).
