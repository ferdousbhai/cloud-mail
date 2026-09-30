use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::error::{Error, ErrorKind, Result};

pub const DEFAULT_POLL_SECONDS: u32 = 60;

#[derive(Debug, Clone)]
pub struct Config {
    pub api_url: String,
    pub api_token: String,
    pub poll_seconds: u32,
}

/// The on-disk config file, all keys optional.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_seconds: Option<u32>,
}

fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config"))
}

/// `~/.config/cloudmail/config.toml`
pub fn path() -> PathBuf {
    config_dir().join("cloudmail").join("config.toml")
}

/// Pre-rename location, still read (and migrated) for compatibility.
pub fn legacy_path() -> PathBuf {
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

/// Writes a file readable only by the current user (it holds the API token).
pub fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
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

fn env(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
}

/// Loads config from env (CLOUDMAIL_API_URL / CLOUDMAIL_API_TOKEN, legacy CLOUD_MAIL_*) over the file.
pub fn load() -> Result<Config> {
    migrate_legacy();
    let file = match read_file(&path())? {
        Some(f) => f,
        None => read_file(&legacy_path())?.unwrap_or_default(),
    };
    let api_url = env(&["CLOUDMAIL_API_URL", "CLOUD_MAIL_API_URL"]).or(file.api_url).filter(|v| !v.trim().is_empty());
    let api_token = env(&["CLOUDMAIL_API_TOKEN", "CLOUD_MAIL_API_TOKEN"]).or(file.api_token).filter(|v| !v.trim().is_empty());

    match (api_url, api_token) {
        (Some(api_url), Some(api_token)) => Ok(Config {
            api_url,
            api_token,
            poll_seconds: file.poll_seconds.unwrap_or(DEFAULT_POLL_SECONDS).max(10),
        }),
        (url, _) => Err(Error::new(
            ErrorKind::Config,
            format!(
                "cloudmail isn't configured: {} is missing from {} (or CLOUDMAIL_API_URL / CLOUDMAIL_API_TOKEN)",
                if url.is_none() { "api_url" } else { "api_token" },
                path().display()
            ),
        )),
    }
}
