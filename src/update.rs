//! Verified release discovery and atomic self-update support.

use std::{
    env, fs,
    io::{Cursor, Read},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Duration,
};

use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{MuxError, Result, install::atomic_replace_with};

const API_BASE: &str = "https://api.github.com/repos/raulfrk/codex-mux/releases";
const DOWNLOAD_PREFIX: &str = "https://github.com/raulfrk/codex-mux/releases/download/";
const MAX_METADATA_BYTES: usize = 1024 * 1024;
const MAX_CHECKSUM_BYTES: usize = 64 * 1024;
const MAX_ARCHIVE_BYTES: usize = 32 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 32 * 1024 * 1024;
const MAX_DECOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Observable result of a self-update request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateOutcome {
    /// The latest stable release was already running, so nothing was downloaded.
    AlreadyCurrent(Version),
    /// The requested release was verified and atomically installed.
    Installed(Version),
}

/// Updates the currently running executable from the official GitHub release.
pub fn update(requested: Option<&str>) -> Result<UpdateOutcome> {
    let target = env::current_exe().map_err(|source| MuxError::Filesystem {
        path: PathBuf::from("current executable"),
        source,
    })?;
    Updater::official().update_at(requested, &target)
}

#[derive(Clone, Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Clone, Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
}

trait HttpClient {
    fn get(&self, url: &str, limit: usize) -> Result<Vec<u8>>;
}

struct UreqClient {
    agent: ureq::Agent,
}

impl UreqClient {
    fn new(https_only: bool) -> Self {
        Self::with_timeout(https_only, REQUEST_TIMEOUT)
    }

    fn with_timeout(https_only: bool, timeout: Duration) -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .https_only(https_only)
                .timeout(timeout)
                .build(),
        }
    }
}

impl HttpClient for UreqClient {
    fn get(&self, url: &str, limit: usize) -> Result<Vec<u8>> {
        let response = self
            .agent
            .get(url)
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28")
            .set(
                "User-Agent",
                concat!("codex-mux/", env!("CARGO_PKG_VERSION")),
            )
            .call()
            .map_err(|error| update_error(format!("request failed for {url}: {error}")))?;
        if response
            .header("Content-Length")
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|length| length > limit)
        {
            return Err(update_error(format!(
                "response for {url} exceeds the {limit}-byte limit"
            )));
        }
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(limit.saturating_add(1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| update_error(format!("could not read {url}: {error}")))?;
        if bytes.len() > limit {
            return Err(update_error(format!(
                "response for {url} exceeds the {limit}-byte limit"
            )));
        }
        Ok(bytes)
    }
}

struct Updater<C = UreqClient> {
    client: C,
    api_base: String,
    download_prefix: String,
}

impl Updater<UreqClient> {
    fn official() -> Self {
        Self {
            client: UreqClient::new(true),
            api_base: API_BASE.to_owned(),
            download_prefix: DOWNLOAD_PREFIX.to_owned(),
        }
    }
}

