//! Remove one installation's registrations and private data without touching other servers.

use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, Signal, System, UpdateKind};

use crate::{
    config::{Home, Profile, private_write},
    error::{Error, Result},
    store::Store,
};

#[derive(Clone, Copy)]
pub enum Format {
    Toml,
    Json,
}

pub struct ClientFile {
    pub path: PathBuf,
    pub format: Format,
}

pub struct Report {
    pub registrations: usize,
    pub accounts: usize,
    pub kept_binary: bool,
    pub deletion_pending: bool,
    pub retained_reason: Option<&'static str>,
}

pub fn client_files() -> Result<Vec<ClientFile>> {
    let user = directories::BaseDirs::new()
        .ok_or_else(|| Error::local("Cannot find your home directory."))?;
    let mut files = vec![
        ClientFile {
            path: user.home_dir().join(".codex/config.toml"),
            format: Format::Toml,
        },
        ClientFile {
            path: user.home_dir().join(".claude.json"),
            format: Format::Json,
        },
        ClientFile {
            path: user.home_dir().join(".claude/.claude.json"),
            format: Format::Json,
        },
        ClientFile {
            path: user.home_dir().join(".gemini/settings.json"),
            format: Format::Json,
        },
    ];
    if let Some(root) = std::env::var_os("CODEX_HOME") {
        files.push(ClientFile {
            path: PathBuf::from(root).join("config.toml"),
            format: Format::Toml,
        });
    }
    if let Some(root) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        let mut adjacent = root.clone();
        adjacent.push(".json");
        files.push(ClientFile {
            path: PathBuf::from(adjacent),
            format: Format::Json,
        });
        files.push(ClientFile {
            path: PathBuf::from(root).join(".claude.json"),
            format: Format::Json,
        });
    }
    let mut seen = BTreeSet::new();
    files.retain(|file| seen.insert(normalise(&file.path)));
    Ok(files)
}

struct Scope {
    root: PathBuf,
    default_root: PathBuf,
    executable: PathBuf,
    selected: Option<BTreeSet<String>>,
    active: Option<String>,
}

impl Scope {
    fn is_executable(&self, command: &str) -> bool {
        let path = Path::new(command);
        normalise(path) == self.executable
            || path
                .file_stem()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("moocp"))
    }

    fn references_binary(&self, command: &str) -> bool {
        normalise(Path::new(command)) == self.executable || matches!(command, "moocp" | "moocp.exe")
    }

    fn owns(&self, command: &str, args: &[String]) -> bool {
        if !self.is_executable(command) || args.last().is_none_or(|arg| arg != "serve") {
            return false;
        }
        self.matches_data(args)
    }

    fn owns_process(&self, command: &str, args: &[String]) -> bool {
        // Linux keeps the previous inode running after an atomic update.
        let executable = command.strip_suffix(" (deleted)").unwrap_or(command);
        let backup = Path::new(executable);
        let running_previous = backup
            .file_name()
            .is_some_and(|name| name == "previous.exe")
            && backup
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name.to_string_lossy().starts_with(".moocp-update-"))
            && args
                .first()
                .is_some_and(|arg| normalise(Path::new(arg)) == self.executable);
        (self.is_executable(executable) || running_previous) && self.matches_data(args)
    }

    fn matches_data(&self, args: &[String]) -> bool {
        let root = argument(args, "--data-dir")
            .map(|value| normalise(Path::new(value)))
            .unwrap_or_else(|| self.default_root.clone());
        if root != self.root {
            return false;
        }
        self.selected.as_ref().is_none_or(|selected| {
            argument(args, "--profile")
                .or_else(|| {
                    args.iter()
                        .any(|arg| arg == "setup")
                        .then(|| argument(args, "--name").unwrap_or("default"))
                })
                .or(self.active.as_deref())
                .is_some_and(|profile| selected.contains(profile))
        })
    }
}

fn argument<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().enumerate().find_map(|(index, arg)| {
        if arg == name {
            args.get(index + 1).map(String::as_str)
        } else {
            arg.strip_prefix(&format!("{name}="))
        }
    })
}

fn normalise(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        };
        let mut clean = PathBuf::new();
        for part in absolute.components() {
            match part {
                std::path::Component::CurDir => (),
                std::path::Component::ParentDir => {
                    clean.pop();
                }
                other => clean.push(other.as_os_str()),
            }
        }
        clean
    })
}

