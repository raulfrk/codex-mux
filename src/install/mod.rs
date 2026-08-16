//! Safe management of the codex-mux-owned block in a host tmux configuration.

use std::{
    collections::BTreeSet,
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::ffi::OsStrExt,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use thiserror::Error;

/// First line of the uniquely owned tmux configuration block.
pub const BEGIN_MARKER: &str = "# >>> codex-mux >>>";
/// Last line of the uniquely owned tmux configuration block.
pub const END_MARKER: &str = "# <<< codex-mux <<<";

const KEY_FIELD: &str = "# codex-mux-key: ";
const BINARY_FIELD: &str = "# codex-mux-binary: ";
const CODEX_FIELD: &str = "# codex-executable: ";
const SMART_LEFT_FIELD: &str = "# codex-mux-smart-left: ";
const LEADING_NEWLINE_FIELD: &str = "# codex-mux-owned-leading-newline: ";

/// Installer-specific failures. Every error is fail-closed unless documented otherwise.
#[derive(Debug, Error)]
pub enum InstallError {
    /// Configuration discovery did not produce one unambiguous user file.
    #[error("configuration discovery failed: {0}")]
    Discovery(String),
    /// The selected path is not a safe writable regular file.
    #[error("unsafe tmux configuration {path}: {reason}")]
    UnsafePath {
        /// Rejected path.
        path: PathBuf,
        /// Actionable rejection reason.
        reason: String,
    },
    /// The marker structure is partial, duplicated, nested, or otherwise malformed.
    #[error("malformed codex-mux marker block: {0}")]
    Markers(String),
    /// An installer value cannot be represented safely.
    #[error("invalid {field}: {reason}")]
    InvalidValue {
        /// Value category.
        field: &'static str,
        /// Actionable validation failure.
        reason: String,
    },
    /// A filesystem operation failed at a known path.
    #[error("filesystem operation failed for {path}: {source}")]
    Filesystem {
        /// Affected path.
        path: PathBuf,
        /// Native error.
        #[source]
        source: io::Error,
    },
    /// The file was written, but the running tmux server could not be synchronized.
    #[error("could not synchronize running tmux with {path}: {message}")]
    ReloadFailed {
        /// Successfully updated configuration path.
        path: PathBuf,
        /// Reload diagnostic.
        message: String,
    },
}

/// Result type for installer operations.
pub type InstallResult<T> = std::result::Result<T, InstallError>;

/// Evidence available when choosing the host-owned tmux entrypoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServerEvidence {
    /// No tmux server is running, so standard user entrypoints are inspected.
    NotRunning,
    /// A server is running and reported its loaded `#{config_files}` paths.
    Running(Vec<PathBuf>),
}

/// Inputs used for deterministic configuration discovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryContext {
    /// Explicit `--config` selection, which takes precedence over all evidence.
    pub explicit: Option<PathBuf>,
    /// Running-server or no-server evidence.
    pub server: ServerEvidence,
    /// Current user's home directory.
    pub home: PathBuf,
    /// Effective XDG configuration root, when explicitly configured.
    pub xdg_config_home: Option<PathBuf>,
}

/// Chooses exactly one existing safe tmux entrypoint.
pub fn discover_config(context: &DiscoveryContext) -> InstallResult<PathBuf> {
    if let Some(path) = &context.explicit {
        validate_regular_writable(path)?;
        return Ok(path.clone());
    }

    match &context.server {
        ServerEvidence::Running(loaded) => {
            let unique = loaded.iter().cloned().collect::<BTreeSet<_>>();
            let user_candidates = unique
                .into_iter()
                .filter(|path| path.starts_with(&context.home))
                .filter(|path| validate_regular_writable(path).is_ok())
                .collect::<Vec<_>>();
            exactly_one(
                user_candidates,
                "running tmux server did not report exactly one safe user configuration",
            )
        }
        ServerEvidence::NotRunning => {
            let mut standards = vec![context.home.join(".tmux.conf")];
            if let Some(xdg) = &context.xdg_config_home {
                standards.push(xdg.join("tmux/tmux.conf"));
            }
            let conventional = context.home.join(".config/tmux/tmux.conf");
            if !standards.contains(&conventional) {
                standards.push(conventional);
            }
            let candidates = standards
                .into_iter()
                .filter(|path| validate_regular_writable(path).is_ok())
                .collect::<Vec<_>>();
            exactly_one(
                candidates,
                "no running server and standard entrypoints were missing or ambiguous",
            )
        }
    }
}

