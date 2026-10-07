//! Self-update for release builds.
//!
//! The release workflow builds each binary with `VIPER_RELEASE_TARGET` set to its release target
//! (e.g. `x86_64-unknown-linux-musl`) and publishes, beside the archives, the binary alone as
//! `viper-<tag>-<target>.gz`. A release build finds the latest tag from where GitHub's
//! `/releases/latest` page redirects, downloads that file, checks it against the release's
//! `SHA256SUMS`, checks that it runs and reports the expected version, and moves it over its own
//! executable. The running process is unaffected; the next start runs the new version. Builds
//! from source never replace themselves.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

const REPO: &str = "stephenberry/viper";

/// The release target this binary was built for; set only by the release workflow.
const RELEASE_TARGET: Option<&str> = option_env!("VIPER_RELEASE_TARGET");

/// Whether this binary is an official release that can update itself.
pub fn enabled() -> bool {
    RELEASE_TARGET.is_some()
}

/// A release version, `major.minor.patch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64, u64, u64);

impl Version {
    /// Parse `1.2.3` or `v1.2.3`. Pre-release and build suffixes are not accepted.
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        let mut parts = text.strip_prefix('v').unwrap_or(text).split('.').map(|part| part.parse::<u64>().ok());
        let version = Version(parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some(version)
    }

    pub fn current() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is major.minor.patch")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The result of a check for a newer release.
#[derive(Debug)]
pub enum Outcome {
    /// This is the latest release.
    UpToDate,
    /// The newer release is installed and runs from the next start.
    Installed(Version),
    /// A newer release is out, and installing it automatically is turned off.
    Available(Version),
    /// A newer release is out, but installing it failed.
    Failed(Version, anyhow::Error),
}

/// Check for a newer release and, if `install` is set, install it over the running executable.
/// Fails only when the latest release cannot be determined, e.g. while offline.
pub async fn check(client: &reqwest::Client, install: bool) -> Result<Outcome> {
    let Some(target) = RELEASE_TARGET else { return Ok(Outcome::UpToDate) };
    let base = format!("https://github.com/{REPO}");
    let releases = Releases { client, base: &base, target };
    let latest = releases.latest().await?;
    if latest <= Version::current() {
        return Ok(Outcome::UpToDate);
    }
    if !install {
        return Ok(Outcome::Available(latest));
    }
    let installed = match current_exe() {
        Ok(exe) => releases.install(exe, latest).await,
        Err(err) => Err(err),
    };
    Ok(match installed {
        Ok(()) => Outcome::Installed(latest),
        Err(err) => Outcome::Failed(latest, err),
    })
}

/// Releases laid out as on GitHub: `<base>/releases/latest` redirects to `.../tag/<tag>`, and
/// `<base>/releases/download/<tag>/<file>` serves a release's files.
struct Releases<'a> {
    client: &'a reqwest::Client,
    base: &'a str,
    /// The release target whose binary to download.
    target: &'a str,
}

impl Releases<'_> {
    /// The latest release's version, read from where `/releases/latest` redirects. Unlike the
    /// GitHub API, this is not rate limited per IP address.
    async fn latest(&self) -> Result<Version> {
        let url = format!("{}/releases/latest", self.base);
        let response = self.client.head(&url).send().await.and_then(|r| r.error_for_status());
        let response = response.with_context(|| format!("could not reach {url}"))?;
        let tag = response.url().path_segments().and_then(|mut segments| segments.next_back()).unwrap_or_default();
        Version::parse(tag).ok_or_else(|| anyhow!("no release found at {}/releases", self.base))
    }

    /// Download `version`'s binary, verify it, and install it over `exe`.
    async fn install(&self, exe: PathBuf, version: Version) -> Result<()> {
        // Another viper may have installed it already.
        if installed_version(&exe) == Some(version) {
            return Ok(());
        }
        let files = format!("{}/releases/download/{version}", self.base);
        let name = format!("viper-{version}-{}.gz", self.target);
        let archive = download(self.client, &format!("{files}/{name}")).await?;
        let sums = download(self.client, &format!("{files}/SHA256SUMS")).await?;
        verify_checksum(&archive, &String::from_utf8_lossy(&sums), &name)?;
        tokio::task::spawn_blocking(move || install(&exe, &archive, version)).await.context("the update task failed")?
    }
}

async fn download(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = client.get(url).send().await.and_then(|r| r.error_for_status());
    let bytes = response.with_context(|| format!("could not download {url}"))?.bytes().await;
    Ok(bytes.with_context(|| format!("could not download {url}"))?.to_vec())
}

