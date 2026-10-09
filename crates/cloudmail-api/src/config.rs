use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, ErrorKind, Result};

pub const DEFAULT_POLL_SECONDS: u32 = 60;

#[derive(Debug, Clone)]
pub struct Config {
    pub api_url: String,
    pub api_token: String,
    pub poll_seconds: u32,
    /// Linked accounts (HEY, …) by name; the name prefixes their IDs (`hey:…`).
    pub accounts: BTreeMap<String, AccountConfig>,
}

/// A linked mail account, `[accounts.<name>]` in the config file. Opt-in: without one,
/// cloudmail only talks to your worker.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct AccountConfig {
    /// Which provider serves it ("hey"); defaults to the account's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The provider's command-line tool, when it isn't on PATH under its usual name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The provider's own account selector (e.g. a HEY linked-account ID); default: all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Gmail: a Google OAuth client to sign in with instead of the one built into cloudmail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// The secret of `client_id`. Kept in the keyring (`client_secret_name`), never written here;
    /// read from the file only to move an older version's into the keyring.
    #[serde(default, skip_serializing)]
    pub client_secret: Option<String>,
}

/// The keyring name of a linked account's own OAuth client secret.
pub fn client_secret_name(account: &str) -> String {
    format!("client_secret:{account}")
}

impl AccountConfig {
    pub fn provider<'a>(&'a self, name: &'a str) -> &'a str {
        self.provider.as_deref().filter(|p| !p.is_empty()).unwrap_or(name)
    }
}

/// The on-disk config file, all keys optional.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
    /// Kept in the keyring (`keyring::API_TOKEN`), never written here; read from the file only to
    /// move an older version's into the keyring.
    #[serde(default, skip_serializing)]
    pub api_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_seconds: Option<u32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub accounts: BTreeMap<String, AccountConfig>,
}

fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config"))
}

/// `~/.config/cloudmail/config.toml`
pub fn path() -> PathBuf {
    config_dir().join("cloudmail").join("config.toml")
}

/// Pre-rename location, still read (and migrated) for compatibility.
fn legacy_path() -> PathBuf {
    config_dir().join("cloud-mail").join("config.toml")
}

/// Copies the legacy config to the new location once, if only the legacy one exists.
pub fn migrate_legacy() -> Option<PathBuf> {
    let (new, old) = (path(), legacy_path());
    if new.exists() || !old.exists() {
        return None;
    }
    let text = std::fs::read_to_string(&old).ok()?;
    write_private(&new, &text).ok()?;
    Some(new)
}