fn exactly_one(mut candidates: Vec<PathBuf>, message: &str) -> InstallResult<PathBuf> {
    if candidates.len() == 1 {
        return Ok(candidates.remove(0));
    }
    let rendered = candidates
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(InstallError::Discovery(if rendered.is_empty() {
        message.to_owned()
    } else {
        format!("{message}: {rendered}")
    }))
}

fn validate_regular_writable(path: &Path) -> InstallResult<fs::Metadata> {
    let metadata = fs::symlink_metadata(path).map_err(|source| InstallError::UnsafePath {
        path: path.to_owned(),
        reason: source.to_string(),
    })?;
    if metadata.file_type().is_symlink() {
        return Err(InstallError::UnsafePath {
            path: path.to_owned(),
            reason: "symbolic links are refused".to_owned(),
        });
    }
    if !metadata.file_type().is_file() {
        return Err(InstallError::UnsafePath {
            path: path.to_owned(),
            reason: "not a regular file".to_owned(),
        });
    }
    if metadata.mode() & 0o200 == 0 {
        return Err(InstallError::UnsafePath {
            path: path.to_owned(),
            reason: "owner write bit is not set".to_owned(),
        });
    }
    Ok(metadata)
}

/// Absolute paths embedded in the managed block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutablePaths {
    /// Installed `codex-mux` binary.
    pub mux: PathBuf,
    /// Configured Codex executable.
    pub codex: PathBuf,
}

impl ExecutablePaths {
    /// Validates that both executable references are absolute, non-empty paths.
    pub fn new(mux: PathBuf, codex: PathBuf) -> InstallResult<Self> {
        for (field, path) in [("codex-mux executable", &mux), ("Codex executable", &codex)] {
            if path.as_os_str().is_empty() || !path.is_absolute() {
                return Err(InstallError::InvalidValue {
                    field,
                    reason: "must be an absolute path".to_owned(),
                });
            }
            if path.to_str().is_none() {
                return Err(InstallError::InvalidValue {
                    field,
                    reason: "must be valid UTF-8 for tmux configuration".to_owned(),
                });
            }
            if path
                .to_str()
                .is_some_and(|value| value.chars().any(char::is_control))
            {
                return Err(InstallError::InvalidValue {
                    field,
                    reason: "must not contain control characters".to_owned(),
                });
            }
            if path
                .to_str()
                .is_some_and(|value| value.contains(['#', '$']))
            {
                return Err(InstallError::InvalidValue {
                    field,
                    reason: "must not contain tmux expansion characters (`#` or `$`)".to_owned(),
                });
            }
        }
        Ok(Self { mux, codex })
    }
}

/// Reload boundary used to keep configuration writes independently testable.
pub trait TmuxReloader {
    /// Returns whether a server is currently available for reload.
    fn is_running(&self) -> bool;
    /// Removes an old owned prefix binding before the updated host file is sourced.
    fn unbind(&mut self, key: &str) -> std::result::Result<(), String>;
    /// Returns whether the live root table already owns Left.
    fn root_left_bound(&mut self) -> std::result::Result<bool, String> {
        Ok(false)
    }
    /// Removes the codex-mux-owned live root-table Left binding.
    fn unbind_root_left(&mut self, _expected_mux: &Path) -> std::result::Result<(), String> {
        Ok(())
    }
    /// Sources the exact updated entrypoint in the running server.
    fn reload(&mut self, path: &Path) -> std::result::Result<(), String>;
}

/// A reloader for tests and hosts without a running tmux server.
#[derive(Default)]
pub struct NoRunningServer;

impl TmuxReloader for NoRunningServer {
    fn is_running(&self) -> bool {
        false
    }

    fn unbind(&mut self, _key: &str) -> std::result::Result<(), String> {
        Ok(())
    }

    fn reload(&mut self, _path: &Path) -> std::result::Result<(), String> {
        Ok(())
    }
}

/// Outcome of an install or update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallOutcome {
    /// Updated host entrypoint.
    pub path: PathBuf,
    /// Backup created for the first installation, if any.
    pub backup: Option<PathBuf>,
    /// Whether bytes changed.
    pub changed: bool,
    /// Whether a running server was successfully reloaded.
    pub reloaded: bool,
}

