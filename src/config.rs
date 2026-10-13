//! Account profiles and OS-backed credentials. Profile IDs also scope private local data.

use std::{
    fs,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{Error, Result};

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStorage {
    #[default]
    Keyring,
    File,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub url: Url,
    pub user_id: Option<u64>,
    pub display_name: Option<String>,
    pub storage: CredentialStorage,
    pub allow_http: bool,
    pub authenticated_at: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub active_profile: Option<String>,
    pub profiles: Vec<Profile>,
}

#[derive(Clone)]
pub struct Home {
    pub path: PathBuf,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub expires: Option<f64>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Credentials {
    pub cookies: Vec<Cookie>,
    pub token: Option<String>,
    pub sesskey: Option<String>,
}

impl Home {
    pub fn discover(path: Option<PathBuf>) -> Result<Self> {
        let path = path
            .or_else(|| std::env::var_os("MOOCP_DATA_DIR").map(PathBuf::from))
            .or_else(|| {
                ProjectDirs::from("org", "moocp", "moocp").map(|d| d.data_local_dir().to_owned())
            })
            .ok_or_else(|| {
                Error::local("Could not determine your data directory. Set MOOCP_DATA_DIR.")
            })?;
        Ok(Self { path })
    }
    pub fn ensure(&self) -> Result<()> {
        private_dir(&self.path)
    }
    pub fn config(&self) -> Result<Config> {
        let path = self.path.join("config.json");
        if !path.exists() {
            return Ok(Config::default());
        }
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }
    pub fn save_config(&self, config: &Config) -> Result<()> {
        self.ensure()?;
        private_write(
            &self.path.join("config.json"),
            &serde_json::to_vec_pretty(config)?,
        )
    }
    pub fn profile(&self, name: Option<&str>) -> Result<Profile> {
        let config = self.config()?;
        let wanted = name.or(config.active_profile.as_deref());
        config
            .profiles
            .into_iter()
            .find(|p| wanted == Some(p.id.as_str()) || wanted == Some(p.name.as_str()))
            .ok_or_else(|| {
                Error::new(
                    "not_configured",
                    "Connect Moodle first: moocp setup https://your-moodle.example",
                )
            })
    }
    pub fn save_profile(&self, profile: &Profile) -> Result<()> {
        let mut config = self.config()?;
        config.profiles.retain(|p| p.id != profile.id);
        config.profiles.push(profile.clone());
        config.active_profile = Some(profile.id.clone());
        self.save_config(&config)
    }
    pub fn credential_path(&self, profile: &Profile) -> PathBuf {
        // Profile IDs are generated UUIDs. Validate old/local config before using as a path.
        self.path
            .join("credentials")
            .join(format!("{}.json", profile.id))
    }
    pub fn credentials(&self, profile: &Profile) -> Result<Credentials> {
        validate_profile_id(&profile.id)?;
        let encoded = match profile.storage {
            CredentialStorage::File => {
                fs::read_to_string(self.credential_path(profile)).map_err(|_| Error::auth())?
            }
            CredentialStorage::Keyring => credential_entry(profile)?
                .get_password()
                .map_err(|_| Error::auth())?,
        };
        serde_json::from_str(&encoded).map_err(|_| Error::auth())
    }
    pub fn save_credentials(&self, profile: &Profile, credentials: &Credentials) -> Result<()> {
        validate_profile_id(&profile.id)?;
        let encoded = serde_json::to_string(credentials)?;
        match profile.storage {
            CredentialStorage::File => {
                private_dir(&self.path.join("credentials"))?;
                private_write(&self.credential_path(profile), encoded.as_bytes())
            }
            CredentialStorage::Keyring => credential_entry(profile)?.set_password(&encoded)
                .map_err(|_| Error::new("credential_store_unavailable", "Your OS credential store is unavailable. Unlock it, or explicitly choose --credential-store file for private local file storage.")),
        }
    }
    pub fn remove_credentials(&self, profile: &Profile) -> Result<()> {
        validate_profile_id(&profile.id)?;
        match profile.storage {
            CredentialStorage::File => {
                let path = self.credential_path(profile);
                if path.exists() {
                    fs::remove_file(path)?;
                }
            }
            CredentialStorage::Keyring => match credential_entry(profile)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => (),
                Err(_) => {
                    return Err(Error::local(
                        "Could not remove the credential from your OS store.",
                    ));
                }
            },
        }
        Ok(())
    }
    pub fn clear_staged_files(&self, profile: &Profile) -> Result<()> {
        validate_profile_id(&profile.id)?;
        let path = self.path.join("proposal-files").join(&profile.id);
        if path.exists() {
            fs::remove_dir_all(path)?;
        }
        Ok(())
    }
}

fn credential_entry(profile: &Profile) -> Result<keyring::Entry> {
    keyring::Entry::new("moocp", &profile.id)
        .map_err(|_| Error::local("Could not initialise OS credential storage."))
}

pub fn validate_profile_id(id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id).map_err(|_| Error::invalid("Invalid profile identifier."))?;
    Ok(())
}

pub fn mark_browser_directory(path: &Path, profile: &Profile) -> Result<()> {
    private_write(
        &path.join("moocp-profile.json"),
        &serde_json::to_vec(&serde_json::json!({"profile":profile.id,"name":profile.name}))?,
    )
}

pub fn site_url(value: &str, allow_http: bool) -> Result<Url> {
    let mut url = Url::parse(value).map_err(|_| {
        Error::invalid("Provide a full Moodle URL, for example https://moodle.example.edu.")
    })?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::invalid(
            "Use the Moodle site root without credentials, query parameters, or fragments.",
        ));
    }
    if url.scheme() != "https" && !(allow_http && url.scheme() == "http") {
        return Err(Error::invalid(
            "Moodle must use HTTPS. Development sites can explicitly use --allow-http.",
        ));
    }
    let path = url.path().trim_end_matches('/');
    let path = path
        .strip_suffix("/login/index.php")
        .or_else(|| path.strip_suffix("/my"))
        .unwrap_or(path)
        .to_string();
    url.set_path(&format!("{path}/"));
    Ok(url)
}

pub fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::local("File has no parent directory."))?;
    private_dir(parent)?;
    let temporary = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonicalises_subpath_without_allowing_credential_urls() {
        assert_eq!(
            site_url("https://example.edu/moodle/my/", false)
                .unwrap()
                .as_str(),
            "https://example.edu/moodle/"
        );
        assert!(site_url("https://secret@example.edu/", false).is_err());
        assert!(site_url("http://example.edu/", false).is_err());
        assert!(site_url("https://example.edu/?token=secret", false).is_err());
    }
    #[test]
    fn profile_paths_cannot_traverse() {
        assert!(validate_profile_id("../../elsewhere").is_err());
    }
}
