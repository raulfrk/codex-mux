//! Theme preference persistence owned by `codex-mux`.

use std::{
    env, fs,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::{
    MuxError, Result,
    domain::{ThemeId, ThemeStore},
};

/// Theme preference loaded for one invocation, including a recoverable warning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThemePreference {
    /// Persisted theme, or the first-run default when none could be loaded.
    pub selected: ThemeId,
    /// Explanation of a malformed or unreadable preference.
    pub warning: Option<String>,
    /// Whether `selected` came from a valid persisted preference.
    pub was_saved: bool,
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
    theme: ThemeId,
}

/// Filesystem-backed theme store using an XDG configuration path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XdgThemeStore {
    path: PathBuf,
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
        match fs::read_to_string(&self.path) {
            Ok(contents) => match toml::from_str::<ConfigFile>(&contents) {
                Ok(config) => ThemePreference {
                    selected: config.theme,
                    warning: None,
                    was_saved: true,
                },
                Err(error) => ThemePreference {
                    selected: ThemeId::default(),
                    warning: Some(format!(
                        "could not parse theme preference at {}: {error}; using {}",
                        self.path.display(),
                        ThemeId::default()
                    )),
                    was_saved: false,
                },
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ThemePreference {
                selected: ThemeId::default(),
                warning: None,
                was_saved: false,
            },
            Err(error) => ThemePreference {
                selected: ThemeId::default(),
                warning: Some(format!(
                    "could not read theme preference at {}: {error}; using {}",
                    self.path.display(),
                    ThemeId::default()
                )),
                was_saved: false,
            },
        }
    }

    fn save_atomic(&self, theme: ThemeId) -> Result<()> {
        let parent = self.path.parent().ok_or_else(|| MuxError::InvalidValue {
            field: "theme configuration path",
            message: "must have a parent directory".to_owned(),
        })?;
        fs::create_dir_all(parent).map_err(|source| MuxError::Filesystem {
            path: parent.to_owned(),
            source,
        })?;

        let contents = toml::to_string_pretty(&ConfigFile { theme }).map_err(|error| {
            MuxError::Command(format!("could not serialize theme preference: {error}"))
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
        self.save_atomic(theme)
    }
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
