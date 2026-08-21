//! Transactional ownership of Codex's exact thread-ID terminal title setting.

use std::{
    env, fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use toml_edit::{Array, DocumentMut, Item, Value};

use crate::{MuxError, Result};

#[cfg(test)]
static FAIL_NEXT_PARENT_SYNC: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static SYNC_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(test)]
static MISMATCH_HOOK: std::sync::Mutex<
    Option<(
        std::sync::Arc<std::sync::Barrier>,
        std::sync::Arc<std::sync::Barrier>,
    )>,
> = std::sync::Mutex::new(None);
#[cfg(test)]
static POST_ROLLBACK_EXCHANGE_HOOK: std::sync::Mutex<
    Option<(
        std::sync::Arc<std::sync::Barrier>,
        std::sync::Arc<std::sync::Barrier>,
    )>,
> = std::sync::Mutex::new(None);

const OWNED_TITLE: &str = "thread-id";
const MAX_OWNED_FILE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
struct FileSnapshot {
    bytes: Vec<u8>,
    mode: u32,
    dev: u64,
    ino: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SavedState {
    config_path: PathBuf,
    previous: Option<Vec<String>>,
}

struct MutationLock(fs::File);

impl Drop for MutationLock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock);
    }
}

/// Rollback-capable Codex configuration edit prepared by `setup`.
pub(crate) struct CodexTitleTransaction {
    _lock: MutationLock,
    config_path: PathBuf,
    state_path: PathBuf,
    original_config: Option<FileSnapshot>,
    original_state: Option<FileSnapshot>,
    installed_config: FileSnapshot,
    installed_state: FileSnapshot,
    changed: bool,
}

impl CodexTitleTransaction {
    /// Installs the exact thread-ID title while retaining the prior supported value.
    pub(crate) fn install() -> Result<Self> {
        let config_path = codex_config_path()?;
        let state_path = state_path()?;
        let lock = lock_mutations(&state_path)?;
        let original_config = snapshot(&config_path)?;
        let original_state = snapshot(&state_path)?;
        let contents = match original_config.as_ref() {
            Some(snapshot) => String::from_utf8(snapshot.bytes.clone()).map_err(|_| {
                MuxError::Command(format!("{} is not valid UTF-8", config_path.display()))
            })?,
            None => String::new(),
        };
        let mut document = contents.parse::<DocumentMut>().map_err(|error| {
            MuxError::Command(format!(
                "could not parse {}: {error}",
                config_path.display()
            ))
        })?;
        let current = title_values(&document)?;
        let prior_state = original_state
            .as_ref()
            .map(|snapshot| parse_state(&state_path, &snapshot.bytes))
            .transpose()?;
        if let Some(state) = &prior_state {
            if state.config_path != config_path {
                return Err(MuxError::Command(
                    "Codex terminal-title ownership points at a different config file".to_owned(),
                ));
            }
            if current.as_deref() != Some(&[OWNED_TITLE.to_owned()]) {
                return Err(MuxError::Command(
                    "Codex terminal-title setting drifted; refusing to overwrite it".to_owned(),
                ));
            }
        }
        let previous = prior_state.map_or(current.clone(), |state| state.previous);
        document["tui"]["terminal_title"] = title_item(&[OWNED_TITLE.to_owned()]);
        let replacement = document.to_string();
        let changed = current.as_deref() != Some(&[OWNED_TITLE.to_owned()]);
        let state = toml::to_string(&SavedState {
            config_path: config_path.clone(),
            previous,
        })
        .map_err(|error| {
            MuxError::Command(format!(
                "could not serialize Codex integration state: {error}"
            ))
        })?;
        let installed_state = write_owned(
            &state_path,
            state.as_bytes(),
            0o600,
            original_state.as_ref(),
        )?;
        let installed_config = match write_owned(
            &config_path,
            replacement.as_bytes(),
            original_config
                .as_ref()
                .map_or(0o600, |snapshot| snapshot.mode),
            original_config.as_ref(),
        ) {
            Ok(installed) => installed,
            Err(error) => {
                restore(&state_path, original_state.as_ref(), Some(&installed_state))?;
                return Err(error);
            }
        };
        Ok(Self {
            _lock: lock,
            config_path,
            state_path,
            original_config,
            original_state,
            installed_config,
            installed_state,
            changed,
        })
    }