/// Installs or updates the unique owned block, preserving all other bytes.
pub fn install(
    path: &Path,
    key: &str,
    executables: &ExecutablePaths,
    reloader: &mut dyn TmuxReloader,
) -> InstallResult<InstallOutcome> {
    install_with_options(path, key, false, executables, reloader)
}

/// Installs or updates the owned block with optional Smart Left activation.
pub fn install_with_options(
    path: &Path,
    key: &str,
    smart_left: bool,
    executables: &ExecutablePaths,
    reloader: &mut dyn TmuxReloader,
) -> InstallResult<InstallOutcome> {
    validate_key(key)?;
    if smart_left {
        validate_smart_left_executable(executables)?;
    }
    let metadata = validate_regular_writable(path)?;
    let original = read(path)?;
    let markers = locate_markers(&original)?;
    let previous_key = markers.and_then(|region| block_field(&original, region, KEY_FIELD));
    let previous_mux = markers.and_then(|region| block_field(&original, region, BINARY_FIELD));
    let previous_smart_left = markers
        .map(|region| block_smart_left(&original, region))
        .transpose()?
        .unwrap_or(false);
    if smart_left && !previous_smart_left {
        if has_root_left_binding_outside_owned_block(&original, markers)? {
            return Err(InstallError::InvalidValue {
                field: "Smart Left binding",
                reason: "the selected tmux configuration already binds root-table Left".to_owned(),
            });
        }
        if reloader.is_running()
            && reloader
                .root_left_bound()
                .map_err(|message| InstallError::ReloadFailed {
                    path: path.to_owned(),
                    message,
                })?
        {
            return Err(InstallError::InvalidValue {
                field: "Smart Left binding",
                reason: "the running tmux server already binds root-table Left".to_owned(),
            });
        }
    }
    let owned_leading_newline = markers.map_or(
        !original.is_empty() && !original.ends_with(b"\n"),
        |region| owned_leading_newline(&original, region),
    );
    let block = render_block(key, smart_left, executables, owned_leading_newline);
    let replacement =
        replace_or_append(&original, markers, owned_leading_newline, block.as_bytes());

    let running = reloader.is_running();
    if replacement == original {
        let mut reloaded = false;
        if running {
            reloader
                .reload(path)
                .map_err(|message| InstallError::ReloadFailed {
                    path: path.to_owned(),
                    message,
                })?;
            reloaded = true;
        }
        return Ok(InstallOutcome {
            path: path.to_owned(),
            backup: None,
            changed: false,
            reloaded,
        });
    }

    let backup = if markers.is_none() {
        Some(create_backup(path, &original, metadata.mode())?)
    } else {
        None
    };
    let changed_key = previous_key.as_deref().filter(|previous| *previous != key);
    let removed_smart_left = previous_smart_left && !smart_left;
    let previous_mux = if removed_smart_left {
        Some(previous_mux.as_deref().ok_or_else(|| {
            InstallError::Markers("owned block is missing its codex-mux path".to_owned())
        })?)
    } else {
        None
    };
    if running {
        if removed_smart_left {
            if let Err(message) = reloader.unbind_root_left(Path::new(previous_mux.unwrap())) {
                return Err(InstallError::ReloadFailed {
                    path: path.to_owned(),
                    message,
                });
            }
        }
        if let Some(previous) = changed_key {
            if let Err(message) = reloader.unbind(previous) {
                if removed_smart_left {
                    let _ = reloader.reload(path);
                }
                return Err(InstallError::ReloadFailed {
                    path: path.to_owned(),
                    message,
                });
            }
        }
    }
    if let Err(error) = atomic_replace(path, &replacement, metadata.mode()) {
        if running && (changed_key.is_some() || removed_smart_left) {
            let _ = reloader.reload(path);
        }
        return Err(error);
    }

    let mut reloaded = false;
    if running {
        reloader
            .reload(path)
            .map_err(|message| InstallError::ReloadFailed {
                path: path.to_owned(),
                message,
            })?;
        reloaded = true;
    }
    Ok(InstallOutcome {
        path: path.to_owned(),
        backup,
        changed: true,
        reloaded,
    })
}