/// Check `data` against its entry in a `sha256sum`-style listing.
fn verify_checksum(data: &[u8], sums: &str, name: &str) -> Result<()> {
    let expected = sums
        .lines()
        .find_map(|line| {
            let (hash, file) = line.split_once(char::is_whitespace)?;
            (file.trim_start().trim_start_matches('*') == name).then_some(hash)
        })
        .ok_or_else(|| anyhow!("SHA256SUMS has no entry for {name}"))?;
    if !sha256_hex(data).eq_ignore_ascii_case(expected) {
        bail!("checksum mismatch for {name}");
    }
    Ok(())
}

fn sha256_hex(data: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, data).as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The path of the running executable, as it is on disk.
fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("could not find the viper executable")?;
    // Linux reports a replaced executable as "<path> (deleted)".
    let exe = match exe.to_str().and_then(|path| path.strip_suffix(" (deleted)")) {
        Some(path) if !exe.exists() => PathBuf::from(path),
        _ => exe,
    };
    std::fs::canonicalize(&exe).with_context(|| format!("could not resolve {}", exe.display()))
}

/// The version `exe --version` reports, if it runs.
fn installed_version(exe: &Path) -> Option<Version> {
    let output = Command::new(exe).arg("--version").output().ok().filter(|output| output.status.success())?;
    // clap prints "viper 1.2.3".
    Version::parse(String::from_utf8_lossy(&output.stdout).split_whitespace().nth(1)?)
}