impl<C: HttpClient> Updater<C> {
    fn update_at(&self, requested: Option<&str>, target: &Path) -> Result<UpdateOutcome> {
        let requested = requested.map(parse_version).transpose()?;
        let release_url = requested.as_ref().map_or_else(
            || format!("{}/latest", self.api_base),
            |version| format!("{}/tags/v{version}", self.api_base),
        );
        let metadata = self.client.get(&release_url, MAX_METADATA_BYTES)?;
        let release: Release = serde_json::from_slice(&metadata)
            .map_err(|error| update_error(format!("invalid GitHub release metadata: {error}")))?;
        let version = release
            .tag_name
            .strip_prefix('v')
            .ok_or_else(|| update_error("release tag does not start with v"))
            .and_then(parse_version)?;
        if requested
            .as_ref()
            .is_some_and(|requested| requested != &version)
        {
            return Err(update_error(format!(
                "GitHub returned v{version} for requested v{}",
                requested.expect("checked above")
            )));
        }
        let current = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        if requested.is_none() && version <= current {
            return Ok(UpdateOutcome::AlreadyCurrent(current));
        }

        let target_triple = host_target()?;
        let archive_name = format!("codex-mux-{version}-{target_triple}.tar.gz");
        let archive = exact_asset(&release.assets, &archive_name)?;
        let checksums = exact_asset(&release.assets, "SHA256SUMS")?;
        validate_asset_url(archive, &self.download_prefix, &release.tag_name)?;
        validate_asset_url(checksums, &self.download_prefix, &release.tag_name)?;
        validate_asset_size(archive, MAX_ARCHIVE_BYTES)?;
        validate_asset_size(checksums, MAX_CHECKSUM_BYTES)?;

        let checksum_bytes = self
            .client
            .get(&checksums.browser_download_url, MAX_CHECKSUM_BYTES)?;
        validate_downloaded_size(checksums, checksum_bytes.len())?;
        let expected = checksum_for(&checksum_bytes, &archive_name)?;
        let archive_bytes = self
            .client
            .get(&archive.browser_download_url, MAX_ARCHIVE_BYTES)?;
        validate_downloaded_size(archive, archive_bytes.len())?;
        let actual = format!("{:x}", Sha256::digest(&archive_bytes));
        if actual != expected {
            return Err(update_error(format!(
                "checksum mismatch for {archive_name}: expected {expected}, got {actual}"
            )));
        }
        let binary = extract_binary(&archive_bytes, &version, target_triple)?;
        let identity = validate_install_target(target)?;
        atomic_replace_with(
            target,
            &binary,
            identity.mode,
            |temporary| validate_unchanged_target(target, temporary, &identity),
            |parent| fs::File::open(parent)?.sync_all(),
        )
        .map_err(|failure| {
            if failure.committed() {
                update_error("binary was replaced but its directory could not be synchronized")
            } else {
                update_error(format!(
                    "could not replace {}: {}",
                    target.display(),
                    failure.into_error()
                ))
            }
        })?;
        Ok(UpdateOutcome::Installed(version))
    }
}

fn parse_version(value: &str) -> Result<Version> {
    let normalized = value.strip_prefix('v').unwrap_or(value);
    let version = Version::parse(normalized)
        .map_err(|error| update_error(format!("invalid release version {value:?}: {error}")))?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        return Err(update_error(
            "release version must be a stable MAJOR.MINOR.PATCH version",
        ));
    }
    Ok(version)
}

fn host_target() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        (os, arch) => Err(update_error(format!(
            "self-update is unsupported on {os}/{arch}"
        ))),
    }
}

fn exact_asset<'a>(assets: &'a [Asset], name: &str) -> Result<&'a Asset> {
    let matches = assets
        .iter()
        .filter(|asset| asset.name == name)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [asset] => Ok(asset),
        [] => Err(update_error(format!("release asset {name} is missing"))),
        _ => Err(update_error(format!("release asset {name} is duplicated"))),
    }
}

fn validate_asset_url(asset: &Asset, prefix: &str, tag: &str) -> Result<()> {
    let expected = format!("{prefix}{tag}/");
    if !asset.browser_download_url.starts_with(&expected)
        || asset.browser_download_url[expected.len()..] != asset.name
    {
        return Err(update_error(format!(
            "release asset {} has an unexpected download URL",
            asset.name
        )));
    }
    Ok(())
}

fn validate_asset_size(asset: &Asset, limit: usize) -> Result<()> {
    if asset.size == 0 || asset.size > limit as u64 {
        return Err(update_error(format!(
            "release asset {} has invalid size {}",
            asset.name, asset.size
        )));
    }
    Ok(())
}

fn validate_downloaded_size(asset: &Asset, actual: usize) -> Result<()> {
    if asset.size != actual as u64 {
        return Err(update_error(format!(
            "release asset {} declared {} bytes but downloaded {actual}",
            asset.name, asset.size
        )));
    }
    Ok(())
}

fn checksum_for(manifest: &[u8], archive_name: &str) -> Result<String> {
    let manifest =
        std::str::from_utf8(manifest).map_err(|_| update_error("SHA256SUMS is not valid UTF-8"))?;
    let mut matches = Vec::new();
    for line in manifest.lines() {
        let Some((digest, name)) = line.split_once("  ") else {
            return Err(update_error("SHA256SUMS contains a malformed line"));
        };
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(update_error("SHA256SUMS contains an invalid digest"));
        }
        if name == archive_name {
            matches.push(digest.to_ascii_lowercase());
        }
    }
    match matches.as_slice() {
        [digest] => Ok(digest.clone()),
        [] => Err(update_error(format!(
            "SHA256SUMS does not contain {archive_name}"
        ))),
        _ => Err(update_error(format!(
            "SHA256SUMS contains duplicate entries for {archive_name}"
        ))),
    }
}

