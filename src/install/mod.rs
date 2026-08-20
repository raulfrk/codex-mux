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
const LAUNCH_FIELD: &str = "# codex-launch-executable: ";
const MATCH_FIELD: &str = "# codex-match-executable: ";
const PANE_COMMAND_FIELD: &str = "# codex-pane-command: ";
const MATCH_SCOPE_FIELD: &str = "# codex-match-scope: ";
const MATCH_COMMAND_REGEX_FIELD: &str = "# codex-match-command-regex: ";
const PANE_COMMAND_REGEX_FIELD: &str = "# codex-pane-command-regex: ";
const SMART_LEFT_FIELD: &str = "# codex-mux-smart-left: ";
const LEADING_NEWLINE_FIELD: &str = "# codex-mux-owned-leading-newline: ";

/// Installer-specific failures. Every error is fail-closed unless documented otherwise.
#[derive(Debug, Error)]
pub enum InstallError {
    /// Configuration discovery did not produce one unambiguous user file.
    #[error("configuration discovery failed: {0}")]
    Discovery(String),
    /// The selected path is not a safe writable regular file.
    #[error("unsafe configuration {path}: {reason}")]
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

pub(crate) fn validate_regular_writable(path: &Path) -> InstallResult<fs::Metadata> {
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
    /// Exact executable or script paths recognized during discovery.
    pub match_executables: Vec<PathBuf>,
    /// Exact tmux pane commands accepted by Smart Left.
    pub pane_commands: Vec<String>,
    /// Candidate scope serialized into generated bindings.
    pub match_scope: String,
    /// Regexes matched against normalized process command lines.
    pub match_command_regexes: Vec<String>,
    /// Regexes matched against pane_current_command.
    pub pane_command_regexes: Vec<String>,
    /// Whether generated invocations retain the legacy `--codex` shorthand.
    pub legacy_shorthand: bool,
}

/// Process detection metadata embedded in the managed tmux block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessMetadata {
    /// Exact executable or script identities accepted during discovery.
    pub match_executables: Vec<PathBuf>,
    /// Exact Smart Left pane-command prefilters.
    pub pane_commands: Vec<String>,
    /// Candidate-process selection scope.
    pub match_scope: String,
    /// Regex fallbacks for normalized process argv.
    pub match_command_regexes: Vec<String>,
    /// Regex Smart Left pane-command prefilters.
    pub pane_command_regexes: Vec<String>,
}

impl ExecutablePaths {
    /// Validates that both executable references are absolute, non-empty paths.
    pub fn new(mux: PathBuf, codex: PathBuf) -> InstallResult<Self> {
        let pane_command = codex
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        Self::with_process(
            mux,
            codex.clone(),
            ProcessMetadata {
                match_executables: vec![codex],
                pane_commands: vec![pane_command],
                match_scope: "foreground".to_owned(),
                match_command_regexes: Vec::new(),
                pane_command_regexes: Vec::new(),
            },
            true,
        )
    }

