use axum::{
    Router,
    extract::{Path as RoutePath, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use moocp::update::{Report, Updater};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn package(binary: &[u8]) -> Vec<u8> {
    if cfg!(windows) {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .compression_level(Some(1));
        writer.start_file("../outside", options).unwrap();
        writer.write_all(b"must never be extracted").unwrap();
        writer.start_file("moocp.exe", options).unwrap();
        writer.write_all(binary).unwrap();
        writer.finish().unwrap().into_inner()
    } else {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut writer = tar::Builder::new(encoder);
        let mut outside = tar::Header::new_gnu();
        outside.set_size(7);
        outside.set_mode(0o644);
        outside.as_mut_bytes()[..10].copy_from_slice(b"../outside");
        outside.set_cksum();
        writer.append(&outside, b"ignored".as_slice()).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(binary.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        writer.append_data(&mut header, "./moocp", binary).unwrap();
        writer.into_inner().unwrap().finish().unwrap()
    }
}

fn release_archive() -> Vec<u8> {
    static ARCHIVE: OnceLock<Vec<u8>> = OnceLock::new();
    ARCHIVE
        .get_or_init(|| {
            if cfg!(windows) {
                package(&fs::read(env!("CARGO_BIN_EXE_moocp")).unwrap())
            } else {
                package(format!("#!/bin/sh\nprintf 'moocp {VERSION}\\n'\n").as_bytes())
            }
        })
        .clone()
}

struct Server {
    archive: Vec<u8>,
    name: String,
    checksum: String,
    version: String,
    root: String,
    downloads: AtomicUsize,
    changed: Option<PathBuf>,
    redirect: bool,
}

struct Fixture {
    server: Arc<Server>,
    directory: tempfile::TempDir,
    executable: PathBuf,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Fixture {
    async fn new(
        version: &str,
        archive: Vec<u8>,
        corrupt: bool,
        changed: bool,
        redirect: bool,
    ) -> anyhow::Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("moocp update with spaces ")
            .tempdir()?;
        let executable = directory
            .path()
            .join(if cfg!(windows) { "moocp.exe" } else { "moocp" });
        fs::write(&executable, b"previous release")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
        }
        fs::write(
            directory.path().join("account.json"),
            b"private fixture account",
        )?;
        fs::write(
            directory.path().join("cli-config.json"),
            b"unrelated MCP servers",
        )?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let root = format!("http://{}/", listener.local_addr()?);
        let target = moocp::update::platform()?;
        let name = format!(
            "moocp-{target}.{}",
            if cfg!(windows) { "zip" } else { "tar.gz" }
        );
        let digest = if corrupt {
            "0".repeat(64)
        } else {
            format!("{:x}", Sha256::digest(&archive))
        };
        let server = Arc::new(Server {
            checksum: format!("\u{feff}{digest}  {name}\n"),
            archive,
            name,
            version: version.to_owned(),
            root,
            downloads: AtomicUsize::new(0),
            changed: changed.then(|| executable.clone()),
            redirect,
        });
        let app = Router::new()
            .route("/latest", get(metadata))
            .route("/releases/download/{version}/{name}", get(download))
            .with_state(server.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Ok(Self {
            server,
            directory,
            executable,
            task,
        })
    }
    fn updater(&self) -> anyhow::Result<Updater> {
        Ok(Updater::new(
            format!("{}latest", self.server.root).parse()?,
            format!("{}releases/download/", self.server.root).parse()?,
        )?)
    }
    fn unchanged_data(&self) {
        assert_eq!(
            fs::read(self.directory.path().join("account.json")).unwrap(),
            b"private fixture account"
        );
        assert_eq!(
            fs::read(self.directory.path().join("cli-config.json")).unwrap(),
            b"unrelated MCP servers"
        );
        assert!(!self.directory.path().join("outside").exists());
    }
    fn stages(&self) -> usize {
        fs::read_dir(self.directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".moocp-update-")
            })
            .count()
    }
}

async fn metadata(State(server): State<Arc<Server>>) -> axum::Json<Value> {
    axum::Json(
        json!({"tag_name":format!("v{}", server.version), "draft":false, "prerelease":false,
        "assets":[{"name":server.name,"size":server.archive.len(),"browser_download_url":format!("{}releases/download/v{}/{}",server.root,server.version,server.name)},
        {"name":format!("{}.sha256",server.name),"size":server.checksum.len(),"browser_download_url":format!("{}releases/download/v{}/{}.sha256",server.root,server.version,server.name)}]}),
    )
}
async fn download(
    State(server): State<Arc<Server>>,
    RoutePath((_, name)): RoutePath<(String, String)>,
) -> axum::response::Response {
    server.downloads.fetch_add(1, Ordering::Relaxed);
    if name.ends_with(".sha256") {
        return server.checksum.clone().into_response();
    }
    if server.redirect {
        return (
            StatusCode::FOUND,
            [("location", "http://127.0.0.1:1/untrusted")],
        )
            .into_response();
    }
    if let Some(path) = &server.changed {
        fs::write(path, b"externally installed release").unwrap();
    }
    server.archive.clone().into_response()
}

#[cfg(not(windows))]
async fn apply(fixture: &Fixture) -> anyhow::Result<String> {
    Ok(
        match fixture
            .updater()?
            .run(&fixture.executable, "0.0.0", false)
            .await
        {
            Ok(Report::Updated { .. }) => "updated".into(),
            Ok(other) => panic!("Unexpected report: {other:?}"),
            Err(error) => error.code,
        },
    )
}

#[cfg(windows)]
async fn apply(fixture: &Fixture) -> anyhow::Result<String> {
    let outcome = fixture.directory.path().join("outcome.json");
    let params =
        json!({"source":fixture.server.root,"executable":fixture.executable,"outcome":outcome});
    let output = tokio::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "windows_update_worker", "--nocapture"])
        .env("MOOCP_TEST_UPDATE", params.to_string())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&fs::read(outcome)?)?;
    Ok(result["result"].as_str().unwrap().to_owned())
}