/// Installed block details and drift relative to currently resolved executables.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallStatus {
    /// Inspected host entrypoint.
    pub path: PathBuf,
    /// Whether a valid owned block exists.
    pub installed: bool,
    /// Installed prefix key.
    pub key: Option<String>,
    /// Recorded codex-mux binary path.
    pub mux: Option<PathBuf>,
    /// Recorded Codex executable path.
    pub codex: Option<PathBuf>,
    /// Whether the owned root-table Smart Left binding is enabled.
    pub smart_left: bool,
    /// Human-readable path mismatches.
    pub drift: Vec<String>,
}

/// Reads installation state without writing any file.
pub fn status(path: &Path, expected: &ExecutablePaths) -> InstallResult<InstallStatus> {
    validate_regular_writable(path)?;
    let bytes = read(path)?;
    let Some(region) = locate_markers(&bytes)? else {
        return Ok(InstallStatus {
            path: path.to_owned(),
            installed: false,
            key: None,
            mux: None,
            codex: None,
            smart_left: false,
            drift: Vec::new(),
        });
    };
    let block = std::str::from_utf8(&bytes[region.marker_start..region.end])
        .map_err(|_| InstallError::Markers("owned block is not valid UTF-8".to_owned()))?;
    let key = field(block, KEY_FIELD).map(ToOwned::to_owned);
    let mux = field(block, BINARY_FIELD).map(PathBuf::from);
    let codex = field(block, CODEX_FIELD).map(PathBuf::from);
    let smart_left = parse_smart_left(field(block, SMART_LEFT_FIELD))?;
    let mut drift = Vec::new();
    if mux.as_deref() != Some(expected.mux.as_path()) {
        drift.push(format!(
            "codex-mux path: installed={} resolved={}",
            mux.as_deref()
                .map_or("<missing>".to_owned(), |p| p.display().to_string()),
            expected.mux.display()
        ));
    }
    if codex.as_deref() != Some(expected.codex.as_path()) {
        drift.push(format!(
            "Codex path: installed={} resolved={}",
            codex
                .as_deref()
                .map_or("<missing>".to_owned(), |p| p.display().to_string()),
            expected.codex.display()
        ));
    }
    Ok(InstallStatus {
        path: path.to_owned(),
        installed: true,
        key,
        mux,
        codex,
        smart_left,
        drift,
    })
}

