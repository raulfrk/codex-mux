//! Theme preference persistence owned by `codex-mux`.

use std::{
    env, fs,
    fs::OpenOptions,
    io::Write,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

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
    /// Optional executable override; `None` uses the CLI-configured Codex binary.
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
            },
            Ok(None) => ThemePreference {
                selected: ThemeId::default(),
                warning: None,
                was_saved: false,
                profiles: default_profiles(),
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
            },
        }
    }

    fn load_config(&self) -> Result<Option<ConfigFile>> {
        let config = self.load_parsed_config()?;
        if let Some(config) = &config {
            validate_profiles(&config.profiles)?;
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
        let theme = self
            .load_parsed_config()?
            .map_or_else(ThemeId::default, |config| config.theme);
        self.save_atomic(theme, profiles)
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

    fn save_atomic(&self, theme: ThemeId, profiles: &[LaunchProfile]) -> Result<()> {
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
        let profiles = self
            .load_config()?
            .map_or_else(default_profiles, |config| config.profiles);
        self.save_atomic(theme, &profiles)
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
