//! Verified native releases, staged beside the executable with rollback on replacement failure.

use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use reqwest::{Client, StatusCode};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::error::{Error, Result};

const RELEASE_URL: &str = "https://api.github.com/repos/ImArjunJ/moocp/releases/latest";
const ASSET_ROOT: &str = "https://github.com/ImArjunJ/moocp/releases/download/";
const MAX_ARCHIVE: usize = 128 * 1024 * 1024;
const MAX_EXECUTABLE: u64 = 256 * 1024 * 1024;

pub struct Updater {
    client: Client,
    release_url: Url,
    asset_root: Url,
}

#[derive(Debug)]
pub enum Report {
    Current {
        installed: Version,
        latest: Version,
    },
    Available {
        installed: Version,
        latest: Version,
    },
    Updated {
        installed: Version,
        latest: Version,
    },
    Pending {
        installed: Version,
        latest: Version,
        log: PathBuf,
    },
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Current { installed, latest } if installed > latest => write!(
                f,
                "Moocp {installed} is newer than the latest release ({latest}). Kept this build."
            ),
            Self::Current { installed, .. } => write!(f, "Moocp {installed} is up to date."),
            Self::Available { installed, latest } => write!(
                f,
                "Moocp {latest} is available (installed: {installed}). Run moocp update."
            ),
            Self::Updated { installed, latest } => write!(
                f,
                "Updated Moocp {installed} to {latest}. Restart connected CLI sessions."
            ),
            Self::Pending {
                installed,
                latest,
                log,
            } => write!(
                f,
                "Verified Moocp {latest} (installed: {installed}). Windows will replace it after this command exits.\nRestart connected CLI sessions to release the executable. Replacement waits up to one minute and backs up the previous executable.\nIf it fails, details are saved at: {}",
                log.display()
            ),
        }
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: Url,
    size: usize,
}

impl Updater {
    pub fn github() -> Result<Self> {
        Self::new(RELEASE_URL.parse().unwrap(), ASSET_ROOT.parse().unwrap())
    }