    /// Reports whether setup changed Codex's configuration bytes.
    pub(crate) const fn changed(&self) -> bool {
        self.changed
    }

    /// Restores both files after a later setup failure.
    pub(crate) fn rollback(&self) -> Result<()> {
        restore(
            &self.config_path,
            self.original_config.as_ref(),
            Some(&self.installed_config),
        )?;
        restore(
            &self.state_path,
            self.original_state.as_ref(),
            Some(&self.installed_state),
        )
    }
}

/// Restores the pre-setup title setting when the currently managed value is intact.
pub(crate) fn uninstall() -> Result<bool> {
    let state_path = state_path()?;
    let _lock = lock_mutations(&state_path)?;
    let Some(original_state) = snapshot(&state_path)? else {
        return Ok(false);
    };
    let state = parse_state(&state_path, &original_state.bytes)?;
    let original_config = snapshot(&state.config_path)?.ok_or_else(|| {
        MuxError::Command(format!(
            "managed Codex config {} is missing",
            state.config_path.display()
        ))
    })?;
    let contents = String::from_utf8(original_config.bytes.clone()).map_err(|_| {
        MuxError::Command(format!(
            "{} is not valid UTF-8",
            state.config_path.display()
        ))
    })?;
    let mut document = contents.parse::<DocumentMut>().map_err(|error| {
        MuxError::Command(format!(
            "could not parse {}: {error}",
            state.config_path.display()
        ))
    })?;
    if title_values(&document)?.as_deref() != Some(&[OWNED_TITLE.to_owned()]) {
        return Err(MuxError::Command(
            "Codex terminal-title setting drifted; refusing to overwrite it".to_owned(),
        ));
    }
    match state.previous {
        Some(previous) => document["tui"]["terminal_title"] = title_item(&previous),
        None => {
            document["tui"]
                .as_table_like_mut()
                .expect("tui table")
                .remove("terminal_title");
        }
    };
    let mode = original_config.mode;
    let restored_config = write_owned(
        &state.config_path,
        document.to_string().as_bytes(),
        mode,
        Some(&original_config),
    )?;
    if let Err(removal) = remove_owned(&state_path, Some(&original_state)) {
        if let Err(rollback) = restore(
            &state.config_path,
            Some(&original_config),
            Some(&restored_config),
        ) {
            return Err(MuxError::Command(format!(
                "{removal}; additionally failed to restore Codex config: {rollback}"
            )));
        }
        return Err(removal);
    }
    Ok(true)
}

