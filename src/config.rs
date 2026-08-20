//! Theme preference persistence owned by `codex-mux`.

use std::{
    env, fs,
    fs::OpenOptions,
    io::Write,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{
    MuxError, Result,
    domain::{ThemeId, ThemeStore},
};

/// Permission behavior applied when a launch profile starts Codex.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionPreset {
    /// Preserve Codex's normal sandbox and approval behavior.
    #[default]
    Standard,
    /// Disable the sandbox and approval prompts through Codex's supported flag.
    Yolo,
}

/// Reusable new-session launch configuration selected by a single key.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchProfile {
    /// Human-readable picker label.
    pub name: String,
    /// One-character key used after entering new-session mode.
    pub key: char,
    /// Legacy profile executable retained as an additional discovery identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<PathBuf>,
    /// Permission behavior for this profile.
    #[serde(default)]
    pub permissions: PermissionPreset,
}

impl LaunchProfile {
    /// Returns the first-run Standard profile.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            name: "standard".to_owned(),
            key: 's',
            executable: None,
            permissions: PermissionPreset::Standard,
        }
    }

    /// Returns the first-run unrestricted profile.
    #[must_use]
    pub fn yolo() -> Self {
        Self {
            name: "yolo".to_owned(),
            key: 'y',
            executable: None,
            permissions: PermissionPreset::Yolo,
        }
    }
}

fn default_profiles() -> Vec<LaunchProfile> {
    vec![LaunchProfile::standard(), LaunchProfile::yolo()]
}

/// Theme preference loaded for one invocation, including a recoverable warning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThemePreference {
    /// Persisted theme, or the first-run default when none could be loaded.
    pub selected: ThemeId,
    /// Explanation of a malformed or unreadable preference.
    pub warning: Option<String>,
    /// Whether `selected` came from a valid persisted preference.
    pub was_saved: bool,
    /// Persisted launch profiles, or safe first-run defaults.
    pub profiles: Vec<LaunchProfile>,
    /// Whether conversation-aware Luna naming is explicitly enabled.
    pub smart_naming: bool,
    /// Explicit process launch and discovery configuration, when configured.
    pub process: Option<ProcessSettings>,
}

/// Persisted separation between launching Codex and recognizing its processes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessSettings {
    /// Executable used for every new Codex and app-server process.
    pub launch_executable: PathBuf,
    /// Executable or interpreted-script paths accepted during discovery.
    pub match_executables: Vec<PathBuf>,
    /// Exact tmux `pane_current_command` values accepted by Smart Left.
    pub pane_commands: Vec<String>,
    /// Candidate scope used for executable and command matching.
    #[serde(default)]
    pub match_scope: MatchScope,
    /// Regexes matched against a shell-free normalized process argv.
    #[serde(default)]
    pub match_command_regexes: Vec<String>,
    /// Regexes matched against tmux pane_current_command.
    #[serde(default)]
    pub pane_command_regexes: Vec<String>,
}

/// Candidate process scope used for wrapper-aware detection.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MatchScope {
    /// Preserve the pane foreground process-group behavior.
    #[default]
    Foreground,
    /// Search readable descendants of the pane process.
    PaneTree,
    /// Search readable processes attached to the pane TTY.
    PaneTty,
}

impl std::str::FromStr for MatchScope {
    type Err = MuxError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "foreground" => Ok(Self::Foreground),
            "pane-tree" => Ok(Self::PaneTree),
            "pane-tty" => Ok(Self::PaneTty),
            _ => Err(MuxError::InvalidValue {
                field: "process match scope",
                message: "must be foreground, pane-tree, or pane-tty".to_owned(),
            }),
        }
    }
}

impl std::fmt::Display for MatchScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Foreground => "foreground",
            Self::PaneTree => "pane-tree",
            Self::PaneTty => "pane-tty",
        }
        .fmt(formatter)
    }
}