pub fn read_file(path: &Path) -> Result<Option<FileConfig>> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str::<FileConfig>(&text)
            .map(Some)
            .map_err(|e| Error::new(ErrorKind::Config, format!("could not parse {}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::new(ErrorKind::Config, format!("could not read {}: {e}", path.display()))),
    }
}

pub fn save(file: &FileConfig) -> Result<PathBuf> {
    let p = path();
    let text = toml::to_string(file).map_err(|e| Error::new(ErrorKind::Config, e.to_string()))?;
    write_private(&p, &text).map_err(|e| Error::new(ErrorKind::Config, format!("could not write {}: {e}", p.display())))?;
    Ok(p)
}

/// Writes a file readable only by the current user.
pub(crate) fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(dir)?;
        // An existing directory from an older version may be world-readable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    let mut f = opts.open(path)?;
    f.write_all(contents.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

const API_URL_ENV: [&str; 2] = ["CLOUDMAIL_API_URL", "CLOUD_MAIL_API_URL"];
const API_TOKEN_ENV: [&str; 2] = ["CLOUDMAIL_API_TOKEN", "CLOUD_MAIL_API_TOKEN"];

fn env_value(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn env(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| env_value(k))
}

/// The environment variables that are set and so override the config file.
pub fn env_overrides() -> Vec<&'static str> {
    API_URL_ENV.into_iter().zip(API_TOKEN_ENV).flat_map(|(u, t)| [u, t]).filter(|k| env_value(k).is_some()).collect()
}

/// Moves secrets an older version wrote into config.toml (the API token, a Gmail OAuth client's
/// secret) into the keyring, then rewrites the file without them. Once, on the first load that
/// finds any; fails, leaving the file as it is, when the keyring can't take them.
pub fn migrate_secrets(file: &mut FileConfig) -> Result<bool> {
    let tokens = file.api_token.is_some() || file.accounts.values().any(|a| a.client_secret.is_some());
    if !tokens {
        return Ok(false);
    }
    let moving = |e: Error| Error::new(e.kind, format!("{} still holds secrets that belong in the keyring: {}", path().display(), e.message));
    if let Some(token) = file.api_token.take().filter(|t| !t.trim().is_empty()) {
        crate::keyring::set(crate::keyring::API_TOKEN, "Cloudmail API token", token.trim()).map_err(moving)?;
    }
    for (name, account) in &mut file.accounts {
        if let Some(secret) = account.client_secret.take().filter(|s| !s.trim().is_empty()) {
            crate::keyring::set(&client_secret_name(name), &format!("Cloudmail {name} OAuth client secret"), secret.trim()).map_err(moving)?;
        }
    }
    save(file)?;
    Ok(true)
}

/// The API token kept in the keyring, if any.
pub fn keyring_token() -> Result<Option<String>> {
    crate::keyring::get(crate::keyring::API_TOKEN)
}

/// Loads config: the URL from env (CLOUDMAIL_API_URL, legacy CLOUD_MAIL_*) over the file, the token
/// from env (CLOUDMAIL_API_TOKEN) over the keyring.
pub fn load() -> Result<Config> {
    migrate_legacy();
    let mut file = match read_file(&path())? {
        Some(f) => f,
        None => read_file(&legacy_path())?.unwrap_or_default(),
    };
    migrate_secrets(&mut file)?;
    let api_url = env(&API_URL_ENV).or(file.api_url).filter(|v| !v.trim().is_empty());
    let api_token = match env(&API_TOKEN_ENV) {
        Some(t) => Some(t),
        // Only an install that has a worker has a token to look for.
        None if api_url.is_some() => crate::keyring::get(crate::keyring::API_TOKEN)?,
        None => None,
    }
    .filter(|v| !v.trim().is_empty());
    for (name, account) in &mut file.accounts {
        if account.client_id.is_some() {
            account.client_secret = crate::keyring::get(&client_secret_name(name))?;
        }
    }

    match (api_url, api_token) {
        (Some(api_url), Some(api_token)) => Ok(Config {
            api_url,
            api_token,
            poll_seconds: file.poll_seconds.unwrap_or(DEFAULT_POLL_SECONDS).max(10),
            accounts: file.accounts,
        }),
        (url, _) => Err(Error::new(
            ErrorKind::Config,
            format!(
                "cloudmail isn't configured: {} (or set CLOUDMAIL_API_URL / CLOUDMAIL_API_TOKEN); `cloudmail setup` sets both",
                if url.is_none() { format!("api_url is missing from {}", path().display()) } else { "the API token isn't in the keyring".to_string() }
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_read_but_never_written() {
        let old: FileConfig = toml::from_str("api_url = \"https://x\"\napi_token = \"t\"\n[accounts.gmail]\nclient_id = \"i\"\nclient_secret = \"s\"\n").unwrap();
        assert_eq!((old.api_token.as_deref(), old.accounts["gmail"].client_secret.as_deref()), (Some("t"), Some("s")), "an older file's secrets can be moved");
        let text = toml::to_string(&old).unwrap();
        assert!(!text.contains("api_token") && !text.contains("client_secret"), "{text}");
    }

    #[test]
    fn accounts_are_optional_and_round_trip() {
        let old: FileConfig = toml::from_str("api_url = \"https://x\"\n").unwrap();
        assert!(old.accounts.is_empty());
        assert!(!toml::to_string(&old).unwrap().contains("accounts"), "a config without accounts is written as before");
        let mut with = old.clone();
        with.accounts.insert("hey".into(), AccountConfig { command: Some("/opt/hey".into()), ..Default::default() });
        let text = toml::to_string(&with).unwrap();
        assert!(text.contains("[accounts.hey]"), "{text}");
        let back: FileConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.accounts["hey"].command.as_deref(), Some("/opt/hey"));
        assert_eq!(back.accounts["hey"].provider("hey"), "hey");
    }
}
