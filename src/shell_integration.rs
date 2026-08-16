//! Marker-managed Bash and Zsh prompt lifecycle integration.

use std::{
    collections::BTreeSet,
    fs::{self, File},
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use crate::install::{
    AtomicReplaceFailure, InstallError, InstallResult, atomic_replace, atomic_replace_tracked,
    create_backup, read, validate_regular_writable,
};

const LEADING_NEWLINE_FIELD: &str = "# codex-mux-owned-leading-newline: ";

/// Supported shell integration kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShellKind {
    /// GNU Bash primary prompt lifecycle.
    Bash,
    /// Zsh primary prompt lifecycle.
    Zsh,
}

impl ShellKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Bash => "Bash",
            Self::Zsh => "Zsh",
        }
    }

    const fn begin_marker(self) -> &'static str {
        match self {
            Self::Bash => "# >>> codex-mux bash >>>",
            Self::Zsh => "# >>> codex-mux zsh >>>",
        }
    }

    const fn end_marker(self) -> &'static str {
        match self {
            Self::Bash => "# <<< codex-mux bash <<<",
            Self::Zsh => "# <<< codex-mux zsh <<<",
        }
    }
}

/// Observable result for one shell configuration target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ShellOutcome {
    /// Shell whose integration was changed.
    pub kind: ShellKind,
    /// Exact host-owned startup file.
    pub path: PathBuf,
    /// Whether file bytes changed.
    pub changed: bool,
    /// First-install backup, when one was created.
    pub backup: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MarkerRegion {
    start: usize,
    end: usize,
}

struct ShellEdit {
    kind: ShellKind,
    path: PathBuf,
    original: Vec<u8>,
    replacement: Vec<u8>,
    mode: u32,
    existed: bool,
    backup_on_apply: bool,
    backup: Option<PathBuf>,
    applied: bool,
}

/// Prepared, rollback-capable edits for distinct shell startup files.
pub(crate) struct ShellTransaction {
    edits: Vec<ShellEdit>,
}

impl ShellTransaction {
    /// Prepares installation in all selected startup files without writing.
    pub(crate) fn prepare_install(
        targets: impl IntoIterator<Item = (ShellKind, PathBuf)>,
    ) -> InstallResult<Self> {
        Self::prepare(targets, Operation::Install)
    }

    /// Prepares removal in all selected startup files without writing.
    pub(crate) fn prepare_remove(
        targets: impl IntoIterator<Item = (ShellKind, PathBuf)>,
    ) -> InstallResult<Self> {
        Self::prepare(targets, Operation::Remove)
    }

    fn prepare(
        targets: impl IntoIterator<Item = (ShellKind, PathBuf)>,
        operation: Operation,
    ) -> InstallResult<Self> {
        let targets = targets.into_iter().collect::<Vec<_>>();
        let unique = targets
            .iter()
            .map(|(_, path)| path.clone())
            .collect::<BTreeSet<_>>();
        if unique.len() != targets.len() {
            return Err(InstallError::InvalidValue {
                field: "shell configuration paths",
                reason: "Bash and Zsh targets must be distinct".to_owned(),
            });
        }
        let edits = targets
            .into_iter()
            .map(|(kind, path)| prepare_edit(kind, path, operation))
            .collect::<InstallResult<Vec<_>>>()?;
        Ok(Self { edits })
    }

    /// Applies every changed file, rolling back earlier files on failure.
    pub(crate) fn apply(&mut self) -> InstallResult<Vec<ShellOutcome>> {
        for index in 0..self.edits.len() {
            if let Err(error) = apply_edit(&mut self.edits[index]) {
                let rollback = self.rollback();
                return match rollback {
                    Ok(()) => Err(error),
                    Err(rollback) => Err(InstallError::InvalidValue {
                        field: "shell configuration rollback",
                        reason: format!("{error}; rollback also failed: {rollback}"),
                    }),
                };
            }
        }
        Ok(self.outcomes())
    }