fn lock_mutations(state_path: &Path) -> Result<MutationLock> {
    let parent = state_path.parent().ok_or_else(|| MuxError::InvalidValue {
        field: "Codex integration state path",
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
    rustix::fs::flock(&directory, rustix::fs::FlockOperation::LockExclusive).map_err(|source| {
        MuxError::Filesystem {
            path: parent.to_owned(),
            source: source.into(),
        }
    })?;
    Ok(MutationLock(directory))
}

/// Returns whether the installed exact-ID setting is present and free of drift.
pub(crate) fn status() -> Result<Option<bool>> {
    let Some(state_path) = optional_state_path() else {
        return Ok(None);
    };
    let Some(state_snapshot) = snapshot(&state_path)? else {
        return Ok(None);
    };
    let state = parse_state(&state_path, &state_snapshot.bytes)?;
    let config_snapshot = snapshot(&state.config_path)?.ok_or_else(|| {
        MuxError::Command(format!(
            "managed Codex config {} is missing",
            state.config_path.display()
        ))
    })?;
    let contents = String::from_utf8(config_snapshot.bytes).map_err(|_| {
        MuxError::Command(format!(
            "{} is not valid UTF-8",
            state.config_path.display()
        ))
    })?;
    let document = contents
        .parse::<DocumentMut>()
        .map_err(|error| MuxError::Command(format!("could not parse Codex config: {error}")))?;
    Ok(Some(
        title_values(&document)?.as_deref() == Some(&[OWNED_TITLE.to_owned()]),
    ))
}

fn title_values(document: &DocumentMut) -> Result<Option<Vec<String>>> {
    let Some(tui) = document.get("tui") else {
        return Ok(None);
    };
    let Some(table) = tui.as_table_like() else {
        return Err(MuxError::Command(
            "Codex tui setting must be a table".to_owned(),
        ));
    };
    let Some(item) = table.get("terminal_title") else {
        return Ok(None);
    };
    let Some(array) = item.as_array() else {
        return Err(MuxError::Command(
            "Codex tui.terminal_title must be an array of strings".to_owned(),
        ));
    };
    array
        .iter()
        .map(|item| {
            item.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                MuxError::Command("Codex tui.terminal_title must contain only strings".to_owned())
            })
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

fn title_item(values: &[String]) -> Item {
    let mut array = Array::new();
    values.iter().for_each(|entry| array.push(entry.as_str()));
    Item::Value(Value::Array(array))
}

fn parse_state(path: &Path, bytes: &[u8]) -> Result<SavedState> {
    let contents = std::str::from_utf8(bytes)
        .map_err(|_| MuxError::Command(format!("{} is not valid UTF-8", path.display())))?;
    toml::from_str(contents)
        .map_err(|error| MuxError::Command(format!("could not parse {}: {error}", path.display())))
}

fn snapshot(path: &Path) -> Result<Option<FileSnapshot>> {
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            if source.raw_os_error() == Some(40) {
                return Err(MuxError::Command(format!(
                    "{} must be a regular file and must not be a symlink",
                    path.display()
                )));
            }
            return Err(MuxError::Filesystem {
                path: path.to_owned(),
                source,
            });
        }
    };
    let metadata = file.metadata().map_err(|source| MuxError::Filesystem {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(MuxError::Command(format!(
            "{} must be a regular file and must not be a symlink",
            path.display()
        )));
    }
    if metadata.len() > MAX_OWNED_FILE_BYTES {
        return Err(MuxError::Command(format!(
            "{} exceeds the managed-file size limit",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_OWNED_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| MuxError::Filesystem {
            path: path.to_owned(),
            source,
        })?;
    if bytes.len() as u64 > MAX_OWNED_FILE_BYTES {
        return Err(MuxError::Command(format!(
            "{} grew beyond the managed-file size limit",
            path.display()
        )));
    }
    Ok(Some(FileSnapshot {
        bytes,
        mode: metadata.mode() & 0o777,
        dev: metadata.dev(),
        ino: metadata.ino(),
    }))
}

fn restore(
    path: &Path,
    original: Option<&FileSnapshot>,
    expected_current: Option<&FileSnapshot>,
) -> Result<()> {
    match original {
        Some(snapshot) => {
            write_owned(path, &snapshot.bytes, snapshot.mode, expected_current).map(|_| ())
        }
        None => remove_owned(path, expected_current),
    }
}

fn write_owned(
    path: &Path,
    bytes: &[u8],
    mode: u32,
    expected: Option<&FileSnapshot>,
) -> Result<FileSnapshot> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| MuxError::Filesystem {
            path: parent.to_owned(),
            source,
        })?;
    }
    let temporary = temporary_path(path, "replace")?;
    let mut committed = None::<FileSnapshot>;
    let mut prepared = None::<FileSnapshot>;
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source| MuxError::Filesystem {
                path: temporary.clone(),
                source,
            })?;
        file.set_permissions(fs::Permissions::from_mode(mode & 0o777))
            .map_err(|source| MuxError::Filesystem {
                path: temporary.clone(),
                source,
            })?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|source| MuxError::Filesystem {
                path: temporary.clone(),
                source,
            })?;
        let metadata = file.metadata().map_err(|source| MuxError::Filesystem {
            path: temporary.clone(),
            source,
        })?;
        let installed = FileSnapshot {
            bytes: bytes.to_vec(),
            mode: metadata.mode() & 0o777,
            dev: metadata.dev(),
            ino: metadata.ino(),
        };
        prepared = Some(installed.clone());
        if let Some(expected) = expected {
            exchange(&temporary, path)?;
            let displaced = snapshot(&temporary)?;
            if !same_snapshot(displaced.as_ref(), Some(expected)) {
                rollback_exchange(
                    &temporary,
                    path,
                    &installed,
                    displaced.as_ref().expect("exchange displaced a file"),
                )?;
                return Err(MuxError::Command(format!(
                    "{} changed concurrently; refusing to overwrite it",
                    path.display()
                )));
            }
            committed = Some(installed.clone());
            fs::remove_file(&temporary).map_err(|source| MuxError::Filesystem {
                path: temporary.clone(),
                source,
            })?;
        } else {
            rustix::fs::renameat_with(
                rustix::fs::CWD,
                &temporary,
                rustix::fs::CWD,
                path,
                rustix::fs::RenameFlags::NOREPLACE,
            )
            .map_err(|source| MuxError::Filesystem {
                path: path.to_owned(),
                source: source.into(),
            })?;
            committed = Some(installed.clone());
        }
        sync_parent(path)?;
        Ok(installed)
    })();
    match result {
        Ok(installed) => Ok(installed),
        Err(error) => {
            let rollback = committed
                .as_ref()
                .map(|installed| restore(path, expected, Some(installed)))
                .transpose();
            if let Some(prepared) = prepared.as_ref() {
                remove_if_snapshot(&temporary, prepared);
            }
            if let Err(rollback) = rollback {
                return Err(MuxError::Command(format!(
                    "{error}; additionally failed to roll back {}: {rollback}",
                    path.display()
                )));
            }
            Err(error)
        }
    }
}

