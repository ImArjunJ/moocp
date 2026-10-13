//! Native setup and client registration, with absolute server paths and preserved existing settings.

use std::{
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
    process::Command,
};

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};

use crate::{
    auth::{LoginOptions, enrich_mobile_token},
    config::{CredentialStorage, Home, Profile, site_url},
    error::Error,
    moodle::Moodle,
    store::Store,
};

#[derive(Parser)]
#[command(version,about="Your Moodle account, connected to your CLI through MCP",long_about=None)]
struct Cli {
    #[arg(long, global = true, env = "MOOCP_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    profile: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Sign in with university SSO/MFA and optionally connect an installed CLI.
    Setup {
        url: String,
        #[arg(long, default_value = "default")]
        name: String,
        #[arg(long, value_enum, default_value = "keyring")]
        credential_store: StorageArg,
        #[arg(long, value_enum)]
        client: Option<ClientArg>,
        #[arg(long)]
        browser: Option<PathBuf>,
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        /// Explicitly allow HTTP for a local/development Moodle instance.
        #[arg(long)]
        allow_http: bool,
    },
    /// Reconnect an existing Moodle account in a browser.
    Login {
        #[arg(long)]
        browser: Option<PathBuf>,
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        /// Import a mobile-service token from stdin instead of browser login.
        #[arg(long)]
        token_stdin: bool,
    },
    /// Run the local MCP server over stdio. Stdout is reserved for MCP messages.
    Serve,
    /// Show local account status; --check also verifies it against Moodle.
    Status {
        #[arg(long)]
        check: bool,
    },
    /// Check browser availability, local storage, and the live Moodle connection.
    Doctor,
    /// Register this executable with a supported CLI without changing other servers.
    Install {
        #[arg(value_enum)]
        client: ClientArg,
        #[arg(long)]
        replace: bool,
    },
    /// Print a ready-to-use client configuration without writing it.
    Config {
        #[arg(value_enum, default_value = "json")]
        format: ConfigFormat,
    },
    /// Choose the default account, or list accounts when no name is provided.
    Account { name: Option<String> },
    /// Remove local credentials and cached private account content.
    Logout,
    /// Review an exact pending action in your own terminal and approve or reject it.
    Review {
        proposal_id: String,
        #[arg(long)]
        execute: bool,
    },
    /// Inspect a proposal or receipt without showing private upload bytes.
    Action { proposal_id: String },
}

#[derive(Clone, Copy, ValueEnum)]
enum StorageArg {
    Keyring,
    File,
}
#[derive(Clone, Copy, ValueEnum)]
pub enum ClientArg {
    Codex,
    Claude,
    Gemini,
}
#[derive(Clone, Copy, ValueEnum)]
enum ConfigFormat {
    Json,
    Codex,
}

pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let home = Home::discover(cli.data_dir)?;
    match cli.command {
        Commands::Serve => crate::server::serve(home, cli.profile).await?,
        Commands::Setup {
            url,
            name,
            credential_store,
            client,
            browser,
            timeout,
            allow_http,
        } => {
            validate_name(&name)?;
            let url = site_url(&url, allow_http)?;
            let config = home.config()?;
            let existing = config.profiles.iter().find(|p| p.name == name);
            if existing.is_some_and(|p| p.url != url) {
                return Err(Error::invalid(
                    "That account name already belongs to another site. Choose a new --name.",
                )
                .into());
            }
            let mut profile = Profile {
                id: existing
                    .map(|p| p.id.clone())
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                name,
                url,
                user_id: None,
                display_name: None,
                storage: match credential_store {
                    StorageArg::Keyring => CredentialStorage::Keyring,
                    StorageArg::File => CredentialStorage::File,
                },
                allow_http,
                authenticated_at: String::new(),
            };
            sign_in(&home, &mut profile, browser, timeout).await?;
            let client = client.or_else(pick_client);
            if let Some(client) = client {
                install(&home, &profile, client, false)?;
            } else {
                println!(
                    "Run moocp install codex, moocp install claude, or moocp config to connect a CLI."
                );
            }
        }
        Commands::Login {
            browser,
            timeout,
            token_stdin,
        } => {
            let mut profile = home.profile(cli.profile.as_deref())?;
            if token_stdin {
                let mut token = String::new();
                io::stdin().take(4096).read_to_string(&mut token)?;
                let token = token.trim().to_string();
                if token.is_empty() || token.len() > 2048 {
                    return Err(Error::invalid(
                        "Expected a nonempty Moodle service token on stdin.",
                    )
                    .into());
                }
                let mut credentials = home.credentials(&profile).unwrap_or_default();
                credentials.token = Some(token);
                let m = Moodle::new(profile.clone(), credentials.clone())?;
                let raw = m.rest("core_webservice_get_site_info", json!({})).await?;
                if raw["userid"].as_u64().is_none() || raw["userid"].as_u64() != profile.user_id {
                    credentials.cookies.clear();
                    credentials.sesskey = None;
                }
                profile.user_id = raw["userid"].as_u64();
                profile.display_name = raw["fullname"].as_str().map(str::to_string);
                profile.authenticated_at = chrono::Utc::now().to_rfc3339();
                Store::open(&home)?.clear_profile(&profile.id)?;
                home.clear_staged_files(&profile)?;
                home.save_credentials(&profile, &credentials)?;
                home.save_profile(&profile)?;
                println!(
                    "Connected to {} as {}.",
                    profile.url,
                    profile
                        .display_name
                        .as_deref()
                        .unwrap_or("your Moodle account")
                );
            } else {
                sign_in(&home, &mut profile, browser, timeout).await?;
            }
        }
        Commands::Status { check } => {
            let config = home.config()?;
            if config.profiles.is_empty() {
                print_json(
                    &json!({"configured":false,"setup":"moocp setup https://your-moodle.example"}),
                )?;
                return Ok(());
            }
            let profile = home.profile(cli.profile.as_deref())?;
            let credentials = home.credentials(&profile).ok();
            let mut value = json!({"profile":profile.name,"site":profile.url,"display_name":profile.display_name,"authenticated_at":profile.authenticated_at,"credentials_available":credentials.is_some(),"mobile_token":credentials.as_ref().is_some_and(|c|c.token.is_some()),"credential_storage":profile.storage,"index_coverage":Store::open(&home)?.coverage(&profile.id)?});
            if check {
                let credentials = credentials.ok_or_else(Error::auth)?;
                value["live"] = Moodle::new(profile, credentials)?.validate().await?;
            }
            print_json(&value)?;
        }
        Commands::Doctor => {
            let browser = chromiumoxide::BrowserConfig::builder()
                .respect_https_errors()
                .build()
                .is_ok();
            let local = home.ensure().and_then(|_| Store::open(&home).map(|_| ()));
            let connection = match home.profile(cli.profile.as_deref()) {
                Ok(p) => match home.credentials(&p) {
                    Ok(c) => match Moodle::new(p, c)?.validate().await {
                        Ok(v) => v,
                        Err(e) => json!({"error":e}),
                    },
                    Err(e) => json!({"error":e}),
                },
                Err(e) => json!({"error":e}),
            };
            print_json(
                &json!({"version":env!("CARGO_PKG_VERSION"),"platform":std::env::consts::OS,"browser_found":browser,"data_directory":home.path,"local_storage":local.is_ok(),"connection":connection,"help":"A browser is needed for SSO. Linux OS credential storage needs an unlocked Secret Service. Use --credential-store file only when you intentionally choose private file storage."}),
            )?;
        }
        Commands::Install { client, replace } => install(
            &home,
            &home.profile(cli.profile.as_deref())?,
            client,
            replace,
        )?,
        Commands::Config { format } => {
            let (command, args) = server_command(&home, cli.profile.as_deref())?;
            match format {
                ConfigFormat::Json => {
                    print_json(&json!({"mcpServers":{"moocp":{"command":command,"args":args}}}))?
                }
                ConfigFormat::Codex => {
                    println!(
                        "[mcp_servers.moocp]\ncommand = {}\nargs = {}",
                        serde_json::to_string(&command)?,
                        serde_json::to_string(&args)?
                    );
                }
            }
        }
        Commands::Account { name } => {
            let mut config = home.config()?;
            if let Some(name) = name {
                let id = config
                    .profiles
                    .iter()
                    .find(|p| p.name == name || p.id == name)
                    .map(|p| p.id.clone())
                    .ok_or_else(|| Error::invalid("No account with that name."))?;
                config.active_profile = Some(id);
                home.save_config(&config)?;
                println!("Active account: {name}");
            } else {
                print_json(
                    &json!({"accounts":config.profiles.iter().map(|p|json!({"name":p.name,"site":p.url,"display_name":p.display_name,"active":config.active_profile.as_deref()==Some(&p.id)})).collect::<Vec<_>>()}),
                )?;
            }
        }
        Commands::Logout => {
            let profile = home.profile(cli.profile.as_deref())?;
            home.remove_credentials(&profile)?;
            Store::open(&home)?.clear_profile(&profile.id)?;
            home.clear_staged_files(&profile)?;
            println!(
                "Removed local credentials and cached content for {}. Moodle service tokens can also be revoked in your Moodle security settings.",
                profile.name
            );
        }
        Commands::Review {
            proposal_id,
            execute,
        } => {
            if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                return Err(Error::new("interactive_review_required","Review actions in your own terminal. Piped or unattended approval is not accepted.").into());
            }
            let store = Store::open(&home)?;
            let proposal = store.proposal(&proposal_id)?;
            let profile = home.profile(cli.profile.as_deref())?;
            if proposal["profile"] != profile.id {
                return Err(Error::new(
                    "account_mismatch",
                    "Switch to the account that owns this proposal before reviewing it.",
                )
                .into());
            }
            println!(
                "Action {}\nStatus: {}\nExpires: {}",
                proposal_id, proposal["status"], proposal["expires_at"]
            );
            if proposal["status"] != "prepared" {
                return Err(Error::new("proposal_state","Only a prepared proposal can be reviewed. Inspect its receipt with moocp action.").into());
            }
            print_json(&proposal["preview"])?;
            print!("Type approve to approve this exact action, or press Enter to reject: ");
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            if input.trim() == "approve" {
                store.approve(&proposal_id)?;
                drop(store);
                if execute && proposal["payload"]["browser"] != true {
                    let credentials = home.credentials(&profile)?;
                    let receipt = crate::actions::execute(
                        &Moodle::new(profile, credentials)?,
                        &home,
                        &proposal_id,
                    )
                    .await?;
                    print_json(&receipt)?;
                } else {
                    println!(
                        "Approved. Your CLI can now call moodle_execute_action with proposal_id {proposal_id}."
                    );
                    if execute {
                        println!(
                            "This browser action must execute in the MCP session that owns its open browser."
                        );
                    }
                }
            } else {
                store.finish(
                    &proposal_id,
                    "rejected",
                    &json!({"reason":"Rejected by student during interactive review."}),
                )?;
                println!("Rejected; Moodle was not changed.");
            }
        }
        Commands::Action { proposal_id } => {
            let mut proposal = Store::open(&home)?.proposal(&proposal_id)?;
            if let Some(o) = proposal.as_object_mut() {
                o.remove("payload");
                o.remove("preconditions");
            }
            print_json(&proposal)?;
        }
    }
    Ok(())
}