struct Edit {
    path: PathBuf,
    before: Vec<u8>,
    after: Vec<u8>,
}

fn prepare_edits(files: &[ClientFile], scope: &Scope) -> Result<(Vec<Edit>, usize, bool)> {
    let mut edits = Vec::new();
    let mut removed = 0;
    let mut other_references = false;
    for file in files {
        let before = match fs::read(&file.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let count_before = removed;
        let after = match file.format {
            Format::Toml => {
                let text = std::str::from_utf8(&before).map_err(|_| {
                    Error::local("Codex config is not valid UTF-8; it was left unchanged.")
                })?;
                let mut document = text.parse::<toml_edit::DocumentMut>().map_err(|_| {
                    Error::local("Codex config is not valid TOML; it was left unchanged.")
                })?;
                if let Some(servers) = document
                    .get_mut("mcp_servers")
                    .and_then(toml_edit::Item::as_table_like_mut)
                {
                    let names: Vec<_> = servers
                        .iter()
                        .filter_map(|(name, entry)| {
                            let command = entry.get("command")?.as_str()?;
                            let args: Option<Vec<_>> = entry
                                .get("args")
                                .and_then(toml_edit::Item::as_array)
                                .map(|args| {
                                    args.iter()
                                        .map(|arg| arg.as_str().map(str::to_owned))
                                        .collect()
                                })
                                .unwrap_or(Some(Vec::new()));
                            let Some(args) = args else {
                                other_references |= scope.references_binary(command);
                                return None;
                            };
                            if scope.owns(command, &args) {
                                Some(name.to_owned())
                            } else {
                                other_references |= scope.references_binary(command);
                                None
                            }
                        })
                        .collect();
                    for name in names {
                        servers.remove(&name);
                        removed += 1;
                    }
                }
                document.to_string().into_bytes()
            }
            Format::Json => {
                let mut settings: Value = serde_json::from_slice(&before).map_err(|_| {
                    Error::local("A CLI config is not valid JSON; it was left unchanged.")
                })?;
                prune_json(&mut settings, scope, &mut removed, &mut other_references);
                if let Some(projects) = settings.get_mut("projects").and_then(Value::as_object_mut)
                {
                    for project in projects.values_mut() {
                        prune_json(project, scope, &mut removed, &mut other_references);
                    }
                }
                serde_json::to_vec_pretty(&settings)?
            }
        };
        if removed != count_before {
            edits.push(Edit {
                path: file.path.clone(),
                before,
                after,
            });
        }
    }
    Ok((edits, removed, other_references))
}

fn prune_json(settings: &mut Value, scope: &Scope, removed: &mut usize, others: &mut bool) {
    if let Some(servers) = settings
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
    {
        servers.retain(|_, entry| {
            let Some(command) = entry["command"].as_str() else {
                return true;
            };
            let Some(args) = entry["args"].as_array().and_then(|args| {
                args.iter()
                    .map(|arg| arg.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            }) else {
                *others |= scope.references_binary(command);
                return true;
            };
            if scope.owns(command, &args) {
                *removed += 1;
                false
            } else {
                *others |= scope.references_binary(command);
                true
            }
        });
    }
}

pub async fn run(
    home: &Home,
    profile: Option<&str>,
    keep_binary: bool,
    executable: &Path,
    files: &[ClientFile],
) -> Result<Report> {
    let _update_lock = crate::update::UpdateLock::acquire(executable)?;
    let mut config = home.config()?;
    let accounts: Vec<Profile> = config
        .profiles
        .iter()
        .filter(|account| {
            profile.is_none_or(|wanted| account.id == wanted || account.name == wanted)
        })
        .cloned()
        .collect();
    for account in &accounts {
        crate::config::validate_profile_id(&account.id)?;
    }
    let scope = Scope {
        root: normalise(&home.path),
        default_root: normalise(&Home::discover(None)?.path),
        executable: normalise(executable),
        active: config.active_profile.clone(),
        selected: profile.map(|wanted| {
            accounts
                .iter()
                .flat_map(|account| [account.id.clone(), account.name.clone()])
                .chain([wanted.to_owned()])
                .collect()
        }),
    };
    // Parse every config before modifying any; never erase private data on a config error.
    let (edits, registrations, other_references) = prepare_edits(files, &scope)?;
    for edit in &edits {
        if fs::read(&edit.path)? != edit.before {
            return Err(Error::local(
                "CLI settings changed during uninstall. Run it again.",
            ));
        }
        private_write(&edit.path, &edit.after)?;
    }
    let other_processes = stop_processes(&scope).await?;
    for account in &accounts {
        // Keep account metadata and the executable if OS credential deletion fails, so retry works.
        home.remove_credentials(account)?;
        remove_file(&home.credential_path(account))?;
        home.clear_staged_files(account)?;
    }
    if home.path.exists() {
        for entry in fs::read_dir(&home.path)? {
            let directory = entry?.path();
            if browser_directory(&directory, &scope.root) && browser_belongs(&directory, &scope) {
                fs::remove_dir_all(directory)?;
            }
        }
    }
    if profile.is_some() {
        if home.path.join("moocp.sqlite3").exists() {
            let store = Store::open(home)?;
            for account in &accounts {
                store.clear_profile(&account.id)?;
            }
        }
        config
            .profiles
            .retain(|account| !accounts.iter().any(|removed| removed.id == account.id));
        if config
            .active_profile
            .as_ref()
            .is_some_and(|id| accounts.iter().any(|account| &account.id == id))
        {
            config.active_profile = config.profiles.first().map(|account| account.id.clone());
        }
        if !accounts.is_empty() {
            home.save_config(&config)?;
        }
    } else {
        for filename in [
            "moocp.sqlite3",
            "moocp.sqlite3-wal",
            "moocp.sqlite3-shm",
            "config.json",
        ] {
            remove_file(&home.path.join(filename))?;
        }
        for directory in ["credentials", "proposal-files"] {
            remove_empty(&home.path.join(directory))?;
        }
        remove_empty(&home.path)?;
    }
    let kept_binary = keep_binary || profile.is_some() || other_references || other_processes;
    let deletion_pending = if kept_binary {
        false
    } else {
        crate::update::remove_staging(&scope.executable)?;
        remove_executable(executable)?
    };
    Ok(Report {
        registrations,
        accounts: accounts.len(),
        kept_binary,
        deletion_pending,
        retained_reason: if keep_binary || profile.is_some() {
            Some("requested")
        } else if other_references {
            Some("other registrations")
        } else if other_processes {
            Some("other running processes")
        } else {
            None
        },
    })
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_empty(path: &Path) -> Result<()> {
    if path.is_dir() && fs::read_dir(path)?.next().is_none() {
        fs::remove_dir(path)?;
    }
    Ok(())
}

fn cargo_install_root(executable: &Path) -> Result<Option<PathBuf>> {
    let Some(bin) = executable
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "bin"))
    else {
        return Ok(None);
    };
    if executable.file_stem().is_none_or(|name| name != "moocp") {
        return Ok(None);
    }
    let Some(root) = bin.parent() else {
        return Ok(None);
    };
    let metadata = match fs::read_to_string(root.join(".crates.toml")) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let document = metadata.parse::<toml_edit::DocumentMut>().map_err(|_| {
        Error::local("Cargo installation metadata is invalid. Repair it and run uninstall again.")
    })?;
    let managed = document
        .get("v1")
        .and_then(toml_edit::Item::as_table_like)
        .is_some_and(|packages| {
            packages.iter().any(|(name, bins)| {
                name.starts_with("moocp ")
                    && bins
                        .as_array()
                        .is_some_and(|bins| bins.iter().any(|bin| bin.as_str() == Some("moocp")))
            })
        });
    Ok(managed.then(|| root.to_owned()))
}

fn browser_directory(path: &Path, root: &Path) -> bool {
    path.parent()
        .is_some_and(|parent| normalise(parent) == root)
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.strip_prefix("browser-")
                    .or_else(|| name.strip_prefix("login-"))
                    .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
            })
}