/// Removes only the unique owned block and preserves every other byte.
pub fn uninstall(path: &Path, reloader: &mut dyn TmuxReloader) -> InstallResult<bool> {
    let metadata = validate_regular_writable(path)?;
    let original = read(path)?;
    let Some(region) = locate_markers(&original)? else {
        if reloader.is_running() {
            reloader
                .reload(path)
                .map_err(|message| InstallError::ReloadFailed {
                    path: path.to_owned(),
                    message,
                })?;
        }
        return Ok(false);
    };
    let key = block_field(&original, region, KEY_FIELD).ok_or_else(|| {
        InstallError::Markers("owned block is missing its binding key".to_owned())
    })?;
    let smart_left = block_smart_left(&original, region)?;
    let mux = block_field(&original, region, BINARY_FIELD).ok_or_else(|| {
        InstallError::Markers("owned block is missing its codex-mux path".to_owned())
    })?;
    let start = if owned_leading_newline(&original, region) && region.start > 0 {
        region.start - 1
    } else {
        region.start
    };
    let mut replacement = original;
    replacement.drain(start..region.end);
    let running = reloader.is_running();
    if running {
        if smart_left {
            if let Err(message) = reloader.unbind_root_left(Path::new(&mux)) {
                return Err(InstallError::ReloadFailed {
                    path: path.to_owned(),
                    message,
                });
            }
        }
        if let Err(message) = reloader.unbind(&key) {
            if smart_left {
                let _ = reloader.reload(path);
            }
            return Err(InstallError::ReloadFailed {
                path: path.to_owned(),
                message,
            });
        }
    }
    if let Err(error) = atomic_replace(path, &replacement, metadata.mode()) {
        if running {
            let _ = reloader.reload(path);
        }
        return Err(error);
    }
    if running {
        reloader
            .reload(path)
            .map_err(|message| InstallError::ReloadFailed {
                path: path.to_owned(),
                message,
            })?;
    }
    Ok(true)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MarkerRegion {
    start: usize,
    marker_start: usize,
    end: usize,
}

fn locate_markers(bytes: &[u8]) -> InstallResult<Option<MarkerRegion>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| InstallError::Markers("configuration is not valid UTF-8".to_owned()))?;
    let mut begins = Vec::new();
    let mut ends = Vec::new();
    let mut offset = 0;
    for inclusive in text.split_inclusive('\n') {
        let line = inclusive.strip_suffix('\n').unwrap_or(inclusive);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line == BEGIN_MARKER {
            begins.push(offset);
        }
        if line == END_MARKER {
            ends.push((offset, offset + inclusive.len()));
        }
        offset += inclusive.len();
    }
    match (begins.as_slice(), ends.as_slice()) {
        ([], []) => Ok(None),
        ([begin], [(end_start, end)]) if begin < end_start => Ok(Some(MarkerRegion {
            start: *begin,
            marker_start: *begin,
            end: *end,
        })),
        ([], _) | (_, []) => Err(InstallError::Markers(
            "begin and end markers must both be present".to_owned(),
        )),
        _ => Err(InstallError::Markers(
            "markers must form exactly one non-nested block".to_owned(),
        )),
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

fn render_block(
    key: &str,
    smart_left: bool,
    executables: &ExecutablePaths,
    owned_leading_newline: bool,
) -> String {
    let command = [
        shell_word(&executables.mux),
        "--codex".to_owned(),
        shell_word(&executables.codex),
        "--client".to_owned(),
        shell_format("client_tty"),
        "--invoking-pane".to_owned(),
        shell_format("pane_id"),
        "--invoking-session".to_owned(),
        shell_format("session_id"),
        "--invoking-path".to_owned(),
        shell_format("pane_current_path"),
    ]
    .join(" ");
    let command = tmux_word(&command);
    let compact = "#{||:#{<:#{client_width},90},#{<:#{client_height},28}}";
    let width = format!("#{{?{compact},100%,80%}}");
    let height = format!("#{{?{compact},100%,70%}}");
    let popup = tmux_word(&format!(
        "display-popup -E -w '{width}' -h '{height}' {command}"
    ));
    let smart_binding = if smart_left {
        render_smart_left_binding(executables)
    } else {
        String::new()
    };
    format!(
        "{BEGIN_MARKER}\n# Managed by codex-mux; changes inside this block are replaced.\n{LEADING_NEWLINE_FIELD}{owned_leading_newline}\n{KEY_FIELD}{key}\n{BINARY_FIELD}{}\n{CODEX_FIELD}{}\n{SMART_LEFT_FIELD}{smart_left}\nbind-key {key} run-shell -C {popup}\n{smart_binding}{END_MARKER}\n",
        executables.mux.display(),
        executables.codex.display(),
    )
}

fn render_smart_left_binding(executables: &ExecutablePaths) -> String {
    let command_name = executables
        .codex
        .file_name()
        .and_then(|name| name.to_str())
        .expect("validated Smart Left executable name");
    let probe = [
        shell_word(&executables.mux),
        "--codex".to_owned(),
        shell_word(&executables.codex),
        "--client".to_owned(),
        shell_format("client_tty"),
        "--invoking-pane".to_owned(),
        shell_format("pane_id"),
        "--invoking-session".to_owned(),
        shell_format("session_id"),
        "--invoking-path".to_owned(),
        shell_format("pane_current_path"),
        "smart-left".to_owned(),
    ]
    .join(" ");
    let fallback = format!("tmux send-keys -t {} Left", shell_format("pane_id"));
    let cleanup = format!(
        "tmux set-option -pu -t {} @codex_mux_smart_left_active",
        shell_format("pane_id")
    );
    let owner = smart_left_owner(&executables.mux);
    let shell = format!(
        "owner={owner}; if [ -x {} ]; then {probe}; else {fallback}; fi; {cleanup}",
        shell_word(&executables.mux)
    );
    let condition = format!(
        "#{{&&:#{{==:#{{pane_current_command}},{command_name}}},#{{!=:#{{@codex_mux_smart_left_active}},1}}}}"
    );
    format!(
        "bind-key -T root Left if-shell -F '{condition}' {{\n  set-option -p @codex_mux_smart_left_active 1\n  run-shell -b {}\n}} {{\n  send-keys Left\n}}\n",
        tmux_word(&shell)
    )
}

pub(crate) fn smart_left_owner(path: &Path) -> String {
    let mut token = String::with_capacity(path.as_os_str().as_bytes().len() * 2);
    for byte in path.as_os_str().as_bytes() {
        write!(&mut token, "{byte:02x}").expect("writing to a String cannot fail");
    }
    token
}

fn owned_leading_newline(bytes: &[u8], region: MarkerRegion) -> bool {
    block_field(bytes, region, LEADING_NEWLINE_FIELD).as_deref() == Some("true")
}

fn block_field(bytes: &[u8], region: MarkerRegion, prefix: &str) -> Option<String> {
    std::str::from_utf8(&bytes[region.marker_start..region.end])
        .ok()
        .and_then(|block| field(block, prefix))
        .map(ToOwned::to_owned)
}

fn block_smart_left(bytes: &[u8], region: MarkerRegion) -> InstallResult<bool> {
    parse_smart_left(block_field(bytes, region, SMART_LEFT_FIELD).as_deref())
}

fn parse_smart_left(value: Option<&str>) -> InstallResult<bool> {
    match value {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(value) => Err(InstallError::Markers(format!(
            "invalid Smart Left metadata {value:?}"
        ))),
    }
}

fn validate_smart_left_executable(executables: &ExecutablePaths) -> InstallResult<()> {
    let Some(name) = executables.codex.file_name().and_then(|name| name.to_str()) else {
        return Err(InstallError::InvalidValue {
            field: "Smart Left Codex executable",
            reason: "must have a UTF-8 file name".to_owned(),
        });
    };
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._+-".contains(character))
    {
        return Err(InstallError::InvalidValue {
            field: "Smart Left Codex executable",
            reason: "file name must contain only ASCII letters, digits, dot, underscore, plus, or hyphen"
                .to_owned(),
        });
    }
    Ok(())
}

fn has_root_left_binding_outside_owned_block(
    bytes: &[u8],
    region: Option<MarkerRegion>,
) -> InstallResult<bool> {
    let mut outside = bytes.to_vec();
    if let Some(region) = region {
        outside.drain(region.start..region.end);
    }
    let text = std::str::from_utf8(&outside)
        .map_err(|_| InstallError::Markers("configuration is not valid UTF-8".to_owned()))?;
    Ok(text_binds_root_left(text))
}

fn text_binds_root_left(text: &str) -> bool {
    let mut logical = String::new();
    for physical in text.split_inclusive('\n') {
        let has_newline = physical.ends_with('\n');
        let line = physical.strip_suffix('\n').unwrap_or(physical);
        logical.push_str(line);
        if has_newline && has_unescaped_trailing_backslash(&logical) {
            logical.pop();
            continue;
        }
        if line_binds_root_left(&logical) {
            return true;
        }
        logical.clear();
    }
    !logical.is_empty() && line_binds_root_left(&logical)
}

fn has_unescaped_trailing_backslash(line: &str) -> bool {
    line.as_bytes()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\\')
        .count()
        % 2
        == 1
}

fn line_binds_root_left(line: &str) -> bool {
    let starts_with_bind = line
        .trim_start()
        .split_ascii_whitespace()
        .next()
        .is_some_and(|command| matches!(command, "bind" | "bind-key"));
    let Some(commands) = tmux_commands(line) else {
        return starts_with_bind;
    };
    commands.into_iter().any(command_binds_root_left)
}

fn command_binds_root_left(words: Vec<String>) -> bool {
    let mut words = words.into_iter();
    if !words
        .next()
        .is_some_and(|command| matches!(command.as_str(), "bind" | "bind-key"))
    {
        return false;
    }
    let mut table = "prefix".to_owned();
    let mut key = None;
    while let Some(word) = words.next() {
        match word.as_str() {
            "-n" => table = "root".to_owned(),
            "-T" => table = words.next().unwrap_or_default(),
            "-N" => {
                let _ = words.next();
            }
            "-r" => {}
            value if value.starts_with('-') && value != "-" => return true,
            value => {
                key = Some(value.to_owned());
                break;
            }
        }
    }
    key.is_none() || (table == "root" && key.as_deref() == Some("Left"))
}

fn tmux_commands(line: &str) -> Option<Vec<Vec<String>>> {
    let mut commands = vec![Vec::new()];
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in line.chars() {
        if escaped {
            word.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            } else {
                word.push(character);
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            ';' => {
                if !word.is_empty() {
                    commands.last_mut()?.push(std::mem::take(&mut word));
                }
                commands.push(Vec::new());
            }
            '#' if word.is_empty() && commands.last()?.is_empty() => break,
            character if character.is_ascii_whitespace() => {
                if !word.is_empty() {
                    commands.last_mut()?.push(std::mem::take(&mut word));
                }
            }
            _ => word.push(character),
        }
    }
    if escaped || quote.is_some() {
        return None;
    }
    if !word.is_empty() {
        commands.last_mut()?.push(word);
    }
    Some(commands)
}

fn validate_key(key: &str) -> InstallResult<()> {
    if key.is_empty()
        || key
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        || key.contains([';', '#', '\'', '"'])
    {
        return Err(InstallError::InvalidValue {
            field: "binding key",
            reason: "must be one non-whitespace tmux key token".to_owned(),
        });
    }
    Ok(())
}

fn shell_word(path: &Path) -> String {
    shell_literal(path.to_str().expect("validated executable UTF-8"))
}

fn shell_format(variable: &str) -> String {
    format!("#{{q:{variable}}}")
}

fn shell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn tmux_word(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$")
    )
}