async fn sign_in(
    home: &Home,
    profile: &mut Profile,
    browser: Option<PathBuf>,
    timeout: u64,
) -> anyhow::Result<()> {
    if matches!(profile.storage, CredentialStorage::Keyring) {
        keyring::Entry::new("moocp",&profile.id).map_err(|_|Error::new("credential_store_unavailable","OS credential storage is unavailable. Unlock it, or explicitly use setup --credential-store file."))?;
    } else {
        eprintln!(
            "Credentials will be stored in private local files in {}.",
            home.path.display()
        );
    }
    eprintln!("Opening Moodle sign-in. Complete university SSO/MFA in the browser.");
    let mut credentials = crate::auth::login(
        home,
        profile,
        LoginOptions {
            browser,
            headless: false,
            timeout_seconds: timeout,
        },
    )
    .await?;
    match enrich_mobile_token(profile, &mut credentials).await {
        Ok(true) => eprintln!("Moodle mobile service access is available."),
        Ok(false) => {
            eprintln!("Using your browser session. Some actions may require mobile service access.")
        }
        Err(e) => {
            credentials.token = None;
            eprintln!(
                "Mobile service discovery: {} Using the browser session.",
                e.message
            );
        }
    }
    let m = Moodle::new(profile.clone(), credentials.clone())?;
    m.validate().await?;
    // A fresh sign-in can select another user at the SSO provider. Do not reuse
    // cached private material or prepared actions from the previous session.
    Store::open(home)?.clear_profile(&profile.id)?;
    home.clear_staged_files(profile)?;
    home.save_credentials(profile, &credentials)?;
    home.save_profile(profile)?;
    println!(
        "Connected to {} as {}.",
        profile.url,
        profile
            .display_name
            .as_deref()
            .unwrap_or("your Moodle account")
    );
    Ok(())
}