fn remove_owned(path: &Path, expected: Option<&FileSnapshot>) -> Result<()> {
    let Some(expected) = expected else {
        return match snapshot(path)? {
            None => Ok(()),
            Some(_) => Err(MuxError::Command(format!(
                "{} appeared concurrently; refusing to remove it",
                path.display()
            ))),
        };
    };
    let tombstone = temporary_path(path, "remove")?;
    let grave = temporary_path(path, "removed")?;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tombstone)
        .and_then(|file| file.sync_all())
        .map_err(|source| MuxError::Filesystem {
            path: tombstone.clone(),
            source,
        })?;
    let tombstone_snapshot = snapshot(&tombstone)?.expect("new tombstone");
    let mut committed_current = None::<Option<FileSnapshot>>;
    let result = (|| {
        exchange(&tombstone, path)?;
        let displaced = snapshot(&tombstone)?;
        if !same_snapshot(displaced.as_ref(), Some(expected)) {
            if rollback_exchange(
                &tombstone,
                path,
                &tombstone_snapshot,
                displaced.as_ref().expect("exchange displaced a file"),
            )
            .is_ok()
            {
                remove_if_snapshot(&tombstone, &tombstone_snapshot);
            }
            return Err(MuxError::Command(format!(
                "{} changed concurrently; refusing to remove it",
                path.display()
            )));
        }
        committed_current = Some(Some(tombstone_snapshot.clone()));
        rustix::fs::renameat_with(
            rustix::fs::CWD,
            path,
            rustix::fs::CWD,
            &grave,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(|source| MuxError::Filesystem {
            path: path.to_owned(),
            source: source.into(),
        })?;
        committed_current = Some(None);
        let moved = snapshot(&grave)?;
        if !same_snapshot(moved.as_ref(), Some(&tombstone_snapshot)) {
            let _ = rustix::fs::renameat_with(
                rustix::fs::CWD,
                &grave,
                rustix::fs::CWD,
                path,
                rustix::fs::RenameFlags::NOREPLACE,
            );
            return Err(MuxError::Command(format!(
                "{} changed concurrently; refusing to remove it",
                path.display()
            )));
        }
        fs::remove_file(&grave).map_err(|source| MuxError::Filesystem {
            path: grave.clone(),
            source,
        })?;
        fs::remove_file(&tombstone).map_err(|source| MuxError::Filesystem {
            path: tombstone.clone(),
            source,
        })?;
        sync_parent(path)
    })();
    if let Err(error) = result {
        if let Some(current) = committed_current.as_ref() {
            if let Err(rollback) = restore(path, Some(expected), current.as_ref()) {
                return Err(MuxError::Command(format!(
                    "{error}; additionally failed to roll back {}: {rollback}",
                    path.display()
                )));
            }
        }
        remove_if_snapshot(&grave, &tombstone_snapshot);
        remove_if_snapshot(&tombstone, expected);
        remove_if_snapshot(&tombstone, &tombstone_snapshot);
        return Err(error);
    }
    Ok(())
}