fn field<'a>(block: &'a str, prefix: &str) -> Option<&'a str> {
    block.lines().find_map(|line| line.strip_prefix(prefix))
}

fn read(path: &Path) -> InstallResult<Vec<u8>> {
    fs::read(path).map_err(|source| InstallError::Filesystem {
        path: path.to_owned(),
        source,
    })
}

fn create_backup(path: &Path, bytes: &[u8], mode: u32) -> InstallResult<PathBuf> {
    for suffix in 0..1000 {
        let extension = if suffix == 0 {
            "codex-mux.bak".to_owned()
        } else {
            format!("codex-mux.bak.{suffix}")
        };
        let candidate = path.with_extension(extension);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                file.set_permissions(fs::Permissions::from_mode(mode & 0o7777))
                    .map_err(|source| InstallError::Filesystem {
                        path: candidate.clone(),
                        source,
                    })?;
                file.write_all(bytes)
                    .and_then(|()| file.sync_all())
                    .map_err(|source| InstallError::Filesystem {
                        path: candidate.clone(),
                        source,
                    })?;
                return Ok(candidate);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(InstallError::Filesystem {
                    path: candidate,
                    source,
                });
            }
        }
    }
    Err(InstallError::Discovery(
        "could not allocate a collision-safe backup name".to_owned(),
    ))
}