    /// Restores every file changed by this transaction.
    pub(crate) fn rollback(&mut self) -> InstallResult<()> {
        let mut failures = Vec::new();
        for edit in self.edits.iter_mut().rev().filter(|edit| edit.applied) {
            let restored = if edit.existed {
                atomic_replace(&edit.path, &edit.original, edit.mode)
            } else {
                match fs::remove_file(&edit.path) {
                    Ok(()) => sync_parent(&edit.path),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(source) => Err(InstallError::Filesystem {
                        path: edit.path.clone(),
                        source,
                    }),
                }
            };
            match restored {
                Ok(()) => edit.applied = false,
                Err(error) => failures.push(format!("{}: {error}", edit.path.display())),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(InstallError::InvalidValue {
                field: "shell configuration rollback",
                reason: failures.join("; "),
            })
        }
    }

    fn outcomes(&self) -> Vec<ShellOutcome> {
        self.edits
            .iter()
            .map(|edit| ShellOutcome {
                kind: edit.kind,
                path: edit.path.clone(),
                changed: edit.applied,
                backup: edit.backup.clone(),
            })
            .collect()
    }
}

#[derive(Clone, Copy)]
enum Operation {
    Install,
    Remove,
}

fn prepare_edit(kind: ShellKind, path: PathBuf, operation: Operation) -> InstallResult<ShellEdit> {
    if !path.is_absolute() {
        return Err(InstallError::UnsafePath {
            path,
            reason: "path must be absolute".to_owned(),
        });
    }
    let (original, mode, existed) = match fs::symlink_metadata(&path) {
        Ok(_) => {
            let metadata = validate_regular_writable(&path)?;
            (read(&path)?, metadata.mode(), true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            validate_missing_target(&path)?;
            (Vec::new(), 0o644, false)
        }
        Err(source) => {
            return Err(InstallError::Filesystem { path, source });
        }
    };
    let region = locate_markers(&original, kind)?;
    let owned_leading_newline = region.map_or(
        !original.is_empty() && !original.ends_with(b"\n"),
        |region| owned_leading_newline(&original, region),
    );
    let replacement = match operation {
        Operation::Install => replace_or_append(
            &original,
            region,
            owned_leading_newline,
            render_block(kind, owned_leading_newline).as_bytes(),
        ),
        Operation::Remove => remove_block(&original, region),
    };
    Ok(ShellEdit {
        kind,
        path,
        original,
        replacement,
        mode,
        existed,
        backup_on_apply: matches!(operation, Operation::Install) && existed && region.is_none(),
        backup: None,
        applied: false,
    })
}

fn apply_edit(edit: &mut ShellEdit) -> InstallResult<()> {
    if edit.existed && edit.replacement == edit.original {
        return Ok(());
    }
    if !edit.existed && edit.replacement.is_empty() {
        return Ok(());
    }
    if edit.backup_on_apply {
        edit.backup = Some(create_backup(&edit.path, &edit.original, edit.mode)?);
    }
    apply_replacement(edit, atomic_replace_tracked)
}

fn apply_replacement(
    edit: &mut ShellEdit,
    replace: impl FnOnce(&Path, &[u8], u32) -> Result<(), AtomicReplaceFailure>,
) -> InstallResult<()> {
    match replace(&edit.path, &edit.replacement, edit.mode) {
        Ok(()) => {
            edit.applied = true;
            Ok(())
        }
        Err(failure) => {
            edit.applied = failure.committed();
            Err(failure.into_error())
        }
    }
}

fn validate_missing_target(path: &Path) -> InstallResult<()> {
    let parent = path.parent().ok_or_else(|| InstallError::UnsafePath {
        path: path.to_owned(),
        reason: "path has no parent directory".to_owned(),
    })?;
    let metadata = fs::symlink_metadata(parent).map_err(|source| InstallError::UnsafePath {
        path: path.to_owned(),
        reason: format!("parent directory is unavailable: {source}"),
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(InstallError::UnsafePath {
            path: path.to_owned(),
            reason: "parent must be a real directory".to_owned(),
        });
    }
    if metadata.mode() & 0o200 == 0 {
        return Err(InstallError::UnsafePath {
            path: path.to_owned(),
            reason: "parent owner write bit is not set".to_owned(),
        });
    }
    Ok(())
}

fn locate_markers(bytes: &[u8], kind: ShellKind) -> InstallResult<Option<MarkerRegion>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| InstallError::Markers("shell configuration is not valid UTF-8".to_owned()))?;
    let mut begins = Vec::new();
    let mut ends = Vec::new();
    let mut offset = 0;
    for inclusive in text.split_inclusive('\n') {
        let line = inclusive.strip_suffix('\n').unwrap_or(inclusive);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line == kind.begin_marker() {
            begins.push(offset);
        }
        if line == kind.end_marker() {
            ends.push((offset, offset + inclusive.len()));
        }
        offset += inclusive.len();
    }
    match (begins.as_slice(), ends.as_slice()) {
        ([], []) => Ok(None),
        ([begin], [(end_start, end)]) if begin < end_start => Ok(Some(MarkerRegion {
            start: *begin,
            end: *end,
        })),
        ([], _) | (_, []) => Err(InstallError::Markers(format!(
            "{} shell markers must both be present",
            kind.label()
        ))),
        _ => Err(InstallError::Markers(format!(
            "{} shell markers must form exactly one non-nested block",
            kind.label()
        ))),
    }
}

fn replace_or_append(
    original: &[u8],
    region: Option<MarkerRegion>,
    owned_leading_newline: bool,
    block: &[u8],
) -> Vec<u8> {
    match region {
        Some(region) => {
            let mut output =
                Vec::with_capacity(original.len() - (region.end - region.start) + block.len());
            output.extend_from_slice(&original[..region.start]);
            output.extend_from_slice(block);
            output.extend_from_slice(&original[region.end..]);
            output
        }
        None => {
            let mut output = original.to_vec();
            if owned_leading_newline {
                output.push(b'\n');
            }
            output.extend_from_slice(block);
            output
        }
    }
}

fn remove_block(original: &[u8], region: Option<MarkerRegion>) -> Vec<u8> {
    let Some(region) = region else {
        return original.to_vec();
    };
    let start = if owned_leading_newline(original, region) && region.start > 0 {
        region.start - 1
    } else {
        region.start
    };
    let mut replacement = original.to_vec();
    replacement.drain(start..region.end);
    replacement
}

fn owned_leading_newline(bytes: &[u8], region: MarkerRegion) -> bool {
    std::str::from_utf8(&bytes[region.start..region.end])
        .ok()
        .and_then(|block| {
            block
                .lines()
                .find_map(|line| line.strip_prefix(LEADING_NEWLINE_FIELD))
        })
        == Some("true")
}

fn render_block(kind: ShellKind, owned_leading_newline: bool) -> String {
    let body = match kind {
        ShellKind::Bash => BASH_BODY,
        ShellKind::Zsh => ZSH_BODY,
    };
    format!(
        "{}\n# Managed by codex-mux; changes inside this block are replaced.\n\
         {LEADING_NEWLINE_FIELD}{owned_leading_newline}\n{body}{}\n",
        kind.begin_marker(),
        kind.end_marker()
    )
}

const BASH_BODY: &str = r#"__codex_mux_prompt_on() {
  local __codex_mux_status=$?
  if ! shopt -q promptvars; then
    __codex_mux_prompt_off
    return "$__codex_mux_status"
  fi
  if [[ -n ${TMUX_PANE-} ]] && command -v tmux >/dev/null 2>&1; then
    command tmux set-option -p -t "$TMUX_PANE" @codex_mux_shell_prompt 1 >/dev/null 2>&1
  fi
  return "$__codex_mux_status"
}
__codex_mux_prompt_off() {
  if [[ -n ${TMUX_PANE-} ]] && command -v tmux >/dev/null 2>&1; then
    command tmux set-option -pu -t "$TMUX_PANE" @codex_mux_shell_prompt >/dev/null 2>&1
  fi
}
if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == "declare -a"* ]]; then
  __codex_mux_prompt_found=false
  for __codex_mux_prompt_hook in "${PROMPT_COMMAND[@]}"; do
    [[ $__codex_mux_prompt_hook == __codex_mux_prompt_on ]] && __codex_mux_prompt_found=true
  done
  $__codex_mux_prompt_found || PROMPT_COMMAND=(__codex_mux_prompt_on "${PROMPT_COMMAND[@]}")
  unset __codex_mux_prompt_found __codex_mux_prompt_hook