    /// Validates complete launch and process-detection metadata.
    pub fn with_process(
        mux: PathBuf,
        codex: PathBuf,
        metadata: ProcessMetadata,
        legacy_shorthand: bool,
    ) -> InstallResult<Self> {
        let ProcessMetadata {
            match_executables,
            pane_commands,
            match_scope,
            match_command_regexes,
            pane_command_regexes,
        } = metadata;
        if match_executables.is_empty()
            || (pane_commands.is_empty() && pane_command_regexes.is_empty())
        {
            return Err(InstallError::InvalidValue {
                field: "process configuration",
                reason: "match executables and pane commands must be non-empty".to_owned(),
            });
        }
        for (field, path) in std::iter::once(("codex-mux executable", &mux))
            .chain(std::iter::once(("Codex launch executable", &codex)))
            .chain(
                match_executables
                    .iter()
                    .map(|path| ("Codex match executable", path)),
            )
        {
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
        for command in &pane_commands {
            if command.is_empty() || command.chars().any(char::is_control) {
                return Err(InstallError::InvalidValue {
                    field: "Codex pane command",
                    reason: "must be non-empty and contain no control characters".to_owned(),
                });
            }
        }
        for expression in match_command_regexes.iter().chain(&pane_command_regexes) {
            if expression.is_empty() || expression.chars().any(char::is_control) {
                return Err(InstallError::InvalidValue {
                    field: "process regex",
                    reason: "must be non-empty and contain no control characters".to_owned(),
                });
            }
        }
        Ok(Self {
            mux,
            codex,
            match_executables,
            pane_commands,
            match_scope,
            match_command_regexes,
            pane_command_regexes,
            legacy_shorthand,
        })
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
    /// Installed exact process match paths.
    pub match_executables: Vec<PathBuf>,
    /// Installed exact Smart Left prefilter commands.
    pub pane_commands: Vec<String>,
    /// Installed candidate matching scope.
    pub match_scope: Option<String>,
    /// Installed process command regexes.
    pub match_command_regexes: Vec<String>,
    /// Installed Smart Left pane-command regexes.
    pub pane_command_regexes: Vec<String>,
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
            match_executables: Vec::new(),
            pane_commands: Vec::new(),
            match_scope: None,
            match_command_regexes: Vec::new(),
            pane_command_regexes: Vec::new(),
            smart_left: false,
            drift: Vec::new(),
        });
    };
    let block = std::str::from_utf8(&bytes[region.marker_start..region.end])
        .map_err(|_| InstallError::Markers("owned block is not valid UTF-8".to_owned()))?;
    let key = field(block, KEY_FIELD).map(ToOwned::to_owned);
    let mux = field(block, BINARY_FIELD).map(PathBuf::from);
    let legacy_codex = field(block, CODEX_FIELD).map(PathBuf::from);
    let codex = field(block, LAUNCH_FIELD)
        .map(PathBuf::from)
        .or_else(|| legacy_codex.clone());
    let mut match_executables = fields(block, MATCH_FIELD)
        .into_iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if match_executables.is_empty() {
        match_executables.extend(legacy_codex.clone());
    }
    let mut pane_commands = fields(block, PANE_COMMAND_FIELD);
    let match_scope = field(block, MATCH_SCOPE_FIELD).map(ToOwned::to_owned);
    let match_command_regexes = fields(block, MATCH_COMMAND_REGEX_FIELD);
    let pane_command_regexes = fields(block, PANE_COMMAND_REGEX_FIELD);
    if pane_commands.is_empty() {
        pane_commands.extend(
            legacy_codex
                .as_deref()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .map(ToOwned::to_owned),
        );
    }
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
    if match_executables != expected.match_executables {
        drift.push(format!(
            "Codex match executables: installed={} resolved={}",
            display_paths(&match_executables),
            display_paths(&expected.match_executables)
        ));
    }
    if pane_commands != expected.pane_commands {
        drift.push(format!(
            "Codex pane commands: installed={} resolved={}",
            pane_commands.join(","),
            expected.pane_commands.join(",")
        ));
    }
    if match_scope.as_deref() != Some(expected.match_scope.as_str()) {
        drift.push(format!(
            "Codex match scope: installed={} resolved={}",
            match_scope.as_deref().unwrap_or("<missing>"),
            expected.match_scope
        ));
    }
    if match_command_regexes != expected.match_command_regexes {
        drift.push("Codex match command regexes differ".to_owned());
    }
    if pane_command_regexes != expected.pane_command_regexes {
        drift.push("Codex pane command regexes differ".to_owned());
    }
    Ok(InstallStatus {
        path: path.to_owned(),
        installed: true,
        key,
        mux,
        codex,
        match_executables,
        pane_commands,
        match_scope,
        match_command_regexes,
        pane_command_regexes,
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
    let mut command = vec![shell_word(&executables.mux)];
    command.extend(process_arguments(executables));
    command.extend([
        "--client".to_owned(),
        shell_format("client_tty"),
        "--invoking-pane".to_owned(),
        shell_format("pane_id"),
        "--invoking-session".to_owned(),
        shell_format("session_id"),
        "--invoking-path".to_owned(),
        shell_format("pane_current_path"),
    ]);
    let command = command.join(" ");
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
    let matches = executables
        .match_executables
        .iter()
        .map(|path| format!("{MATCH_FIELD}{}\n", path.display()))
        .collect::<String>();
    let pane_commands = executables
        .pane_commands
        .iter()
        .map(|command| format!("{PANE_COMMAND_FIELD}{command}\n"))
        .collect::<String>();
    let match_regexes = executables
        .match_command_regexes
        .iter()
        .map(|expression| format!("{MATCH_COMMAND_REGEX_FIELD}{expression}\n"))
        .collect::<String>();
    let pane_regexes = executables
        .pane_command_regexes
        .iter()
        .map(|expression| format!("{PANE_COMMAND_REGEX_FIELD}{expression}\n"))
        .collect::<String>();
    format!(
        "{BEGIN_MARKER}\n# Managed by codex-mux; changes inside this block are replaced.\n{LEADING_NEWLINE_FIELD}{owned_leading_newline}\n{KEY_FIELD}{key}\n{BINARY_FIELD}{}\n{CODEX_FIELD}{}\n{LAUNCH_FIELD}{}\n{MATCH_SCOPE_FIELD}{}\n{matches}{pane_commands}{match_regexes}{pane_regexes}{SMART_LEFT_FIELD}{smart_left}\nbind-key {key} run-shell -C {popup}\n{smart_binding}{END_MARKER}\n",
        executables.mux.display(),
        executables.codex.display(),
        executables.codex.display(),
        executables.match_scope,
    )
}

fn render_smart_left_binding(executables: &ExecutablePaths) -> String {
    let mut probe = vec![shell_word(&executables.mux)];
    probe.extend(process_arguments(executables));
    probe.extend([
        "--client".to_owned(),
        shell_format("client_tty"),
        "--invoking-pane".to_owned(),
        shell_format("pane_id"),
        "--invoking-session".to_owned(),
        shell_format("session_id"),
        "--invoking-path".to_owned(),
        shell_format("pane_current_path"),
        "smart-left".to_owned(),
    ]);
    let probe = probe.join(" ");
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
    let command_is_shell =
        "#{||:#{==:#{pane_current_command},bash},#{==:#{pane_current_command},zsh}}";
    let shell_is_at_prompt =
        format!("#{{&&:{command_is_shell},#{{==:#{{@codex_mux_shell_prompt}},1}}}}");
    let command_matches = executables
        .pane_commands
        .iter()
        .map(|command| format!("#{{==:#{{pane_current_command}},{command}}}"))
        .reduce(|left, right| format!("#{{||:{left},{right}}}"));
    // Regex prefiltering is performed by the Rust probe after this safe tmux
    // fast-path. Do not interpolate user regex syntax into tmux format strings.
    let command_matches = match (command_matches, executables.pane_command_regexes.is_empty()) {
        (Some(exact), true) => exact,
        (Some(exact), false) => format!("#{{||:{exact},1}}"),
        (None, false) => "1".to_owned(),
        (None, true) => "0".to_owned(),
    };
    let eligible = format!("#{{||:{command_matches},{shell_is_at_prompt}}}");
    let condition = format!("#{{&&:{eligible},#{{!=:#{{@codex_mux_smart_left_active}},1}}}}");
    format!(
        "bind-key -T root Left if-shell -F '{condition}' {{\n  set-option -p @codex_mux_smart_left_active 1\n  run-shell -b {}\n}} {{\n  send-keys Left\n}}\n",
        tmux_word(&shell)
    )
}

fn process_arguments(executables: &ExecutablePaths) -> Vec<String> {
    if executables.legacy_shorthand {
        return vec!["--codex".to_owned(), shell_word(&executables.codex)];
    }
    let mut arguments = vec![
        "--launch-executable".to_owned(),
        shell_word(&executables.codex),
    ];
    for executable in &executables.match_executables {
        arguments.push("--match-executable".to_owned());
        arguments.push(shell_word(executable));
    }
    for command in &executables.pane_commands {
        arguments.push("--pane-command".to_owned());
        arguments.push(command.clone());
    }
    arguments.push("--match-scope".to_owned());
    arguments.push(executables.match_scope.clone());
    for expression in &executables.match_command_regexes {
        arguments.push("--match-command-regex".to_owned());
        arguments.push(shell_literal(expression));
    }
    for expression in &executables.pane_command_regexes {
        arguments.push("--pane-command-regex".to_owned());
        arguments.push(shell_literal(expression));
    }
    arguments
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
    if executables.pane_commands.iter().any(|command| {
        !command
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._+-".contains(character))
    }) {
        return Err(InstallError::InvalidValue {
            field: "Smart Left pane command",
            reason: "must contain only ASCII letters, digits, dot, underscore, plus, or hyphen"
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

fn fields(block: &str, prefix: &str) -> Vec<String> {
    block
        .lines()
        .filter_map(|line| line.strip_prefix(prefix))
        .map(ToOwned::to_owned)
        .collect()
}

fn display_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

pub(crate) fn read(path: &Path) -> InstallResult<Vec<u8>> {
    fs::read(path).map_err(|source| InstallError::Filesystem {
        path: path.to_owned(),
        source,
    })
}

pub(crate) fn create_backup(path: &Path, bytes: &[u8], mode: u32) -> InstallResult<PathBuf> {
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

pub(crate) fn atomic_replace(path: &Path, bytes: &[u8], mode: u32) -> InstallResult<()> {
    atomic_replace_tracked(path, bytes, mode).map_err(AtomicReplaceFailure::into_error)
}

#[derive(Debug)]
pub(crate) struct AtomicReplaceFailure {
    error: InstallError,
    committed: bool,
}

impl AtomicReplaceFailure {
    pub(crate) const fn committed(&self) -> bool {
        self.committed
    }

    pub(crate) fn into_error(self) -> InstallError {
        self.error
    }
}

impl From<InstallError> for AtomicReplaceFailure {
    fn from(error: InstallError) -> Self {
        Self {
            error,
            committed: false,
        }
    }
}

pub(crate) fn atomic_replace_tracked(
    path: &Path,
    bytes: &[u8],
    mode: u32,
) -> Result<(), AtomicReplaceFailure> {
    atomic_replace_with(
        path,
        bytes,
        mode,
        |_| Ok(()),
        |parent| File::open(parent)?.sync_all(),
    )
}

pub(crate) fn atomic_replace_with(
    path: &Path,
    bytes: &[u8],
    mode: u32,
    before_rename: impl FnOnce(&Path) -> io::Result<()>,
    after_rename: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<(), AtomicReplaceFailure> {
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
    let mut committed = false;
    let mut created = false;
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        created = true;
        file.set_permissions(fs::Permissions::from_mode(mode & 0o7777))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        before_rename(&temporary)?;
        fs::rename(&temporary, path)?;
        committed = true;
        after_rename(parent)?;
        Ok::<(), io::Error>(())
    })();
    if let Err(source) = result {
        if created {
            let _ = fs::remove_file(&temporary);
        }
        return Err(AtomicReplaceFailure {
            error: InstallError::Filesystem {
                path: path.to_owned(),
                source,
            },
            committed,
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

    use super::{ExecutablePaths, ProcessMetadata, atomic_replace_with, process_arguments};

    #[test]
    fn regex_arguments_are_shell_literal_and_controls_are_refused() {
        let paths = ExecutablePaths::with_process(
            PathBuf::from("/opt/codex-mux"),
            PathBuf::from("/opt/launcher"),
            ProcessMetadata {
                match_executables: vec![PathBuf::from("/opt/launcher")],
                pane_commands: vec!["supervisor".to_owned()],
                match_scope: "pane-tree".to_owned(),
                match_command_regexes: vec!["x; touch /tmp/nope ' $(id) # space".to_owned()],
                pane_command_regexes: vec!["^super visor$".to_owned()],
            },
            false,
        )
        .unwrap();
        let arguments = process_arguments(&paths).join(" ");
        assert!(arguments.contains("'x; touch /tmp/nope '\\'' $(id) # space'"));
        assert!(arguments.contains("'^super visor$'"));
        assert!(
            ExecutablePaths::with_process(
                PathBuf::from("/opt/codex-mux"),
                PathBuf::from("/opt/launcher"),
                ProcessMetadata {
                    match_executables: vec![PathBuf::from("/opt/launcher")],
                    pane_commands: vec!["supervisor".to_owned()],
                    match_scope: "foreground".to_owned(),
                    match_command_regexes: vec!["bad\nvalue".to_owned()],
                    pane_command_regexes: Vec::new()
                },
                false,
            )
            .is_err()
        );
    }

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

        let error = atomic_replace_with(
            &target,
            b"replacement\n",
            0o600,
            |_| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected rename failure",
                ))
            },
            |_| Ok(()),
        )
        .unwrap_err();

        assert!(!error.committed());
        assert!(
            error
                .into_error()
                .to_string()
                .contains("injected rename failure")
        );
        assert_eq!(fs::read(&target).unwrap(), b"original bytes\n");
        let remaining = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<PathBuf>>();
        assert_eq!(remaining, vec![target]);
    }
}
