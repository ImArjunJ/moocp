use moocp::{
    config::{CredentialStorage, Credentials, Home, Profile},
    store::Store,
    uninstall::{self, ClientFile, Format},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn account(home: &Home, name: &str) -> Profile {
    let profile = Profile {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.into(),
        url: "https://example.invalid/".parse().unwrap(),
        user_id: Some(3),
        display_name: None,
        storage: CredentialStorage::File,
        allow_http: false,
        authenticated_at: chrono::Utc::now().to_rfc3339(),
    };
    home.save_profile(&profile).unwrap();
    home.save_credentials(
        &profile,
        &Credentials {
            token: Some("fixture-token".into()),
            ..Default::default()
        },
    )
    .unwrap();
    Store::open(home)
        .unwrap()
        .prepare(
            &profile.id,
            &json!({"kind":"fixture"}),
            &json!({}),
            &json!({}),
        )
        .unwrap();
    let staged = home.path.join("proposal-files").join(&profile.id);
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::write(staged.join("private-draft"), "draft").unwrap();
    profile
}

fn server(executable: &Path, root: &Path, profile: &str) -> Value {
    json!({"command":executable,"args":["--data-dir",root,"--profile",profile,"serve"]})
}

fn fake_executable(root: &Path) -> PathBuf {
    let executable = root.join("moocp");
    std::fs::write(&executable, b"fixture executable").unwrap();
    executable
}

#[tokio::test]
async fn full_cleanup_preserves_other_servers_comments_and_foreign_files() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let executable = fake_executable(directory.path());
    let home = Home {
        path: directory.path().join("data"),
    };
    let profile = account(&home, "dev");
    let other = directory.path().join("production");
    std::fs::write(home.path.join("notes.txt"), "my unrelated file")?;
    let json_path = directory.path().join("claude.json");
    std::fs::write(
        &json_path,
        serde_json::to_vec(&json!({"theme":"dark","mcpServers":{
        "dev":server(&executable,&home.path,&profile.id),"production":server(&executable,&other,"other"),
        "moocp":{"command":"unrelated-tool","args":["serve"]}
    },"projects":{"example":{"trusted":true,"mcpServers":{"dev":server(&executable,&home.path,&profile.id)}}}}))?,
    )?;
    let toml_path = directory.path().join("codex.toml");
    std::fs::write(
        &toml_path,
        format!(
            "# keep this comment\nmodel = 'example'\n[mcp_servers.dev]\ncommand = {}\nargs = {}\n[mcp_servers.unrelated]\ncommand = 'other-tool'\nargs = ['serve']\n",
            serde_json::to_string(&executable)?,
            serde_json::to_string(&server(&executable, &home.path, &profile.id)["args"])?
        ),
    )?;
    let files = [
        ClientFile {
            path: json_path.clone(),
            format: Format::Json,
        },
        ClientFile {
            path: toml_path.clone(),
            format: Format::Toml,
        },
    ];
    let result = uninstall::run(&home, None, false, &executable, &files).await?;
    assert_eq!(result.registrations, 3);
    assert_eq!(result.accounts, 1);
    assert!(result.kept_binary);
    let remaining: Value = serde_json::from_slice(&std::fs::read(json_path)?)?;
    assert_eq!(remaining["theme"], "dark");
    assert!(remaining["mcpServers"].get("dev").is_none());
    assert!(remaining["mcpServers"].get("production").is_some());
    assert!(remaining["mcpServers"].get("moocp").is_some());
    assert_eq!(remaining["projects"]["example"]["trusted"], true);
    assert!(std::fs::read_to_string(toml_path)?.starts_with("# keep this comment"));
    assert!(!home.path.join("config.json").exists());
    assert!(!home.path.join("moocp.sqlite3").exists());
    assert!(!home.credential_path(&profile).exists());
    assert!(!home.path.join("proposal-files").exists());
    assert_eq!(
        std::fs::read_to_string(home.path.join("notes.txt"))?,
        "my unrelated file"
    );
    assert!(executable.exists());
    assert_eq!(
        uninstall::run(&home, None, true, &executable, &files)
            .await?
            .registrations,
        0
    );
    Ok(())
}