else
  __codex_mux_previous_prompt_command=${PROMPT_COMMAND-}
  PROMPT_COMMAND=(__codex_mux_prompt_on)
  [[ -z $__codex_mux_previous_prompt_command ]] || PROMPT_COMMAND+=("$__codex_mux_previous_prompt_command")
  unset __codex_mux_previous_prompt_command
fi
case ${PS0-} in
  *'$(__codex_mux_prompt_off)'*) ;;
  *) PS0='$(__codex_mux_prompt_off)'"${PS0-}" ;;
esac
case ${PS2-} in
  *'$(__codex_mux_prompt_off)'*) ;;
  *) PS2='$(__codex_mux_prompt_off)'"${PS2-}" ;;
esac
"#;

const ZSH_BODY: &str = r#"__codex_mux_prompt_on() {
  if [[ ${__codex_mux_prompt_marked-0} != 1 && -n ${TMUX_PANE-} ]] && (( $+commands[tmux] )); then
    command tmux set-option -p -t "$TMUX_PANE" @codex_mux_shell_prompt 1 >/dev/null 2>&1 && __codex_mux_prompt_marked=1
  fi
  return 0
}
__codex_mux_prompt_off() {
  if [[ ${__codex_mux_prompt_marked-0} == 1 && -n ${TMUX_PANE-} ]] && (( $+commands[tmux] )); then
    command tmux set-option -pu -t "$TMUX_PANE" @codex_mux_shell_prompt >/dev/null 2>&1 && __codex_mux_prompt_marked=0
  fi
}
autoload -Uz add-zsh-hook
autoload -Uz add-zle-hook-widget
zmodload zsh/zle
__codex_mux_zle_prompt_state() {
  if [[ $CONTEXT == start && -z $PREBUFFER && $BUFFER != *$'\n'* ]]; then
    __codex_mux_prompt_on
  else
    __codex_mux_prompt_off
  fi
  return 0
}
__codex_mux_zle_line_finish() {
  __codex_mux_prompt_off
  return 0
}
add-zsh-hook -d precmd __codex_mux_prompt_on 2>/dev/null
add-zsh-hook -d preexec __codex_mux_prompt_off 2>/dev/null
add-zsh-hook precmd __codex_mux_prompt_on
add-zsh-hook preexec __codex_mux_prompt_off
add-zle-hook-widget -d line-init __codex_mux_zle_prompt_state 2>/dev/null
add-zle-hook-widget -d line-pre-redraw __codex_mux_zle_prompt_state 2>/dev/null
add-zle-hook-widget -d line-finish __codex_mux_zle_line_finish 2>/dev/null
add-zle-hook-widget line-init __codex_mux_zle_prompt_state
add-zle-hook-widget line-pre-redraw __codex_mux_zle_prompt_state
add-zle-hook-widget line-finish __codex_mux_zle_line_finish
"#;