fn browser_belongs(directory: &Path, scope: &Scope) -> bool {
    scope.selected.as_ref().is_none_or(|selected| {
        fs::read(directory.join("moocp-profile.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|marker| {
                ["profile", "name"].iter().any(|key| {
                    marker[*key]
                        .as_str()
                        .is_some_and(|name| selected.contains(name))
                })
            })
    })
}

async fn stop_processes(scope: &Scope) -> Result<bool> {
    use futures::StreamExt;
    let mut system = System::new();
    let refresh = ProcessRefreshKind::nothing()
        .without_tasks()
        .with_exe(UpdateKind::Always)
        .with_cmd(UpdateKind::Always);
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
    let own = sysinfo::Pid::from_u32(std::process::id());
    let other_processes = system.processes().iter().any(|(pid, process)| {
        let args: Vec<_> = process
            .cmd()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        *pid != own
            && process.exe().is_some_and(|path| {
                let text = path.to_string_lossy();
                normalise(Path::new(text.strip_suffix(" (deleted)").unwrap_or(&text)))
                    == scope.executable
            })
            && !scope.owns_process(&scope.executable.to_string_lossy(), &args)
    });
    let mut targets: BTreeSet<_> = system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            let executable = process.exe()?;
            let args: Vec<_> = process
                .cmd()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            if *pid != own && scope.owns_process(&executable.to_string_lossy(), &args) {
                Some(*pid)
            } else {
                None
            }
        })
        .collect();
    let directories: Vec<_> = if scope.root.exists() {
        fs::read_dir(&scope.root)?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| browser_directory(path, &scope.root))
            .collect()
    } else {
        Vec::new()
    };
    for directory in directories {
        let belongs = browser_belongs(&directory, scope)
            || targets.iter().any(|pid| {
                system
                    .process(*pid)
                    .is_some_and(|process| uses_browser_directory(process.cmd(), &directory))
            });
        if belongs {
            // Close Chrome over its own local debugging endpoint before deleting its profile.
            if let Ok(port) = fs::read_to_string(directory.join("DevToolsActivePort")) {
                let mut lines = port.lines();
                if let (Some(number), Some(path)) = (
                    lines.next().and_then(|line| line.parse::<u16>().ok()),
                    lines.next(),
                ) && path.starts_with("/devtools/browser/")
                {
                    let close = async {
                        if let Ok((mut browser, mut handler)) = chromiumoxide::Browser::connect(
                            format!("ws://127.0.0.1:{number}{path}"),
                        )
                        .await
                        {
                            let driver =
                                tokio::spawn(
                                    async move { while handler.next().await.is_some() {} },
                                );
                            let _ = browser.close().await;
                            driver.abort();
                        }
                    };
                    let _ = tokio::time::timeout(Duration::from_secs(3), close).await;
                }
            }
            for (pid, process) in system.processes() {
                if uses_browser_directory(process.cmd(), &directory) {
                    targets.insert(*pid);
                }
            }
        }
    }
    // Include only descendants of those exact servers, and isolated browsers inside this data directory.
    loop {
        let before = targets.len();
        for (pid, process) in system.processes() {
            if *pid != own
                && process
                    .parent()
                    .is_some_and(|parent| targets.contains(&parent))
            {
                targets.insert(*pid);
            }
        }
        if targets.len() == before {
            break;
        }
    }
    for pid in &targets {
        if let Some(process) = system.process(*pid) {
            let _ = process
                .kill_with(Signal::Term)
                .or_else(|| process.kill_with(Signal::Kill));
        }
    }
    for _ in 0..30 {
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&targets.iter().copied().collect::<Vec<_>>()),
            true,
            ProcessRefreshKind::nothing().without_tasks(),
        );
        if targets.iter().all(|pid| {
            system.process(*pid).is_none_or(|process| {
                matches!(
                    process.status(),
                    sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
                )
            })
        }) {
            return Ok(other_processes);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for pid in &targets {
        if let Some(process) = system.process(*pid) {
            let _ = process.kill();
        }
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&targets.iter().copied().collect::<Vec<_>>()),
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    if targets.iter().any(|pid| {
        system.process(*pid).is_some_and(|process| {
            !matches!(
                process.status(),
                sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
            )
        })
    }) {
        return Err(Error::local(
            "Could not stop a running Moocp server. Close that CLI session and run uninstall again.",
        ));
    }
    Ok(other_processes)
}