#[tokio::test]
async fn profile_cleanup_keeps_other_accounts_and_the_executable() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let executable = fake_executable(directory.path());
    let home = Home {
        path: directory.path().join("data"),
    };
    let dev = account(&home, "dev");
    let production = account(&home, "production");
    let path = directory.path().join("gemini.json");
    std::fs::write(
        &path,
        serde_json::to_vec(
            &json!({"mcpServers":{"dev":server(&executable,&home.path,&dev.id),"production":server(&executable,&home.path,&production.id)}}),
        )?,
    )?;
    let result = uninstall::run(
        &home,
        Some("dev"),
        false,
        &executable,
        &[ClientFile {
            path: path.clone(),
            format: Format::Json,
        }],
    )
    .await?;
    assert_eq!(result.registrations, 1);
    assert!(result.kept_binary);
    assert!(executable.exists());
    assert_eq!(home.config()?.profiles.len(), 1);
    assert!(home.credentials(&production).is_ok());
    assert!(!home.credential_path(&dev).exists());
    assert_eq!(Store::open(&home)?.coverage(&production.id)?, json!([]));
    let state: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    assert!(state["mcpServers"].get("dev").is_none());
    assert!(state["mcpServers"].get("production").is_some());
    Ok(())
}

#[tokio::test]
async fn malformed_client_settings_leave_accounts_and_binary_intact() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let executable = fake_executable(directory.path());
    let home = Home {
        path: directory.path().join("data"),
    };
    let profile = account(&home, "dev");
    let path = directory.path().join("config.json");
    std::fs::write(&path, "{invalid")?;
    assert!(
        uninstall::run(
            &home,
            None,
            false,
            &executable,
            &[ClientFile {
                path,
                format: Format::Json
            }]
        )
        .await
        .is_err()
    );
    assert!(home.credentials(&profile).is_ok());
    assert_eq!(home.config()?.profiles.len(), 1);
    assert!(executable.exists());
    Ok(())
}

async fn start_server(binary: &Path, root: &Path) -> anyhow::Result<tokio::process::Child> {
    let mut child = tokio::process::Command::new(binary)
        .arg("--data-dir")
        .arg(root)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    child.stdin.as_mut().unwrap().write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"clientInfo\":{\"name\":\"uninstall-test\",\"version\":\"1\"}}}\n").await?;
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line)).await??;
    assert!(serde_json::from_str::<Value>(&line)?["result"].is_object());
    Ok(child)
}

#[tokio::test]
async fn stops_only_servers_for_the_removed_data_directory() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let binary = directory
        .path()
        .join(if cfg!(windows) { "moocp.exe" } else { "moocp" });
    std::fs::copy(env!("CARGO_BIN_EXE_moocp"), &binary)?;
    let home = Home {
        path: directory.path().join("dev"),
    };
    let other = directory.path().join("other");
    let mut owned = start_server(&binary, &home.path).await?;
    let mut preserved = start_server(&binary, &other).await?;
    #[cfg(unix)]
    {
        let replacement = directory.path().join("replacement");
        std::fs::copy(&binary, &replacement)?;
        std::fs::rename(replacement, &binary)?;
    }
    let result = uninstall::run(&home, None, false, &binary, &[]).await?;
    assert!(result.kept_binary);
    assert_eq!(result.retained_reason, Some("other running processes"));
    tokio::time::timeout(Duration::from_secs(2), owned.wait()).await??;
    assert!(preserved.try_wait()?.is_none());
    preserved.kill().await?;
    preserved.wait().await?;
    Ok(())
}

#[tokio::test]
async fn cli_uninstall_removes_its_own_copied_executable() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let binary = directory
        .path()
        .join(if cfg!(windows) { "moocp.exe" } else { "moocp" });
    std::fs::copy(env!("CARGO_BIN_EXE_moocp"), &binary)?;
    let lock = directory.path().join(format!(
        ".{}.update.lock",
        binary.file_name().unwrap().to_string_lossy()
    ));
    let failed_update = directory.path().join(".moocp-update-leftover");
    std::fs::create_dir(&failed_update)?;
    std::fs::write(
        failed_update.join("target.json"),
        serde_json::to_vec(&binary.canonicalize()?)?,
    )?;
    std::fs::write(failed_update.join("previous"), b"old update backup")?;
    let home = Home {
        path: directory.path().join("data"),
    };
    account(&home, "dev");
    let output = tokio::process::Command::new(&binary)
        .arg("--data-dir")
        .arg(&home.path)
        .arg("uninstall")
        .output()
        .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for _ in 0..100 {
        if !binary.exists() && !lock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !binary.exists(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!home.path.exists());
    assert!(!lock.exists());
    assert!(!failed_update.exists());
    Ok(())
}