    /// Pin a release source and its download root. HTTP is allowed only for loopback fixtures.
    /// The CLI always uses the official GitHub source.
    pub fn new(release_url: Url, asset_root: Url) -> Result<Self> {
        for url in [&release_url, &asset_root] {
            let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"));
            if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(Error::invalid(
                    "Updates require HTTPS or a local test server.",
                ));
            }
        }
        let origins = [release_url.origin(), asset_root.origin()];
        let client = Client::builder()
            .https_only(release_url.scheme() == "https" && asset_root.scheme() == "https")
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                let url = attempt.url();
                let github_cdn = url.scheme() == "https"
                    && matches!(
                        url.host_str(),
                        Some(
                            "release-assets.githubusercontent.com"
                                | "objects.githubusercontent.com"
                                | "github-releases.githubusercontent.com"
                        )
                    );
                if attempt.previous().len() >= 5
                    || !(origins.contains(&url.origin()) || github_cdn)
                    || !url.username().is_empty()
                    || url.password().is_some()
                {
                    attempt.error("Unexpected release redirect")
                } else {
                    attempt.follow()
                }
            }))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(180))
            .user_agent(concat!("Moocp/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| Error::new("update_network", "Could not initialise update networking."))?;
        Ok(Self {
            client,
            release_url,
            asset_root,
        })
    }

    pub async fn run(&self, executable: &Path, installed: &str, check: bool) -> Result<Report> {
        let installed =
            Version::parse(installed).map_err(|_| Error::invalid("Invalid installed version."))?;
        let original_hash = if check {
            None
        } else {
            Some(file_hash(executable)?)
        };
        let body = self.download(&self.release_url, 1024 * 1024).await?;
        let release: Release = serde_json::from_slice(&body)?;
        let latest = Version::parse(release.tag_name.strip_prefix('v').ok_or_else(|| {
            Error::new("invalid_release", "The release tag is not a Moocp version.")
        })?)
        .map_err(|_| Error::new("invalid_release", "The release tag is not a Moocp version."))?;
        if release.draft || release.prerelease || !latest.pre.is_empty() {
            return Err(Error::new(
                "invalid_release",
                "Expected a published stable release.",
            ));
        }
        if latest <= installed {
            return Ok(Report::Current { installed, latest });
        }
        let target = platform()?;
        let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
        let name = format!("moocp-{target}.{extension}");
        let archive_url = self.asset(&release, &name, MAX_ARCHIVE)?;
        let checksum_url = self.asset(&release, &format!("{name}.sha256"), 1024)?;
        if check {
            return Ok(Report::Available { installed, latest });
        }
        let executable = executable.canonicalize()?;
        let _lock = UpdateLock::acquire(&executable)?;
        let original_hash =
            original_hash.ok_or_else(|| Error::invalid("A check cannot install an update."))?;
        unchanged(&executable, &original_hash)?;
        let parent = executable
            .parent()
            .ok_or_else(|| Error::local("Cannot find the installation directory."))?;
        let stage = tempfile::Builder::new().prefix(".moocp-update-").tempdir_in(parent)
            .map_err(|_| Error::local("Cannot write to the installation directory. Use the original installer to update Moocp."))?;
        crate::config::private_dir(stage.path())?;
        crate::config::private_write(
            &stage.path().join("target.json"),
            &serde_json::to_vec(&executable)?,
        )?;
        let checksum = self.download(&checksum_url, 1024).await?;
        let expected = parse_checksum(&checksum, &name)?;
        let archive = self.download(&archive_url, MAX_ARCHIVE).await?;
        if format!("{:x}", Sha256::digest(&archive)) != expected {
            return Err(Error::new(
                "update_checksum",
                "Release checksum did not match. The installed version was kept.",
            ));
        }
        unchanged(&executable, &original_hash)?;
        let candidate = stage
            .path()
            .join(if cfg!(windows) { "moocp.exe" } else { "moocp" });
        extract(&archive, extension, &candidate)?;
        fs::set_permissions(&candidate, fs::metadata(&executable)?.permissions())?;
        verify_version(&candidate, &latest).await?;
        unchanged(&executable, &original_hash)?;
        #[cfg(not(windows))]
        {
            replace_unix(&executable, &candidate, stage, &latest).await?;
            Ok(Report::Updated { installed, latest })
        }
        #[cfg(windows)]
        {
            let log = replace_windows(&executable, stage, &latest, &original_hash)?;
            Ok(Report::Pending {
                installed,
                latest,
                log,
            })
        }
    }

    fn asset(&self, release: &Release, name: &str, limit: usize) -> Result<Url> {
        let mut matches = release.assets.iter().filter(|asset| asset.name == name);
        let asset = matches.next().ok_or_else(|| Error::new("update_asset", "The release has no verified download for this platform. Try again after publication finishes."))?;
        let expected = self
            .asset_root
            .join(&format!("{}/{name}", release.tag_name))
            .map_err(|_| Error::new("invalid_release", "Invalid release download path."))?;
        if matches.next().is_some()
            || asset.browser_download_url != expected
            || asset.size == 0
            || asset.size > limit
        {
            return Err(Error::new(
                "invalid_release",
                "The release download metadata is invalid.",
            ));
        }
        Ok(expected)
    }

    async fn download(&self, url: &Url, limit: usize) -> Result<Vec<u8>> {
        let mut response = self
            .client
            .get(url.clone())
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|_| {
                Error::new(
                    "update_network",
                    "Could not download the release. Check your connection and try again.",
                )
            })?;
        if !response.status().is_success() {
            let message = match response.status() {
                StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => {
                    "GitHub temporarily limited update checks. Try again later."
                }
                StatusCode::NOT_FOUND => {
                    "The release download is unavailable. Try again after publication finishes."
                }
                _ => "The release server could not complete the download. Try again later.",
            };
            return Err(Error::new("update_network", message));
        }
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(Error::new(
                "update_size",
                "The release download exceeds its size limit.",
            ));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| {
            Error::new(
                "update_network",
                "Release download was interrupted. The installed version was kept.",
            )
        })? {
            if chunk.len() > limit.saturating_sub(body.len()) {
                return Err(Error::new(
                    "update_size",
                    "The release download exceeds its size limit.",
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

pub fn platform() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") if cfg!(target_env = "gnu") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") if cfg!(target_env = "gnu") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        _ => Err(Error::unsupported(
            "No native release for this platform. Update using the source installation.",
        )),
    }
}

fn parse_checksum(bytes: &[u8], name: &str) -> Result<String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Error::new("update_checksum", "Invalid release checksum."))?
        .trim_start_matches('\u{feff}');
    let parts: Vec<_> = text.split_whitespace().collect();
    if parts.len() != 2
        || parts[0].len() != 64
        || !parts[0].bytes().all(|byte| byte.is_ascii_hexdigit())
        || parts[1].trim_start_matches('*') != name
    {
        return Err(Error::new("update_checksum", "Invalid release checksum."));
    }
    Ok(parts[0].to_ascii_lowercase())
}

