# codex-mux

`codex-mux` is a tmux-native agent view for Codex. Press one tmux prefix binding to open a responsive popup, see Codex processes across the current tmux server, switch to one, start a new session, resume an existing thread, or close a pane deliberately.

It talks to public tmux commands and Linux `/proc`. It does not use the Codex app server, read Codex private session files, or require a shell framework. Setforge may install a released binary in the future, but `codex-mux` has no Setforge runtime dependency and Setforge does not own its tmux or theme configuration.

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

The checksum command must report the selected archive as `OK`. Stop if it fails. Releases currently target glibc-based Linux; building from source requires Rust 1.85 or newer:

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

Smart Left first sends the requested `Left`, then observes the rendered cursor over a 30 ms sampling window. In Codex it opens only when the cursor did not move and is on the composer prompt. In Bash or Zsh it opens only when the shell's prompt lifecycle hook marks the pane as waiting at its primary prompt and Left cannot move the cursor. During command execution, nested interactive programs, shell editing away from the boundary, and every uncertain state, the key remains ordinary `Left`.

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
| profile key (default `s` or `y`) | Immediately start that profile; `s` is standard and `y` adds Codex's `--yolo` flag |
| `j` / `Down`, `k` / `Up`, then `Enter` in profile picker | Choose and start a profile |
| `a` / `e` in profile picker | Add a profile or edit the selected profile |
| `r` | Start `codex resume --all` with the same directory rule |
| `x` | Ask to close the selected pane |
| second fresh `x` or `Enter` | Confirm close |
| `q` or `Esc` during confirmation | Cancel close |
| `t` | Open and cycle the theme picker |
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

## Inspect or remove configuration

The preferred removal command removes the owned tmux, Bash, and Zsh blocks together:

```sh
codex-mux remove
```

It preserves all bytes outside the marker blocks and never deletes the host files. Existing shells may retain their already-loaded hook functions until restarted, but removing the tmux root binding disables interception immediately.

The lower-level tmux-only commands remain available:

Use the same executable paths and tmux entrypoint used during installation:

```sh
codex-mux --codex "$(command -v codex)" tmux status --config "$HOME/.tmux.conf"
codex-mux tmux uninstall --config "$HOME/.tmux.conf"
```

`status` is read-only and reports the configured key, Smart Left state, both executable paths, and drift. `uninstall` removes only the marked block and unbinds the recorded prefix key and owned Smart Left key from a running server; it does not delete the host tmux file or theme preference.

## Troubleshooting

- **No panes appear:** confirm Codex is running in tmux, and pass the exact absolute Codex executable used by those processes. Wrapper scripts must retain an identity that can be verified through `/proc`.
- **Configuration discovery is ambiguous:** pass `--config` with the exact host-owned tmux entrypoint. `codex-mux` intentionally does not guess among multiple files.
- **Status reports drift:** re-run `tmux install` with the intended `--codex`, binary location, key, and config path.
- **The key does nothing:** run `tmux list-keys -T prefix KEY`, inspect `tmux status`, and verify the configured binary still exists and is executable.
- **Smart Left stays an ordinary Left:** run `tmux list-keys -T root Left`, confirm `tmux status` reports `smart-left: enabled`, and use the directly configured Codex executable rather than a wrapper. The feature intentionally fails through when process or cursor identity is uncertain.
- **A phone shows a large layout:** make sure the binding was invoked by that phone's tmux client; the popup uses `client_width` and `client_height` from the invoking client.
- **Colors are unreadable:** set `NO_COLOR=1` or select the monochrome theme.
- **Another client saw the selected window change:** clients sharing one tmux session also share that session's active window and window zoom state. Attach the clients to separate sessions when independent views are required.
- **A command fails:** the popup restores the terminal before printing `codex-mux: ...` to standard error. No fallback action is attempted after an exact tmux command fails.

## Security and privacy

See [SECURITY.md](SECURITY.md). In short, configuration values are validated and passed as argument vectors where possible, the installer refuses unsafe config paths, and close/switch actions use exact tmux targets. `codex-mux` stores only its theme and launch-profile settings. It does not store prompts, transcripts, Codex session metadata, credentials, or private Codex files.

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