impl ThemePreference {
    /// Returns the theme effective for this invocation.
    ///
    /// `NO_COLOR` deliberately does not change the persisted selection.
    #[must_use]
    pub const fn effective_theme(&self, no_color: bool) -> ThemeId {
        if no_color {
            ThemeId::Monochrome
        } else {
            self.selected
        }
    }

    /// Returns the effective theme after inspecting `NO_COLOR` for this invocation.
    #[must_use]
    pub fn effective_for_environment(&self) -> ThemeId {
        self.effective_theme(no_color_requested())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    theme: ThemeId,
    #[serde(default = "default_profiles")]
    profiles: Vec<LaunchProfile>,
    #[serde(default)]
    smart_naming: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process: Option<ProcessSettings>,
}

/// Filesystem-backed theme store using an XDG configuration path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XdgThemeStore {
    path: PathBuf,
}

struct ConfigLock(fs::File);

impl Drop for ConfigLock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock);
    }
}

impl XdgThemeStore {
    /// Constructs a store at the default XDG configuration path.
    pub fn discover() -> Result<Self> {
        Ok(Self::at(default_config_path()?))
    }

    /// Constructs a store at an explicit path, primarily for embedding and tests.
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the exact configuration file managed by this store.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads a preference, falling back with a warning for malformed or unreadable files.
    #[must_use]
    pub fn load_preference(&self) -> ThemePreference {
        match self.load_config() {
            Ok(Some(config)) => ThemePreference {
                selected: config.theme,
                warning: None,
                was_saved: true,
                profiles: config.profiles,
                smart_naming: config.smart_naming,
                process: config.process,
            },
            Ok(None) => ThemePreference {
                selected: ThemeId::default(),
                warning: None,
                was_saved: false,
                profiles: default_profiles(),
                smart_naming: false,
                process: None,
            },
            Err(error) => ThemePreference {
                selected: ThemeId::default(),
                warning: Some(format!(
                    "could not load configuration at {}: {error}; using {}",
                    self.path.display(),
                    ThemeId::default()
                )),
                was_saved: false,
                profiles: default_profiles(),
                smart_naming: false,
                process: None,
            },
        }
    }

    fn load_config(&self) -> Result<Option<ConfigFile>> {
        let config = self.load_parsed_config()?;
        if let Some(config) = &config {
            validate_profiles(&config.profiles)?;
            if let Some(process) = &config.process {
                validate_process_settings(process)?;
            }
        }
        Ok(config)
    }