fn extract_binary(archive: &[u8], version: &Version, target: &str) -> Result<Vec<u8>> {
    let expected = PathBuf::from(format!("codex-mux-{version}-{target}/codex-mux"));
    let mut found = None;
    let mut decoded = Vec::new();
    GzDecoder::new(Cursor::new(archive))
        .take(MAX_DECOMPRESSED_BYTES + 1)
        .read_to_end(&mut decoded)
        .map_err(|error| update_error(format!("invalid compressed release archive: {error}")))?;
    if decoded.len() as u64 > MAX_DECOMPRESSED_BYTES {
        return Err(update_error(
            "release archive exceeds decompressed size limit",
        ));
    }
    let mut archive = tar::Archive::new(Cursor::new(decoded));
    let entries = archive
        .entries()
        .map_err(|error| update_error(format!("invalid release archive: {error}")))?;
    for entry in entries {
        let mut entry = entry
            .map_err(|error| update_error(format!("invalid release archive entry: {error}")))?;
        let path = entry
            .path()
            .map_err(|error| update_error(format!("invalid release archive path: {error}")))?;
        if path.as_ref() != expected {
            continue;
        }
        if found.is_some() || !entry.header().entry_type().is_file() {
            return Err(update_error(
                "release archive contains an invalid or duplicate binary entry",
            ));
        }
        if entry.size() == 0 || entry.size() > MAX_BINARY_BYTES {
            return Err(update_error("release binary has an invalid size"));
        }
        let mut binary = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut binary)
            .map_err(|error| update_error(format!("could not read release binary: {error}")))?;
        found = Some(binary);
    }
    found.ok_or_else(|| update_error("release archive does not contain the expected binary"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InstallIdentity {
    device: u64,
    inode: u64,
    uid: u32,
    gid: u32,
    links: u64,
    mode: u32,
}

fn validate_install_target(path: &Path) -> Result<InstallIdentity> {
    let metadata = fs::symlink_metadata(path).map_err(|source| MuxError::Filesystem {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.file_type().is_file() || metadata.nlink() != 1 {
        return Err(update_error(format!(
            "refusing to replace non-regular or hard-linked executable {}",
            path.display()
        )));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.gid() != rustix::process::getegid().as_raw()
    {
        return Err(update_error(format!(
            "refusing to replace executable {} because its ownership cannot be preserved",
            path.display()
        )));
    }
    if metadata.mode() & 0o7000 != 0 {
        return Err(update_error(format!(
            "refusing to replace executable {} because special permission bits cannot be preserved",
            path.display()
        )));
    }
    Ok(InstallIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        links: metadata.nlink(),
        mode: metadata.mode() & 0o7777,
    })
}

fn validate_unchanged_target(
    target: &Path,
    temporary: &Path,
    expected: &InstallIdentity,
) -> std::io::Result<()> {
    let target_metadata = fs::symlink_metadata(target)?;
    let temporary_metadata = fs::symlink_metadata(temporary)?;
    let current = InstallIdentity {
        device: target_metadata.dev(),
        inode: target_metadata.ino(),
        uid: target_metadata.uid(),
        gid: target_metadata.gid(),
        links: target_metadata.nlink(),
        mode: target_metadata.mode() & 0o7777,
    };
    if current != *expected
        || temporary_metadata.uid() != expected.uid
        || temporary_metadata.gid() != expected.gid
        || temporary_metadata.nlink() != 1
        || temporary_metadata.mode() & 0o7777 != expected.mode
    {
        return Err(std::io::Error::other(
            "installed executable changed during update",
        ));
    }
    Ok(())
}

fn update_error(message: impl Into<String>) -> MuxError {
    MuxError::Command(format!("update failed: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        os::unix::fs::PermissionsExt,
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
        thread,
        time::{Duration, Instant},
    };

    use flate2::{Compression, write::GzEncoder};
    use serde_json::json;

    use super::*;

    static NONCE: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = env::temp_dir().join(format!(
                "codex-mux-update-{}-{}",
                std::process::id(),
                NONCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Fixture {
        base: String,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Fixture {
        fn spawn(build: impl FnOnce(&str) -> HashMap<String, Vec<u8>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let responses = build(&base);
            let thread = thread::spawn(move || {
                for _ in 0..responses.len() {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request = String::new();
                    reader.read_line(&mut request).unwrap();
                    let path = request.split_ascii_whitespace().nth(1).unwrap();
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" || line.is_empty() {
                            break;
                        }
                    }
                    let body = responses.get(path).expect("unexpected fixture request");
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    stream.write_all(body).unwrap();
                }
            });
            Self {
                base,
                thread: Some(thread),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if !thread::panicking() {
                self.thread.take().unwrap().join().unwrap();
            }
        }
    }

    fn release_fixture(version: &str, binary: &[u8], corrupt_checksum: bool) -> Fixture {
        release_fixture_at(
            version,
            binary,
            corrupt_checksum,
            &format!("/releases/tags/v{version}"),
        )
    }

    fn release_fixture_at(
        version: &str,
        binary: &[u8],
        corrupt_checksum: bool,
        metadata_path: &str,
    ) -> Fixture {
        let target = host_target().unwrap();
        let archive_name = format!("codex-mux-{version}-{target}.tar.gz");
        let archive = archive(version, target, binary);
        let digest = if corrupt_checksum {
            "0".repeat(64)
        } else {
            format!("{:x}", Sha256::digest(&archive))
        };
        let checksums = format!("{digest}  {archive_name}\n").into_bytes();
        let metadata_path = metadata_path.to_owned();
        Fixture::spawn(move |base| {
            let mut responses = HashMap::new();
            responses.insert(
                metadata_path,
                serde_json::to_vec(&json!({
                    "tag_name": format!("v{version}"),
                    "assets": [
                        {
                            "name": archive_name,
                            "browser_download_url": format!("{base}/download/v{version}/{archive_name}"),
                            "size": archive.len(),
                        },
                        {
                            "name": "SHA256SUMS",
                            "browser_download_url": format!("{base}/download/v{version}/SHA256SUMS"),
                            "size": checksums.len(),
                        }
                    ]
                }))
                .unwrap(),
            );
            responses.insert(format!("/download/v{version}/{archive_name}"), archive);
            responses.insert(format!("/download/v{version}/SHA256SUMS"), checksums);
            responses
        })
    }

    fn archive(version: &str, target: &str, binary: &[u8]) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(binary.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                format!("codex-mux-{version}-{target}/codex-mux"),
                binary,
            )
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn explicit_release_downloads_verifies_and_atomically_replaces() {
        let scratch = Scratch::new();
        let executable = scratch.0.join("codex-mux");
        fs::write(&executable, b"old binary").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let fixture = release_fixture("9.8.7", b"new verified binary", false);
        let updater = Updater {
            client: UreqClient::new(false),
            api_base: format!("{}/releases", fixture.base),
            download_prefix: format!("{}/download/", fixture.base),
        };

        assert_eq!(
            updater.update_at(Some("v9.8.7"), &executable).unwrap(),
            UpdateOutcome::Installed(Version::new(9, 8, 7))
        );
        assert_eq!(fs::read(&executable).unwrap(), b"new verified binary");
        assert_eq!(
            fs::metadata(&executable).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn checksum_failure_preserves_the_installed_binary() {
        let scratch = Scratch::new();
        let executable = scratch.0.join("codex-mux");
        fs::write(&executable, b"original").unwrap();
        let fixture = release_fixture("9.8.7", b"untrusted", true);
        let updater = Updater {
            client: UreqClient::new(false),
            api_base: format!("{}/releases", fixture.base),
            download_prefix: format!("{}/download/", fixture.base),
        };

        assert!(updater.update_at(Some("9.8.7"), &executable).is_err());
        assert_eq!(fs::read(&executable).unwrap(), b"original");
    }

    #[test]
    fn latest_current_release_is_a_download_free_no_op() {
        let current = env!("CARGO_PKG_VERSION");
        let fixture = Fixture::spawn(|_| {
            HashMap::from([(
                "/releases/latest".to_owned(),
                serde_json::to_vec(&json!({
                    "tag_name": format!("v{current}"),
                    "assets": []
                }))
                .unwrap(),
            )])
        });
        let updater = Updater {
            client: UreqClient::new(false),
            api_base: format!("{}/releases", fixture.base),
            download_prefix: format!("{}/download/", fixture.base),
        };

        assert_eq!(
            updater
                .update_at(None, Path::new("/does/not/matter"))
                .unwrap(),
            UpdateOutcome::AlreadyCurrent(Version::parse(current).unwrap())
        );
    }

    #[test]
    fn latest_older_release_does_not_download_or_downgrade() {
        let current = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        let fixture = Fixture::spawn(|_| {
            HashMap::from([(
                "/releases/latest".to_owned(),
                serde_json::to_vec(&json!({
                    "tag_name": "v0.1.0",
                    "assets": []
                }))
                .unwrap(),
            )])
        });
        let updater = Updater {
            client: UreqClient::new(false),
            api_base: format!("{}/releases", fixture.base),
            download_prefix: format!("{}/download/", fixture.base),
        };

        assert_eq!(
            updater
                .update_at(None, Path::new("/does/not/matter"))
                .unwrap(),
            UpdateOutcome::AlreadyCurrent(current)
        );
    }

    #[test]
    fn latest_newer_release_runs_the_full_replacement_pipeline() {
        let scratch = Scratch::new();
        let executable = scratch.0.join("codex-mux");
        fs::write(&executable, b"old").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let fixture = release_fixture_at("9.8.7", b"new latest", false, "/releases/latest");
        let updater = Updater {
            client: UreqClient::new(false),
            api_base: format!("{}/releases", fixture.base),
            download_prefix: format!("{}/download/", fixture.base),
        };

        assert_eq!(
            updater.update_at(None, &executable).unwrap(),
            UpdateOutcome::Installed(Version::new(9, 8, 7))
        );
        assert_eq!(fs::read(&executable).unwrap(), b"new latest");
        assert_eq!(
            fs::metadata(&executable).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn running_old_image_survives_while_fresh_invocation_runs_replacement() {
        let scratch = Scratch::new();
        let executable = scratch.0.join("codex-mux");
        let signal = scratch.0.join("signal");
        fs::write(
            &executable,
            b"#!/bin/sh\nprintf old-started > \"$1\"\nsleep 1\nprintf -- '-survived' >> \"$1\"\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let mut old = Command::new(&executable).arg(&signal).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !signal.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(fs::read_to_string(&signal).unwrap(), "old-started");

        let replacement = b"#!/bin/sh\nprintf new-image\n";
        let fixture = release_fixture("9.8.7", replacement, false);
        let updater = Updater {
            client: UreqClient::new(false),
            api_base: format!("{}/releases", fixture.base),
            download_prefix: format!("{}/download/", fixture.base),
        };
        updater.update_at(Some("9.8.7"), &executable).unwrap();

        let fresh = Command::new(&executable).output().unwrap();
        assert!(fresh.status.success());
        assert_eq!(fresh.stdout, b"new-image");
        assert!(old.wait().unwrap().success());
        assert_eq!(fs::read_to_string(&signal).unwrap(), "old-started-survived");
        assert_eq!(
            fs::metadata(&executable).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn hard_linked_installation_is_refused_after_verification() {
        let scratch = Scratch::new();
        let executable = scratch.0.join("codex-mux");
        let alias = scratch.0.join("alias");
        fs::write(&executable, b"original").unwrap();
        fs::hard_link(&executable, &alias).unwrap();
        let fixture = release_fixture("9.8.7", b"replacement", false);
        let updater = Updater {
            client: UreqClient::new(false),
            api_base: format!("{}/releases", fixture.base),
            download_prefix: format!("{}/download/", fixture.base),
        };

        assert!(updater.update_at(Some("9.8.7"), &executable).is_err());
        assert_eq!(fs::read(&executable).unwrap(), b"original");
        assert_eq!(fs::read(&alias).unwrap(), b"original");
    }

    #[test]
    fn unwritable_install_directory_preserves_the_original() {
        let scratch = Scratch::new();
        let executable = scratch.0.join("codex-mux");
        fs::write(&executable, b"original").unwrap();
        fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o555)).unwrap();
        let fixture = release_fixture("9.8.7", b"replacement", false);
        let updater = Updater {
            client: UreqClient::new(false),
            api_base: format!("{}/releases", fixture.base),
            download_prefix: format!("{}/download/", fixture.base),
        };

        let result = updater.update_at(Some("9.8.7"), &executable);
        fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert_eq!(fs::read(&executable).unwrap(), b"original");
    }

    #[test]
    fn checksum_manifest_requires_one_exact_well_formed_entry() {
        let name = "codex-mux-1.2.3-x86_64-unknown-linux-gnu.tar.gz";
        let digest = "a".repeat(64);
        assert_eq!(
            checksum_for(format!("{digest}  {name}\n").as_bytes(), name).unwrap(),
            digest
        );
        assert!(checksum_for(format!("{digest} *{name}\n").as_bytes(), name).is_err());
        assert!(
            checksum_for(
                format!("{digest}  {name}\n{digest}  {name}\n").as_bytes(),
                name
            )
            .is_err()
        );
        assert!(checksum_for(format!("1234  {name}\n").as_bytes(), name).is_err());
    }

    #[test]
    fn concurrent_target_replacement_is_not_overwritten() {
        let scratch = Scratch::new();
        let executable = scratch.0.join("codex-mux");
        let displaced = scratch.0.join("displaced");
        fs::write(&executable, b"validated old").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let identity = validate_install_target(&executable).unwrap();
        fs::rename(&executable, &displaced).unwrap();
        fs::write(&executable, b"concurrent newer").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

        let result = atomic_replace_with(
            &executable,
            b"stale update",
            identity.mode,
            |temporary| validate_unchanged_target(&executable, temporary, &identity),
            |parent| fs::File::open(parent)?.sync_all(),
        );

        assert!(result.is_err());
        assert_eq!(fs::read(&executable).unwrap(), b"concurrent newer");
    }

    #[test]
    fn hard_link_created_after_validation_blocks_replacement() {
        let scratch = Scratch::new();
        let executable = scratch.0.join("codex-mux");
        let alias = scratch.0.join("alias");
        fs::write(&executable, b"validated old").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let identity = validate_install_target(&executable).unwrap();
        fs::hard_link(&executable, &alias).unwrap();

        let result = atomic_replace_with(
            &executable,
            b"stale update",
            identity.mode,
            |temporary| validate_unchanged_target(&executable, temporary, &identity),
            |parent| fs::File::open(parent)?.sync_all(),
        );

        assert!(result.is_err());
        assert_eq!(fs::read(&executable).unwrap(), b"validated old");
        assert_eq!(fs::read(&alias).unwrap(), b"validated old");
    }

    #[test]
    fn special_permission_bits_are_rejected_as_unpreservable() {
        let scratch = Scratch::new();
        let executable = scratch.0.join("codex-mux");
        fs::write(&executable, b"privileged").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o4755)).unwrap();

        assert!(validate_install_target(&executable).is_err());
        assert_eq!(fs::read(&executable).unwrap(), b"privileged");
    }

    #[test]
    fn versions_are_strict_stable_semver() {
        assert_eq!(parse_version("v1.2.3").unwrap(), Version::new(1, 2, 3));
        for invalid in ["latest", "1.2", "1.2.3-beta.1", "1.2.3+local", "vv1.2.3"] {
            assert!(parse_version(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn production_transport_rejects_plaintext_urls() {
        let error = UreqClient::new(true)
            .get("http://127.0.0.1:9/release", 16)
            .unwrap_err()
            .to_string();
        assert!(error.to_ascii_lowercase().contains("https_only"), "{error}");
    }

    #[test]
    fn stalled_response_is_bounded_by_the_request_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(200));
        });
        let started = Instant::now();
        let result = UreqClient::with_timeout(false, Duration::from_millis(50))
            .get(&format!("http://{address}/stall"), 16);
        let elapsed = started.elapsed();
        server.join().unwrap();

        assert!(result.is_err());
        assert!(elapsed < Duration::from_secs(1), "elapsed {elapsed:?}");
    }
}