fn sync_parent(path: &Path) -> InstallResult<()> {
    let parent = path.parent().ok_or_else(|| InstallError::UnsafePath {
        path: path.to_owned(),
        reason: "path has no parent directory".to_owned(),
    })?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| InstallError::Filesystem {
            path: parent.to_owned(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::install::atomic_replace_with;

    use super::{ShellKind, ShellTransaction, apply_replacement};

    fn scratch(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("codex-mux-shell-{name}-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn setup_and_remove_preserve_host_bytes_and_are_idempotent() {
        let root = scratch("round-trip");
        let bash = root.join(".bashrc");
        let zsh = root.join(".zshrc");
        fs::write(&bash, b"host bash").unwrap();
        fs::write(&zsh, b"host zsh\n").unwrap();

        let targets = [
            (ShellKind::Bash, bash.clone()),
            (ShellKind::Zsh, zsh.clone()),
        ];
        let mut setup = ShellTransaction::prepare_install(targets.clone()).unwrap();
        assert!(setup.apply().unwrap().iter().all(|outcome| outcome.changed));
        let installed_bash = fs::read(&bash).unwrap();
        let installed_zsh = fs::read(&zsh).unwrap();
        assert!(String::from_utf8_lossy(&installed_bash).contains("PROMPT_COMMAND"));
        assert!(String::from_utf8_lossy(&installed_zsh).contains("add-zsh-hook"));

        let mut again = ShellTransaction::prepare_install(targets.clone()).unwrap();
        assert!(
            again
                .apply()
                .unwrap()
                .iter()
                .all(|outcome| !outcome.changed)
        );
        assert_eq!(fs::read(&bash).unwrap(), installed_bash);
        assert_eq!(fs::read(&zsh).unwrap(), installed_zsh);

        let mut remove = ShellTransaction::prepare_remove(targets).unwrap();
        remove.apply().unwrap();
        assert_eq!(fs::read(&bash).unwrap(), b"host bash");
        assert_eq!(fs::read(&zsh).unwrap(), b"host zsh\n");
    }

    #[test]
    fn missing_files_are_created_and_remove_leaves_empty_files() {
        let root = scratch("missing");
        let bash = root.join(".bashrc");
        let targets = [(ShellKind::Bash, bash.clone())];

        ShellTransaction::prepare_install(targets.clone())
            .unwrap()
            .apply()
            .unwrap();
        assert!(bash.is_file());
        assert!(
            fs::read_to_string(&bash)
                .unwrap()
                .contains("codex-mux bash")
        );

        ShellTransaction::prepare_remove(targets)
            .unwrap()
            .apply()
            .unwrap();
        assert_eq!(fs::read(&bash).unwrap(), b"");
    }

    #[test]
    fn malformed_markers_and_duplicate_targets_fail_before_writes() {
        let root = scratch("malformed");
        let bash = root.join(".bashrc");
        fs::write(&bash, b"# >>> codex-mux bash >>>\n").unwrap();
        assert!(ShellTransaction::prepare_install([(ShellKind::Bash, bash.clone())]).is_err());
        assert_eq!(fs::read(&bash).unwrap(), b"# >>> codex-mux bash >>>\n");
        assert!(
            ShellTransaction::prepare_install([
                (ShellKind::Bash, bash.clone()),
                (ShellKind::Zsh, bash),
            ])
            .is_err()
        );
    }

    #[test]
    fn second_write_failure_restores_the_first_host_file() {
        let root = scratch("rollback");
        let bash = root.join("bashrc");
        let zsh_parent = root.join("zsh");
        let zsh = zsh_parent.join("zshrc");
        fs::create_dir(&zsh_parent).unwrap();
        fs::write(&bash, b"host bash\n").unwrap();
        fs::write(&zsh, b"host zsh\n").unwrap();
        let mut transaction = ShellTransaction::prepare_install([
            (ShellKind::Bash, bash.clone()),
            (ShellKind::Zsh, zsh),
        ])
        .unwrap();
        fs::remove_dir_all(zsh_parent).unwrap();

        assert!(transaction.apply().is_err());
        assert_eq!(fs::read(&bash).unwrap(), b"host bash\n");
    }

    #[test]
    fn bash_hook_is_source_idempotent_and_preserves_an_existing_command() {
        let root = scratch("bash-source");
        let bash = root.join("bashrc");
        fs::write(
            &bash,
            b"PROMPT_COMMAND='printf existing;'\nPS0='old-ps0'\nPS2='old-ps2'\n",
        )
        .unwrap();
        ShellTransaction::prepare_install([(ShellKind::Bash, bash.clone())])
            .unwrap()
            .apply()
            .unwrap();

        let output = Command::new("bash")
            .args([
                "--noprofile",
                "--norc",
                "-c",
                "source \"$1\"; source \"$1\"; declare -p PROMPT_COMMAND; printf 'PS0=%s\\nPS2=%s\\n' \"$PS0\" \"$PS2\"",
                "bash",
            ])
            .arg(&bash)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("declare -a PROMPT_COMMAND"), "{stdout}");
        assert_eq!(
            stdout.matches("__codex_mux_prompt_on").count(),
            1,
            "{stdout}"
        );
        assert!(stdout.contains("printf existing;"), "{stdout}");
        assert!(
            stdout.contains("PS0=$(__codex_mux_prompt_off)old-ps0"),
            "{stdout}"
        );
        assert!(
            stdout.contains("PS2=$(__codex_mux_prompt_off)old-ps2"),
            "{stdout}"
        );
    }

    #[test]
    fn post_rename_failure_is_recorded_and_rolled_back() {
        let root = scratch("post-rename");
        let bash = root.join("bashrc");
        fs::write(&bash, b"host bash\n").unwrap();
        let mut transaction =
            ShellTransaction::prepare_install([(ShellKind::Bash, bash.clone())]).unwrap();
        let unreadable = bash.clone();
        let result = apply_replacement(&mut transaction.edits[0], |path, bytes, mode| {
            atomic_replace_with(
                path,
                bytes,
                mode,
                |_| Ok(()),
                |_| {
                    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o0))?;
                    Err(std::io::Error::other(
                        "injected post-rename directory fsync failure",
                    ))
                },
            )
        });
        assert!(result.is_err());
        assert!(transaction.edits[0].applied);
        assert!(fs::read(&bash).is_err());
        transaction.rollback().unwrap();
        assert_eq!(fs::read(&bash).unwrap(), b"host bash\n");
    }

    #[test]
    fn rollback_continues_after_one_target_cannot_be_restored() {
        let root = scratch("best-effort-rollback");
        let bash = root.join("bashrc");
        let zsh = root.join("zshrc");
        fs::write(&bash, b"host bash\n").unwrap();
        fs::write(&zsh, b"host zsh\n").unwrap();
        let mut transaction = ShellTransaction::prepare_install([
            (ShellKind::Bash, bash.clone()),
            (ShellKind::Zsh, zsh.clone()),
        ])
        .unwrap();
        transaction.apply().unwrap();
        fs::remove_file(&zsh).unwrap();
        fs::create_dir(&zsh).unwrap();

        assert!(transaction.rollback().is_err());
        assert_eq!(fs::read(&bash).unwrap(), b"host bash\n");
        assert!(zsh.is_dir());
    }
}