    fn load_parsed_config(&self) -> Result<Option<ConfigFile>> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => {
                let config = toml::from_str::<ConfigFile>(&contents).map_err(|error| {
                    MuxError::Command(format!("could not parse {}: {error}", self.path.display()))
                })?;
                Ok(Some(config))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(MuxError::Filesystem {
                path: self.path.clone(),
                source,
            }),
        }
    }

    /// Atomically persists launch profiles while retaining the selected theme.
    pub fn save_profiles(&self, profiles: &[LaunchProfile]) -> Result<()> {
        validate_profiles(profiles)?;
        let _lock = self.lock_parent()?;
        let config = self.load_parsed_config()?;
        let theme = config
            .as_ref()
            .map_or_else(ThemeId::default, |config| config.theme);
        let smart_naming = config.as_ref().is_some_and(|config| config.smart_naming);
        let process = config.and_then(|config| config.process);
        self.save_atomic(theme, profiles, smart_naming, process)
    }

    /// Atomically persists the explicit conversation-aware naming preference.
    pub fn save_smart_naming(&self, enabled: bool) -> Result<()> {
        let _lock = self.lock_parent()?;
        let config = self.load_config()?;
        let theme = config
            .as_ref()
            .map_or_else(ThemeId::default, |value| value.theme);
        let profiles = config.map_or_else(default_profiles, |value| value.profiles);
        let process = self.load_parsed_config()?.and_then(|value| value.process);
        self.save_atomic(theme, &profiles, enabled, process)
    }

    /// Atomically persists process launch/detection settings without changing
    /// theme, profiles, or Smart Naming.
    pub fn save_process(&self, process: ProcessSettings) -> Result<()> {
        validate_process_settings(&process)?;
        let _lock = self.lock_parent()?;
        let config = self.load_config()?;
        let theme = config
            .as_ref()
            .map_or_else(ThemeId::default, |value| value.theme);
        let profiles = config
            .as_ref()
            .map_or_else(default_profiles, |value| value.profiles.clone());
        let smart_naming = config.as_ref().is_some_and(|value| value.smart_naming);
        self.save_atomic(theme, &profiles, smart_naming, Some(process))
    }

    fn lock_parent(&self) -> Result<ConfigLock> {
        let parent = self.path.parent().ok_or_else(|| MuxError::InvalidValue {
            field: "theme configuration path",
            message: "must have a parent directory".to_owned(),
        })?;
        fs::create_dir_all(parent).map_err(|source| MuxError::Filesystem {
            path: parent.to_owned(),
            source,
        })?;
        let directory = fs::File::open(parent).map_err(|source| MuxError::Filesystem {
            path: parent.to_owned(),
            source,
        })?;
        rustix::fs::flock(&directory, rustix::fs::FlockOperation::LockExclusive).map_err(
            |source| MuxError::Filesystem {
                path: parent.to_owned(),
                source: source.into(),
            },
        )?;
        Ok(ConfigLock(directory))
    }

    fn save_atomic(
        &self,
        theme: ThemeId,
        profiles: &[LaunchProfile],
        smart_naming: bool,
        process: Option<ProcessSettings>,
    ) -> Result<()> {
        let parent = self.path.parent().ok_or_else(|| MuxError::InvalidValue {
            field: "theme configuration path",
            message: "must have a parent directory".to_owned(),
        })?;
        fs::create_dir_all(parent).map_err(|source| MuxError::Filesystem {
            path: parent.to_owned(),
            source,
        })?;

        let contents = toml::to_string_pretty(&ConfigFile {
            theme,
            profiles: profiles.to_vec(),
            smart_naming,
            process,
        })
        .map_err(|error| {
            MuxError::Command(format!("could not serialize configuration: {error}"))
        })?;
        let temporary = self.temporary_path();
        let write_result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&temporary)
                .map_err(|source| MuxError::Filesystem {
                    path: temporary.clone(),
                    source,
                })?;
            file.write_all(contents.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|source| MuxError::Filesystem {
                    path: temporary.clone(),
                    source,
                })?;
            fs::rename(&temporary, &self.path).map_err(|source| MuxError::Filesystem {
                path: self.path.clone(),
                source,
            })?;
            if let Ok(directory) = fs::File::open(parent) {
                let _ = directory.sync_all();
            }
            Ok(())
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }

    fn temporary_path(&self) -> PathBuf {
        static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.toml");
        self.path.with_file_name(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ))
    }
}

/// Reports whether the standard per-process color opt-out is present and non-empty.
#[must_use]
pub fn no_color_requested() -> bool {
    env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
}

impl ThemeStore for XdgThemeStore {
    fn load(&self) -> Result<Option<ThemeId>> {
        let preference = self.load_preference();
        Ok(preference.was_saved.then_some(preference.selected))
    }

    fn save(&self, theme: ThemeId) -> Result<()> {
        let _lock = self.lock_parent()?;
        let profiles = self.load_config()?;
        let smart_naming = profiles.as_ref().is_some_and(|config| config.smart_naming);
        let process = profiles.as_ref().and_then(|config| config.process.clone());
        let profiles = profiles.map_or_else(default_profiles, |config| config.profiles);
        self.save_atomic(theme, &profiles, smart_naming, process)
    }
}