fn uses_browser_directory(args: &[std::ffi::OsString], directory: &Path) -> bool {
    // Chrome flattens argv on Linux. Resolve candidate paths rather than splitting
    // on spaces: profile paths can contain spaces or use /var's macOS symlink.
    let wanted = normalise(directory);
    let flag = "--user-data-dir=";
    args.iter().any(|arg| {
        let text = arg.to_string_lossy();
        text.match_indices(flag).any(|(index, _)| {
            if index != 0 && !matches!(text.as_bytes()[index - 1], b' ' | b'"') {
                return false;
            }
            let value = &text[index + flag.len()..];
            value
                .char_indices()
                .filter_map(|(end, character)| matches!(character, ' ' | '"').then_some(end))
                .chain([value.len()])
                .any(|end| {
                    let candidate = value[..end].trim_matches('"');
                    !candidate.is_empty() && normalise(Path::new(candidate)) == wanted
                })
        })
    })
}

#[cfg(not(windows))]
fn remove_executable(path: &Path) -> Result<bool> {
    if let Some(root) = cargo_install_root(path)? {
        let status = std::process::Command::new("cargo").args(["uninstall", "--quiet", "--root"]).arg(root).args(["--bin", "moocp", "moocp"]).status()
            .map_err(|_| Error::local("Could not run Cargo's uninstaller. Restore cargo on PATH and run uninstall again."))?;
        if !status.success() {
            return Err(Error::local(
                "Cargo could not finish uninstalling Moocp. Run uninstall again.",
            ));
        }
    } else {
        remove_file(path)?;
    }
    remove_file(&crate::update::lock_path(path))?;
    Ok(false)
}