fn remove_if_snapshot(path: &Path, expected: &FileSnapshot) {
    if snapshot(path)
        .ok()
        .flatten()
        .as_ref()
        .is_some_and(|current| same_snapshot(Some(current), Some(expected)))
    {
        let _ = fs::remove_file(path);
    }
}

fn rollback_exchange(
    temporary: &Path,
    path: &Path,
    installed: &FileSnapshot,
    restore_target: &FileSnapshot,
) -> Result<()> {
    if !same_snapshot(snapshot(path)?.as_ref(), Some(installed)) {
        return Err(MuxError::Command(format!(
            "{} changed during rollback; preserved the newer public file",
            path.display()
        )));
    }
    test_mismatch_hook();
    exchange(temporary, path)?;
    test_post_rollback_exchange_hook();
    let displaced = snapshot(temporary)?;
    if same_snapshot(displaced.as_ref(), Some(installed)) {
        return Ok(());
    }
    let mut expected_public = restore_target.clone();
    for _ in 0..16 {
        if !same_snapshot(snapshot(path)?.as_ref(), Some(&expected_public)) {
            return Err(MuxError::Command(format!(
                "{} changed during rollback; preserved the newer public file",
                path.display()
            )));
        }
        exchange(temporary, path)?;
        let newly_displaced = snapshot(temporary)?;
        if same_snapshot(newly_displaced.as_ref(), Some(&expected_public)) {
            return Err(MuxError::Command(format!(
                "{} changed during rollback; restored the newer public file",
                path.display()
            )));
        }
        expected_public = snapshot(path)?.expect("exchange installed a file");
    }
    Err(MuxError::Command(format!(
        "{} kept changing during rollback; preserved every displaced file",
        path.display()
    )))
}

#[cfg(test)]
fn test_mismatch_hook() {
    let hook = MISMATCH_HOOK.lock().unwrap().clone();
    if let Some((reached, release)) = hook {
        reached.wait();
        release.wait();
    }
}

#[cfg(test)]
fn test_post_rollback_exchange_hook() {
    let hook = POST_ROLLBACK_EXCHANGE_HOOK.lock().unwrap().clone();
    if let Some((reached, release)) = hook {
        reached.wait();
        release.wait();
    }
}

#[cfg(not(test))]
fn test_mismatch_hook() {}
#[cfg(not(test))]
fn test_post_rollback_exchange_hook() {}

fn same_snapshot(left: Option<&FileSnapshot>, right: Option<&FileSnapshot>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.dev == right.dev
                && left.ino == right.ino
                && left.mode == right.mode
                && left.bytes == right.bytes
        }
        _ => false,
    }
}

fn exchange(left: &Path, right: &Path) -> Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        left,
        rustix::fs::CWD,
        right,
        rustix::fs::RenameFlags::EXCHANGE,
    )
    .map_err(|source| MuxError::Filesystem {
        path: right.to_owned(),
        source: source.into(),
    })
}

fn temporary_path(path: &Path, role: &str) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| MuxError::Command("managed path has no parent".to_owned()))?;
    let name = path
        .file_name()
        .ok_or_else(|| MuxError::Command("managed path has no file name".to_owned()))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(parent.join(format!(
        ".{}.codex-mux-{role}-{}-{nonce}",
        name.to_string_lossy(),
        std::process::id()
    )))
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| MuxError::Command("managed path has no parent".to_owned()))?;
    #[cfg(test)]
    if FAIL_NEXT_PARENT_SYNC.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return Err(MuxError::Filesystem {
            path: parent.to_owned(),
            source: std::io::Error::other("injected parent sync failure"),
        });
    }
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| MuxError::Filesystem {
            path: parent.to_owned(),
            source,
        })
}

fn codex_config_path() -> Result<PathBuf> {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .map(|home| home.join("config.toml"))
        .ok_or_else(|| MuxError::Command("Codex home is unavailable".to_owned()))
}

fn state_path() -> Result<PathBuf> {
    optional_state_path().ok_or_else(|| MuxError::Command("state home is unavailable".to_owned()))
}