fn atomic_replace(path: &Path, bytes: &[u8], mode: u32) -> InstallResult<()> {
    atomic_replace_with(path, bytes, mode, |_| Ok(()))
}

fn atomic_replace_with(
    path: &Path,
    bytes: &[u8],
    mode: u32,
    before_rename: impl FnOnce(&Path) -> io::Result<()>,
) -> InstallResult<()> {
    let parent = path.parent().ok_or_else(|| InstallError::UnsafePath {
        path: path.to_owned(),
        reason: "path has no parent directory".to_owned(),
    })?;
    let name = path.file_name().ok_or_else(|| InstallError::UnsafePath {
        path: path.to_owned(),
        reason: "path has no file name".to_owned(),
    })?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".{}.codex-mux.{nonce}.tmp", name.to_string_lossy()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.set_permissions(fs::Permissions::from_mode(mode & 0o7777))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        before_rename(&temporary)?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok::<(), io::Error>(())
    })();
    if let Err(source) = result {
        let _ = fs::remove_file(&temporary);
        return Err(InstallError::Filesystem {
            path: path.to_owned(),
            source,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::atomic_replace_with;

    #[test]
    fn failed_rename_keeps_original_and_removes_temporary_file() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("codex-mux-rename-failure-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        let target = root.join("tmux.conf");
        fs::write(&target, b"original bytes\n").unwrap();

        let error = atomic_replace_with(&target, b"replacement\n", 0o600, |_| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected rename failure",
            ))
        })
        .unwrap_err();

        assert!(error.to_string().contains("injected rename failure"));
        assert_eq!(fs::read(&target).unwrap(), b"original bytes\n");
        let remaining = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<PathBuf>>();
        assert_eq!(remaining, vec![target]);
    }
}