fn extract(archive: &[u8], extension: &str, output: &Path) -> Result<()> {
    let mut file = File::create(output)?;
    let invalid = || {
        Error::new(
            "update_archive",
            "The release archive has no unique regular Moocp executable.",
        )
    };
    match extension {
        "tar.gz" => {
            let mut archive = tar::Archive::new(
                flate2::read::GzDecoder::new(archive).take(MAX_EXECUTABLE + 1024 * 1024),
            );
            let mut found = false;
            for entry in archive.entries().map_err(|_| invalid())? {
                let mut entry = entry.map_err(|_| invalid())?;
                if entry.path().map_err(|_| invalid())?.as_ref() == Path::new("moocp")
                    || entry.path().map_err(|_| invalid())?.as_ref() == Path::new("./moocp")
                {
                    if found
                        || !entry.header().entry_type().is_file()
                        || entry.size() > MAX_EXECUTABLE
                    {
                        return Err(invalid());
                    }
                    let length =
                        std::io::copy(&mut entry.by_ref().take(MAX_EXECUTABLE + 1), &mut file)?;
                    if length != entry.size() || length == 0 {
                        return Err(invalid());
                    }
                    found = true;
                }
            }
            if !found {
                return Err(invalid());
            }
        }
        "zip" => {
            let mut archive =
                zip::ZipArchive::new(std::io::Cursor::new(archive)).map_err(|_| invalid())?;
            let mut found = false;
            for index in 0..archive.len() {
                let mut entry = archive.by_index(index).map_err(|_| invalid())?;
                if entry.name() == "moocp.exe" {
                    if found
                        || !entry.is_file()
                        || entry.is_symlink()
                        || entry.size() > MAX_EXECUTABLE
                    {
                        return Err(invalid());
                    }
                    let size = entry.size();
                    let length =
                        std::io::copy(&mut entry.by_ref().take(MAX_EXECUTABLE + 1), &mut file)?;
                    if length != size || length == 0 {
                        return Err(invalid());
                    }
                    found = true;
                }
            }
            if !found {
                return Err(invalid());
            }
        }
        _ => return Err(invalid()),
    }
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

async fn verify_version(executable: &Path, expected: &Version) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|_| {
        Error::new(
            "update_executable",
            "The downloaded executable cannot run here. The installed version was kept.",
        )
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::local("Cannot read executable version."))?;
    let probe = async {
        let mut output = Vec::new();
        stdout.take(129).read_to_end(&mut output).await?;
        if output.len() > 128 {
            return Ok::<_, std::io::Error>(None);
        }
        let status = child.wait().await?;
        Ok(Some((status, output)))
    };
    let output = tokio::time::timeout(Duration::from_secs(15), probe)
        .await
        .map_err(|_| {
            Error::new(
                "update_executable",
                "The downloaded executable timed out. The installed version was kept.",
            )
        })?
        .map_err(|_| {
            Error::new(
                "update_executable",
                "The downloaded executable cannot run here. The installed version was kept.",
            )
        })?;
    if output.is_none_or(|(status, stdout)| {
        !status.success() || stdout != format!("moocp {expected}\n").as_bytes()
    }) {
        return Err(Error::new(
            "update_executable",
            "The executable version does not match the release. The installed version was kept.",
        ));
    }
    Ok(())
}

fn file_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn unchanged(executable: &Path, expected: &str) -> Result<()> {
    if file_hash(executable).ok().as_deref() != Some(expected) {
        return Err(Error::new(
            "update_changed",
            "Moocp changed during the download. Run update again.",
        ));
    }
    Ok(())
}