#[cfg(windows)]
fn remove_executable(path: &Path) -> Result<bool> {
    use std::{
        os::windows::process::CommandExt,
        process::{Command, Stdio},
    };
    let directory = std::env::temp_dir().join(format!("moocp-uninstall-{}", uuid::Uuid::new_v4()));
    let script = directory.join("remove.ps1");
    let cargo_root = cargo_install_root(path)?;
    private_write(
        &script,
        br#"param([int]$ProcessId, [string]$Executable, [string]$CargoRoot, [string]$UpdateLock)
$ErrorActionPreference = 'Stop'
Wait-Process -Id $ProcessId -ErrorAction SilentlyContinue
for ($attempt = 0; $attempt -lt 100; $attempt++) {
    try {
        if ($CargoRoot) {
            & cargo uninstall --quiet --root $CargoRoot --bin moocp moocp
            if ($LASTEXITCODE -ne 0) { throw 'Cargo uninstall failed.' }
        } elseif (Test-Path -LiteralPath $Executable) { Remove-Item -LiteralPath $Executable -Force }
        break
    } catch { Start-Sleep -Milliseconds 100 }
}
if (-not (Test-Path -LiteralPath $Executable)) { Remove-Item -LiteralPath $UpdateLock -Force -ErrorAction SilentlyContinue }
Remove-Item -LiteralPath $PSScriptRoot -Recurse -Force
"#,
    )?;
    let mut command = Command::new("powershell.exe");
    command
        .env_remove("PSModulePath")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script)
        .arg("-ProcessId")
        .arg(std::process::id().to_string())
        .arg("-Executable")
        .arg(path)
        .arg("-UpdateLock")
        .arg(crate::update::lock_path(path));
    if let Some(root) = cargo_root {
        command.arg("-CargoRoot").arg(root);
    }
    command
        .creation_flags(0x08000000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| {
            Error::local("Could not start Windows executable cleanup. Run uninstall again.")
        })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_cleanup_requires_this_binary_in_the_installation_record() {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let executable = bin.join("moocp");
        fs::write(&executable, b"fixture").unwrap();
        let metadata = directory.path().join(".crates.toml");
        fs::write(
            &metadata,
            "[v1]\n'other 1.0.0 (registry+example)' = ['other']\n",
        )
        .unwrap();
        assert!(cargo_install_root(&executable).unwrap().is_none());
        fs::write(&metadata, "[v1]\n'moocp 0.1.0 (path+file:///example)' = ['moocp']\n'other 1.0.0 (registry+example)' = ['other']\n").unwrap();
        assert_eq!(
            cargo_install_root(&executable).unwrap(),
            Some(directory.path().to_owned())
        );
        assert!(fs::read_to_string(metadata).unwrap().contains("other"));
    }
}