/// Validates a complete profile set before it can be persisted or activated.
pub fn validate_profiles(profiles: &[LaunchProfile]) -> Result<()> {
    if profiles.is_empty() {
        return Err(MuxError::InvalidValue {
            field: "launch profiles",
            message: "must contain at least one profile".to_owned(),
        });
    }
    let mut keys = std::collections::BTreeSet::new();
    for profile in profiles {
        if profile.name.trim().is_empty() {
            return Err(MuxError::InvalidValue {
                field: "profile name",
                message: "must not be empty".to_owned(),
            });
        }
        if profile.name.chars().any(char::is_control) {
            return Err(MuxError::InvalidValue {
                field: "profile name",
                message: "must not contain control characters".to_owned(),
            });
        }
        let key = profile.key.to_ascii_lowercase();
        if !key.is_ascii_alphanumeric() || matches!(key, 'a' | 'e' | 'j' | 'k' | 'n' | 'q') {
            return Err(MuxError::InvalidValue {
                field: "profile key",
                message: format!("{key:?} is reserved or unsupported"),
            });
        }
        if !keys.insert(key) {
            return Err(MuxError::InvalidValue {
                field: "profile key",
                message: format!("{key:?} is assigned more than once"),
            });
        }
        if let Some(executable) = &profile.executable {
            if !executable.is_absolute() {
                return Err(MuxError::InvalidValue {
                    field: "profile executable",
                    message: "must be an absolute path".to_owned(),
                });
            }
            let metadata = fs::metadata(executable).map_err(|source| MuxError::Filesystem {
                path: executable.clone(),
                source,
            })?;
            #[cfg(unix)]
            if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
                return Err(MuxError::InvalidValue {
                    field: "profile executable",
                    message: format!("{} is not executable", executable.display()),
                });
            }
        }
    }
    Ok(())
}

/// Validates persisted process paths and exact tmux command names.
pub fn validate_process_settings(settings: &ProcessSettings) -> Result<()> {
    validate_executable_path(&settings.launch_executable, "process launch executable")?;
    if settings.match_executables.is_empty() {
        return Err(MuxError::InvalidValue {
            field: "process match executables",
            message: "must contain at least one executable".to_owned(),
        });
    }
    for path in &settings.match_executables {
        validate_executable_path(path, "process match executable")?;
    }
    if settings.pane_commands.is_empty() && settings.pane_command_regexes.is_empty() {
        return Err(MuxError::InvalidValue {
            field: "process pane commands",
            message: "must contain at least one command".to_owned(),
        });
    }
    for command in &settings.pane_commands {
        if command.is_empty()
            || !command
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "._+-".contains(character))
        {
            return Err(MuxError::InvalidValue {
                field: "process pane command",
                message: format!("{command:?} must be one exact safe command name"),
            });
        }
    }
    validate_regexes(
        &settings.match_command_regexes,
        "process match command regex",
    )?;
    validate_regexes(&settings.pane_command_regexes, "process pane command regex")?;
    Ok(())
}

fn validate_regexes(regexes: &[String], field: &'static str) -> Result<()> {
    for expression in regexes {
        if expression.is_empty() || expression.chars().any(char::is_control) {
            return Err(MuxError::InvalidValue {
                field,
                message: "must not be empty or contain control characters".to_owned(),
            });
        }
        let regex = Regex::new(expression).map_err(|error| MuxError::InvalidValue {
            field,
            message: format!("invalid regex {expression:?}: {error}"),
        })?;
        if regex.is_match("") {
            return Err(MuxError::InvalidValue {
                field,
                message: format!("{expression:?} must not match an empty command"),
            });
        }
    }
    Ok(())
}

fn validate_executable_path(path: &Path, field: &'static str) -> Result<()> {
    if !path.is_absolute() {
        return Err(MuxError::InvalidValue {
            field,
            message: "must be an absolute path".to_owned(),
        });
    }
    let metadata = fs::metadata(path).map_err(|source| MuxError::Filesystem {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err(MuxError::InvalidValue {
            field,
            message: format!("{} is not executable", path.display()),
        });
    }
    Ok(())
}

fn default_config_path() -> Result<PathBuf> {
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(config_home)
            .join("codex-mux")
            .join("config.toml"));
    }
    if let Some(home) = env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home)
            .join(".config")
            .join("codex-mux")
            .join("config.toml"));
    }
    Err(MuxError::InvalidValue {
        field: "configuration environment",
        message: "neither XDG_CONFIG_HOME nor HOME is set".to_owned(),
    })
}