/// Write the gzipped binary next to `exe`, check that it is `version`, and move it over `exe`.
fn install(exe: &Path, archive: &[u8], version: Version) -> Result<()> {
    let mut binary = Vec::new();
    flate2::read::GzDecoder::new(archive).read_to_end(&mut binary).context("could not decompress the update")?;
    let dir = exe.parent().context("the viper executable has no parent directory")?;
    // A unique name, with the extension Windows needs to run it.
    let staged = dir.join(format!(".viper-update-{}{}", std::process::id(), std::env::consts::EXE_SUFFIX));
    let result = stage(&staged, &binary, version).and_then(|()| replace(exe, &staged));
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

fn stage(staged: &Path, binary: &[u8], version: Version) -> Result<()> {
    std::fs::write(staged, binary).with_context(|| format!("could not write {}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(staged, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("could not make {} executable", staged.display()))?;
    }
    match installed_version(staged) {
        Some(found) if found == version => Ok(()),
        Some(found) => bail!("the downloaded binary reports {found}, not {version}"),
        None => bail!("the downloaded binary does not run"),
    }
}

/// Rename over the executable; the running process keeps the old file open.
#[cfg(not(windows))]
fn replace(exe: &Path, staged: &Path) -> Result<()> {
    std::fs::rename(staged, exe).with_context(|| format!("could not replace {}", exe.display()))
}

/// A running executable cannot be replaced on Windows, but it can be renamed: move it aside to
/// `viper.exe.old` (removed at a later start) and put the new one in its place.
#[cfg(windows)]
fn replace(exe: &Path, staged: &Path) -> Result<()> {
    let old = old_exe_path(exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).with_context(|| format!("could not move {} aside", exe.display()))?;
    if let Err(err) = std::fs::rename(staged, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(err).with_context(|| format!("could not replace {}", exe.display()));
    }
    Ok(())
}

#[cfg(windows)]
fn old_exe_path(exe: &Path) -> PathBuf {
    let mut name = exe.as_os_str().to_owned();
    name.push(".old");
    PathBuf::from(name)
}

/// Remove the executable a previous update on Windows moved aside, now that it no longer runs.
pub fn remove_old_exe() {
    #[cfg(windows)]
    if let Ok(exe) = current_exe() {
        let _ = std::fs::remove_file(old_exe_path(&exe));
    }
}

/// The text to show for an outcome, and whether it is a warning; nothing when up to date.
pub fn describe(outcome: &Outcome) -> Option<(String, bool)> {
    let current = Version::current();
    let install = format!("rerun the installer to update: https://github.com/{REPO}#install");
    match outcome {
        Outcome::UpToDate => None,
        Outcome::Installed(version) => Some((format!("Updated viper to {version}; restart viper to use it."), false)),
        Outcome::Available(version) => {
            Some((format!("viper {version} is available (this is {current}); {install}"), false))
        }
        Outcome::Failed(version, err) => {
            Some((format!("viper {version} is available, but updating failed: {err:#}. To update, {install}"), true))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_and_order() {
        assert_eq!(Version::parse("v0.1.2"), Some(Version(0, 1, 2)));
        assert_eq!(Version::parse("10.0.3"), Some(Version(10, 0, 3)));
        assert_eq!(Version::parse("v1.2"), None);
        assert_eq!(Version::parse("v1.2.3.4"), None);
        assert_eq!(Version::parse("v1.2.3-rc.1"), None);
        assert_eq!(Version::parse("releases"), None);
        assert!(Version(0, 10, 0) > Version(0, 9, 9));
        assert_eq!(Version(1, 2, 3).to_string(), "v1.2.3");
        assert!(Version::parse(env!("CARGO_PKG_VERSION")).is_some());
    }

    #[test]
    fn checksums_match_their_entry() {
        let data = b"viper";
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let sums =
            format!("{}  viper-v1.0.0-other.gz\n{}  viper-v1.0.0-x.gz\n", sha256_hex(b"other"), sha256_hex(data));
        assert!(verify_checksum(data, &sums, "viper-v1.0.0-x.gz").is_ok());
        assert!(verify_checksum(data, &sums, "viper-v1.0.0-other.gz").is_err());
        assert!(verify_checksum(data, &sums, "viper-v1.0.0-missing.gz").is_err());
    }

    /// A stand-in for a release binary: a script that reports version 9.8.7, gzipped.
    #[cfg(unix)]
    fn release_archive() -> Vec<u8> {
        use std::io::Write;
        let mut archive = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        archive.write_all(b"#!/bin/sh\necho 'viper 9.8.7'\n").unwrap();
        archive.finish().unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn installs_over_the_executable() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("viper");
        std::fs::write(&exe, "old").unwrap();
        let archive = release_archive();

        assert!(install(&exe, &archive, Version(9, 9, 9)).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
        install(&exe, &archive, Version(9, 8, 7)).unwrap();
        assert_eq!(installed_version(&exe), Some(Version(9, 8, 7)));
        // Nothing staged is left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// Serve `routes` (path, status, `location` header, body) over HTTP, one connection per request.
    async fn serve(routes: Vec<(String, u16, Option<&'static str>, Vec<u8>)>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut head = Vec::new();
                let mut chunk = [0u8; 1024];
                while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                    let n = socket.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    head.extend_from_slice(&chunk[..n]);
                }
                let request = String::from_utf8_lossy(&head).into_owned();
                let mut words = request.split_whitespace();
                let (method, path) = (words.next().unwrap_or_default(), words.next().unwrap_or_default());
                let (status, location, body) = match routes.iter().find(|route| route.0 == path) {
                    Some((_, status, location, body)) => (*status, *location, body.as_slice()),
                    None => (404, None, &[][..]),
                };
                let mut reply =
                    format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n", body.len());
                if let Some(location) = location {
                    reply.push_str(&format!("location: {location}\r\n"));
                }
                reply.push_str("\r\n");
                let mut reply = reply.into_bytes();
                if method != "HEAD" {
                    reply.extend_from_slice(body);
                }
                socket.write_all(&reply).await.unwrap();
                socket.shutdown().await.ok();
            }
        });
        url
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn finds_downloads_verifies_and_installs_the_latest_release() {
        let archive = release_archive();
        let name = "viper-v9.8.7-test-target.gz";
        let files = "/releases/download/v9.8.7";
        let sums = |hash: String| format!("{hash}  {name}\n").into_bytes();
        let routes = |sums: Vec<u8>| {
            vec![
                ("/releases/latest".to_string(), 302, Some("/releases/tag/v9.8.7"), Vec::new()),
                ("/releases/tag/v9.8.7".to_string(), 200, None, Vec::new()),
                (format!("{files}/{name}"), 200, None, archive.clone()),
                (format!("{files}/SHA256SUMS"), 200, None, sums),
            ]
        };
        let client = reqwest::Client::new();
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("viper");

        // A download that does not match its checksum is not installed.
        let base = serve(routes(sums(sha256_hex(b"something else")))).await;
        let releases = Releases { client: &client, base: &base, target: "test-target" };
        std::fs::write(&exe, "old").unwrap();
        let err = releases.install(exe.clone(), Version(9, 8, 7)).await.unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"), "{err:#}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");

        let base = serve(routes(sums(sha256_hex(&archive)))).await;
        let releases = Releases { client: &client, base: &base, target: "test-target" };
        assert_eq!(releases.latest().await.unwrap(), Version(9, 8, 7));
        releases.install(exe.clone(), Version(9, 8, 7)).await.unwrap();
        assert_eq!(installed_version(&exe), Some(Version(9, 8, 7)));

        // A missing release is an error, not an update.
        let base = serve(vec![("/releases/latest".to_string(), 302, Some("/releases"), Vec::new())]).await;
        let releases = Releases { client: &client, base: &base, target: "test-target" };
        assert!(releases.latest().await.is_err());
    }
}