fn validate_name(name: &str) -> crate::error::Result<()> {
    if name.is_empty()
        || name.len() > 50
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        Err(Error::invalid(
            "Account names must contain 1–50 letters, digits, hyphens or underscores.",
        ))
    } else {
        Ok(())
    }
}
fn print_json(value: &Value) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
fn server_command(home: &Home, profile: Option<&str>) -> anyhow::Result<(String, Vec<String>)> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let data = if home.path.is_absolute() {
        home.path.clone()
    } else {
        std::env::current_dir()?.join(&home.path)
    };
    let mut args = vec!["--data-dir".into(), data.to_string_lossy().into_owned()];
    if let Some(profile) = profile {
        args.extend(["--profile".into(), profile.into()]);
    }
    args.push("serve".into());
    Ok((executable.to_string_lossy().into_owned(), args))
}
fn installed(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}
fn pick_client() -> Option<ClientArg> {
    let choices = [
        (ClientArg::Codex, "codex"),
        (ClientArg::Claude, "claude"),
        (ClientArg::Gemini, "gemini"),
    ]
    .into_iter()
    .filter(|(_, cmd)| installed(cmd))
    .collect::<Vec<_>>();
    if choices.len() == 1 {
        return Some(choices[0].0);
    }
    if choices.is_empty() || !io::stdin().is_terminal() {
        return None;
    }
    println!("Connect a CLI:");
    for (i, (_, name)) in choices.iter().enumerate() {
        println!("  {}. {name}", i + 1);
    }
    print!("Choose a number, or press Enter to configure later: ");
    io::stdout().flush().ok()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input).ok()?;
    input
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|i| choices.get(i))
        .map(|(c, _)| *c)
}

fn install(home: &Home, profile: &Profile, client: ClientArg, replace: bool) -> anyhow::Result<()> {
    let (executable, args) = server_command(home, Some(&profile.id))?;
    let name = if profile.name == "default" {
        "moocp".to_string()
    } else {
        format!("moocp-{}", profile.name)
    };
    match client {
        ClientArg::Codex | ClientArg::Claude => {
            let cmd = if matches!(client, ClientArg::Codex) {
                "codex"
            } else {
                "claude"
            };
            let exists = Command::new(cmd)
                .args(["mcp", "get", &name])
                .output()?
                .status
                .success();
            if exists && !replace {
                return Err(Error::new("existing_connection",format!("{cmd} already has a {name} server. Its configuration was preserved. Use moocp install {cmd} --replace to replace that entry.")).into());
            }
            if exists && matches!(client, ClientArg::Claude) {
                let status = Command::new(cmd)
                    .args(["mcp", "remove", "--scope", "user", &name])
                    .status()?;
                if !status.success() {
                    return Err(
                        Error::local("Could not replace the existing CLI server entry.").into(),
                    );
                }
            }
            let mut command = Command::new(cmd);
            command.args(["mcp", "add"]);
            if matches!(client, ClientArg::Claude) {
                command.args(["--scope", "user", "--transport", "stdio"]);
            }
            let status = command
                .arg(&name)
                .arg("--")
                .arg(executable)
                .args(args)
                .status()?;
            if !status.success() {
                return Err(Error::local(
                    "CLI registration failed. Run moocp config for a manual configuration.",
                )
                .into());
            }
        }
        ClientArg::Gemini => {
            let user = directories::BaseDirs::new()
                .ok_or_else(|| Error::local("Cannot locate your home directory."))?;
            let path = user.home_dir().join(".gemini/settings.json");
            let mut settings: Value = if path.exists() {
                serde_json::from_slice(&std::fs::read(&path)?)?
            } else {
                json!({})
            };
            if !settings.is_object() {
                return Err(Error::local(
                    "Gemini settings are not a JSON object; no changes were made.",
                )
                .into());
            }
            if settings.get("mcpServers").is_none() {
                settings["mcpServers"] = json!({});
            }
            let servers = settings["mcpServers"].as_object_mut().ok_or_else(|| {
                Error::local("Gemini mcpServers is not an object; no changes were made.")
            })?;
            if servers.contains_key(&name) && !replace {
                return Err(Error::new(
                    "existing_connection",
                    "Gemini already has this server; use --replace to replace only its entry.",
                )
                .into());
            }
            servers.insert(name.clone(), json!({"command":executable,"args":args}));
            crate::config::private_write(&path, &serde_json::to_vec_pretty(&settings)?)?;
        }
    }
    println!("Connected {name}. Restart your CLI session to load its Moodle tools.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_names_are_safe_for_cli_config() {
        assert!(validate_name("university-2026").is_ok());
        assert!(validate_name("../escape").is_err());
    }
}