#[cfg(windows)]
#[test]
fn windows_update_worker() {
    let Ok(params) = std::env::var("MOOCP_TEST_UPDATE") else {
        return;
    };
    let params: Value = serde_json::from_str(&params).unwrap();
    let root = params["source"].as_str().unwrap();
    let updater = Updater::new(
        format!("{root}latest").parse().unwrap(),
        format!("{root}releases/download/").parse().unwrap(),
    )
    .unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let report = runtime.block_on(updater.run(
        std::path::Path::new(params["executable"].as_str().unwrap()),
        "0.0.0",
        false,
    ));
    let result = match report {
        Ok(Report::Pending { .. }) => "updated".to_owned(),
        Ok(other) => panic!("{other:?}"),
        Err(error) => error.code,
    };
    fs::write(
        params["outcome"].as_str().unwrap(),
        json!({"result":result}).to_string(),
    )
    .unwrap();
}

#[tokio::test]
async fn check_and_newer_builds_never_download_or_replace() -> anyhow::Result<()> {
    let fixture = Fixture::new(VERSION, release_archive(), false, false, false).await?;
    assert!(matches!(
        fixture
            .updater()?
            .run(&fixture.executable, "0.0.0", true)
            .await?,
        Report::Available { .. }
    ));
    assert!(matches!(
        fixture
            .updater()?
            .run(&fixture.executable, VERSION, false)
            .await?,
        Report::Current { .. }
    ));
    assert!(matches!(
        fixture
            .updater()?
            .run(&fixture.executable, "99.0.0", false)
            .await?,
        Report::Current { .. }
    ));
    assert_eq!(fixture.server.downloads.load(Ordering::Relaxed), 0);
    assert_eq!(fs::read(&fixture.executable)?, b"previous release");
    assert_eq!(fixture.stages(), 0);
    fixture.unchanged_data();
    Ok(())
}

#[tokio::test]
async fn verified_update_preserves_install_path_accounts_and_other_servers() -> anyhow::Result<()> {
    let fixture = Fixture::new(VERSION, release_archive(), false, false, false).await?;
    assert_eq!(apply(&fixture).await?, "updated");
    for _ in 0..200 {
        if fixture.stages() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(fixture.stages(), 0);
    let output = tokio::process::Command::new(&fixture.executable)
        .arg("--version")
        .output()
        .await?;
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)?.trim(),
        format!("moocp {VERSION}")
    );
    fixture.unchanged_data();
    Ok(())
}

#[tokio::test]
async fn failed_verification_and_redirects_keep_the_previous_release() -> anyhow::Result<()> {
    for (version, corrupt, redirect, expected) in [
        (VERSION, true, false, "update_checksum"),
        ("9.9.9", false, false, "update_executable"),
        (VERSION, false, true, "update_network"),
    ] {
        let fixture = Fixture::new(version, release_archive(), corrupt, false, redirect).await?;
        assert_eq!(apply(&fixture).await?, expected);
        assert_eq!(fs::read(&fixture.executable)?, b"previous release");
        assert_eq!(fixture.stages(), 0);
        fixture.unchanged_data();
    }
    Ok(())
}

#[tokio::test]
async fn external_replacement_is_preserved() -> anyhow::Result<()> {
    let fixture = Fixture::new(VERSION, release_archive(), false, true, false).await?;
    assert_eq!(apply(&fixture).await?, "update_changed");
    assert_eq!(
        fs::read(&fixture.executable)?,
        b"externally installed release"
    );
    assert_eq!(fixture.stages(), 0);
    fixture.unchanged_data();
    Ok(())
}

#[tokio::test]
async fn a_concurrent_update_keeps_the_previous_release() -> anyhow::Result<()> {
    let fixture = Fixture::new(VERSION, release_archive(), false, false, false).await?;
    let name = format!(
        ".{}.update.lock",
        fixture.executable.file_name().unwrap().to_string_lossy()
    );
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(fixture.directory.path().join(name))?;
    fs2::FileExt::try_lock_exclusive(&lock)?;
    assert_eq!(apply(&fixture).await?, "update_busy");
    assert_eq!(fixture.server.downloads.load(Ordering::Relaxed), 0);
    assert_eq!(fs::read(&fixture.executable)?, b"previous release");
    fixture.unchanged_data();
    Ok(())
}

#[tokio::test]
async fn malformed_archive_is_rejected_without_installing() -> anyhow::Result<()> {
    let fixture = Fixture::new(VERSION, b"invalid archive".to_vec(), false, false, false).await?;
    assert_eq!(apply(&fixture).await?, "update_archive");
    assert_eq!(fs::read(&fixture.executable)?, b"previous release");
    assert_eq!(fixture.stages(), 0);
    fixture.unchanged_data();
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn failed_post_install_check_restores_the_previous_executable() -> anyhow::Result<()> {
    let program = format!(
        "#!/bin/sh\ncase \"$0\" in */.moocp-update-*/moocp) printf 'moocp {VERSION}\\n';; *) exit 1;; esac\n"
    );
    let fixture = Fixture::new(VERSION, package(program.as_bytes()), false, false, false).await?;
    assert_eq!(apply(&fixture).await?, "update_executable");
    assert_eq!(fs::read(&fixture.executable)?, b"previous release");
    assert_eq!(fixture.stages(), 0);
    fixture.unchanged_data();
    Ok(())
}