fn optional_state_path() -> Option<PathBuf> {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .map(|root| root.join("codex-mux/codex-terminal-title.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditional_replace_and_remove_preserve_concurrent_changes() {
        let _serial = SYNC_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "codex-mux-codex-config-cas-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("config.toml");
        fs::write(&path, b"original").unwrap();
        let original = snapshot(&path).unwrap().unwrap();
        fs::write(&path, b"concurrent").unwrap();
        assert!(write_owned(&path, b"replacement", 0o600, Some(&original)).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"concurrent");
        assert!(remove_owned(&path, Some(&original)).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"concurrent");

        let absent = root.join("state.toml");
        fs::write(&absent, b"new owner").unwrap();
        assert!(write_owned(&absent, b"ours", 0o600, None).is_err());
        assert!(remove_owned(&absent, None).is_err());
        assert_eq!(fs::read(&absent).unwrap(), b"new owner");
        assert!(fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("codex-mux-")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_lock_serializes_two_removers_without_temporary_artifacts() {
        let _serial = SYNC_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "codex-mux-codex-config-lock-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("state.toml");
        fs::write(&path, b"owned").unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let removers = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let _lock = lock_mutations(&path).unwrap();
                    if let Some(current) = snapshot(&path).unwrap() {
                        remove_owned(&path, Some(&current)).unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        for remover in removers {
            remover.join().unwrap();
        }
        assert!(!path.exists());
        assert!(fs::read_dir(&root).unwrap().next().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn post_commit_sync_failures_restore_replaced_and_removed_paths() {
        let _serial = SYNC_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "codex-mux-codex-config-commit-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("state.toml");
        fs::write(&path, b"original").unwrap();
        let original = snapshot(&path).unwrap().unwrap();

        FAIL_NEXT_PARENT_SYNC.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(write_owned(&path, b"replacement", 0o600, Some(&original)).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"original");

        let restored = snapshot(&path).unwrap().unwrap();
        FAIL_NEXT_PARENT_SYNC.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(remove_owned(&path, Some(&restored)).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert!(fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("codex-mux-")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mismatch_rollback_never_deletes_a_later_external_edit() {
        let _serial = SYNC_TEST_LOCK.lock().unwrap();
        for operation in ["replace", "remove"] {
            let root = std::env::temp_dir().join(format!(
                "codex-mux-codex-config-external-{operation}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&root).unwrap();
            let path = root.join("state.toml");
            fs::write(&path, b"original").unwrap();
            let expected = snapshot(&path).unwrap().unwrap();
            fs::write(&path, b"conflicting editor bytes").unwrap();
            let reached = std::sync::Arc::new(std::sync::Barrier::new(2));
            let release = std::sync::Arc::new(std::sync::Barrier::new(2));
            let post_reached = std::sync::Arc::new(std::sync::Barrier::new(2));
            let post_release = std::sync::Arc::new(std::sync::Barrier::new(2));
            *MISMATCH_HOOK.lock().unwrap() = Some((reached.clone(), release.clone()));
            *POST_ROLLBACK_EXCHANGE_HOOK.lock().unwrap() =
                Some((post_reached.clone(), post_release.clone()));
            let worker_path = path.clone();
            let worker = std::thread::spawn(move || {
                if operation == "replace" {
                    write_owned(&worker_path, b"ours", 0o600, Some(&expected)).map(|_| ())
                } else {
                    remove_owned(&worker_path, Some(&expected))
                }
            });
            reached.wait();
            let external = root.join("external-editor.tmp");
            fs::write(&external, b"first external edit").unwrap();
            fs::rename(&external, &path).unwrap();
            release.wait();
            post_reached.wait();
            let newest = root.join("newest-editor.tmp");
            fs::write(&newest, b"newest external edit").unwrap();
            fs::rename(&newest, &path).unwrap();
            post_release.wait();
            assert!(worker.join().unwrap().is_err());
            *MISMATCH_HOOK.lock().unwrap() = None;
            *POST_ROLLBACK_EXCHANGE_HOOK.lock().unwrap() = None;
            assert_eq!(fs::read(&path).unwrap(), b"newest external edit");
            assert!(fs::read_dir(&root).unwrap().any(|entry| {
                let entry = entry.unwrap();
                entry.path() != path
                    && fs::read(entry.path()).ok().as_deref() == Some(b"first external edit")
            }));
            fs::remove_dir_all(root).unwrap();
        }
    }
}