pub(crate) struct UpdateLock {
    _file: File,
}
impl UpdateLock {
    pub(crate) fn acquire(executable: &Path) -> Result<Self> {
        let path = lock_path(executable);
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        fs2::FileExt::try_lock_exclusive(&file).map_err(|_| {
            Error::new(
                "update_busy",
                "An update is already running. Restart connected CLI sessions and try again.",
            )
        })?;
        Ok(Self { _file: file })
    }
}

pub(crate) fn lock_path(executable: &Path) -> PathBuf {
    executable.with_file_name(format!(
        ".{}.update.lock",
        executable.file_name().unwrap_or_default().to_string_lossy()
    ))
}

pub(crate) fn remove_staging(executable: &Path) -> Result<()> {
    let Some(parent) = executable.parent() else {
        return Ok(());
    };
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && entry
                .file_name()
                .to_string_lossy()
                .starts_with(".moocp-update-")
            && fs::read(entry.path().join("target.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<PathBuf>(&bytes).ok())
                .is_some_and(|path| path == executable)
        {
            fs::remove_dir_all(entry.path())?;
            match fs::remove_file(entry.path().with_extension("failure.txt")) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

#[cfg(not(windows))]
async fn replace_unix(
    executable: &Path,
    candidate: &Path,
    stage: tempfile::TempDir,
    latest: &Version,
) -> Result<()> {
    let backup = stage.path().join("previous");
    fs::hard_link(executable, &backup).or_else(|_| fs::copy(executable, &backup).map(|_| ()))?;
    fs::rename(candidate, executable)?;
    if let Err(error) = verify_version(executable, latest).await {
        if fs::rename(&backup, executable).is_err() {
            let saved = stage.keep();
            return Err(Error::new(
                "update_rollback",
                format!(
                    "Update failed and rollback could not finish. The previous executable is at {}.",
                    saved.join("previous").display()
                ),
            ));
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(windows)]
fn replace_windows(
    executable: &Path,
    stage: tempfile::TempDir,
    latest: &Version,
    original_hash: &str,
) -> Result<PathBuf> {
    use std::{os::windows::process::CommandExt, process::Command};
    let script = stage.path().join("replace.ps1");
    // Keep helper errors outside staging so an open stderr handle cannot block cleanup.
    let log = stage.path().with_extension("failure.txt");
    crate::config::private_write(&script, include_bytes!("update_windows.ps1"))?;
    let lock = lock_path(executable);
    Command::new("powershell.exe")
        // A PowerShell 7 parent can otherwise shadow Windows PowerShell's built-in modules.
        .env_remove("PSModulePath")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(powershell_path(&script))
        .arg("-ProcessId")
        .arg(std::process::id().to_string())
        .arg("-Executable")
        .arg(powershell_path(executable))
        .arg("-Version")
        .arg(latest.to_string())
        .arg("-OriginalHash")
        .arg(original_hash)
        .arg("-LockFile")
        .arg(powershell_path(&lock))
        .arg("-FailureLog")
        .arg(powershell_path(&log))
        .creation_flags(0x08000000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(File::create(&log)?))
        .spawn()
        .map_err(|_| {
            Error::new(
                "update_helper",
                "Could not start Windows replacement. The installed version was kept.",
            )
        })?;
    let _ = stage.keep();
    Ok(powershell_path(&log))
}

#[cfg(windows)]
fn powershell_path(path: &Path) -> PathBuf {
    use std::{
        ffi::OsString,
        os::windows::ffi::{OsStrExt, OsStringExt},
    };
    // PowerShell's -File parser does not understand Windows verbatim path prefixes.
    let wide: Vec<_> = path.as_os_str().encode_wide().collect();
    if wide.starts_with(&[92, 92, 63, 92, 85, 78, 67, 92]) {
        PathBuf::from(OsString::from_wide(&[&[92, 92], &wide[8..]].concat()))
    } else if wide.starts_with(&[92, 92, 63, 92]) {
        PathBuf::from(OsString::from_wide(&wide[4..]))
    } else {
        path.to_owned()
    }
}
