//! Linking, signing in and unlinking accounts, for the CLI and the app alike.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{self, AccountConfig};
use crate::error::{Error, ErrorKind};
use crate::gmail::{self, Gmail};
use crate::hey::Hey;
use crate::icloud::Icloud;
use crate::provider::{self, AccountStatus, KNOWN_PROVIDERS, Provider};
use crate::session;
use crate::unified::Mail;

pub const HEY_INSTALL: &str = "install the hey CLI (https://github.com/basecamp/hey-cli, e.g. `mise use -g github:basecamp/hey-cli`), or pass --command <path>";
pub const ICLOUD_SESSION_INSTALL: &str = "install icloud-for-omarchy, whose icloud-session keeps the iCloud sign-in (https://github.com/ferdousbhai/icloud-for-omarchy)";
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(600);

/// Why linking or signing in stopped.
#[derive(Debug)]
pub enum LinkError {
    Usage { message: String, hint: Option<String> },
    NotInstalled { message: String, hint: String },
    NotSignedIn { message: String, hint: String },
    NotConfigured(String),
    Failed(Error),
}

impl From<Error> for LinkError {
    fn from(e: Error) -> Self {
        LinkError::Failed(e)
    }
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::Usage { message, .. }
            | LinkError::NotInstalled { message, .. }
            | LinkError::NotSignedIn { message, .. }
            | LinkError::NotConfigured(message) => f.write_str(message),
            LinkError::Failed(e) => f.write_str(&e.message),
        }
    }
}

fn usage(message: impl Into<String>, hint: Option<&str>) -> LinkError {
    LinkError::Usage { message: message.into(), hint: hint.map(str::to_string) }
}

/// What to link.
#[derive(Debug, Clone, Default)]
pub struct Link {
    pub provider: String,
    pub name: Option<String>,
    /// HEY, Gmail: the CLI, when it isn't on PATH.
    pub command: Option<String>,
    /// HEY: one of its linked accounts.
    pub account: Option<String>,
    /// Gmail: an OAuth client of your own.
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// Whether a sign-in (browser, icloud-session's window) may start when needed.
    pub can_sign_in: bool,
}

#[derive(Debug, Clone)]
pub struct Linked {
    pub name: String,
    pub provider: String,
    pub label: &'static str,
    pub addresses: Vec<String>,
    /// The CLI's version (HEY, Gmail).
    pub version: Option<String>,
    pub gws_dir: Option<PathBuf>,
    pub replaced: bool,
    pub config_path: PathBuf,
    /// For an account your worker screens: how many of its correspondents were screened in, or
    /// why they couldn't be.
    pub screened_in: Option<std::result::Result<i64, String>>,
}

/// A linked account's config, with its keyring secret filled in.
pub fn account_config(name: &str) -> std::result::Result<Option<AccountConfig>, Error> {
    let file = config::read_file(&config::path())?.unwrap_or_default();
    let Some(mut cfg) = file.accounts.get(name).cloned() else { return Ok(None) };
    if cfg.client_id.is_some() {
        cfg.client_secret = crate::keyring::get(&config::client_secret_name(name))?;
    }
    Ok(Some(cfg))
}

/// Opens a linked account by name.
pub fn open(name: &str) -> std::result::Result<Arc<dyn Provider>, Error> {
    let cfg =
        account_config(name)?.ok_or_else(|| Error::new(ErrorKind::NotFound, format!("no linked account {name}")))?;
    provider::open(name, &cfg)
}

/// Links an account: checks its tool, signs in when needed and allowed, saves it, and (with your
/// worker at hand) screens in the people it corresponds with. `notice` tells what's happening.
pub fn link(req: &Link, mail: Option<&Mail>, notice: &dyn Fn(&str)) -> std::result::Result<Linked, LinkError> {
    let provider_name = req.provider.to_ascii_lowercase();
    if !KNOWN_PROVIDERS.iter().any(|(p, _)| *p == provider_name) {
        let known = KNOWN_PROVIDERS.iter().map(|(p, d)| format!("{p} ({d})")).collect::<Vec<_>>().join(", ");
        return Err(usage(format!("unknown provider \"{provider_name}\""), Some(&format!("known: {known}"))));
    }
    let name = req.name.clone().unwrap_or_else(|| provider_name.clone());
    if !provider::valid_name(&name) {
        return Err(usage(
            format!("\"{name}\" can't be an account name"),
            Some("use lowercase letters, digits and dashes (it prefixes the account's IDs)"),
        ));
    }
    let (hey, gmail, icloud) = (provider_name == "hey", provider_name == "gmail", provider_name == "icloud");
    if !hey && req.account.is_some() {
        return Err(usage(
            format!(
                "--account picks one of HEY's linked accounts; for another {} account, add it under another --name",
                provider::account_label(&provider_name)
            ),
            None,
        ));
    }
    if !gmail && req.client_id.is_some() {
        return Err(usage("--client-id and --client-secret are for Gmail", None));
    }
    if icloud && req.command.is_some() {
        return Err(usage("--command is for HEY and Gmail; iCloud Mail goes through icloud-session", None));
    }
    let mut cfg = AccountConfig {
        provider: (name != provider_name).then(|| provider_name.clone()),
        command: req.command.clone(),
        account: req.account.clone(),
        client_id: req.client_id.clone(),
        client_secret: req.client_secret.clone(),
    };
    let (label, version, addresses, gws_dir): (&'static str, Option<String>, Vec<String>, Option<PathBuf>) = if gmail {
        let (version, addresses, dir) = link_gmail(&name, &cfg, req.can_sign_in, notice)?;
        ("Gmail", Some(version), addresses, Some(dir))
    } else if icloud {
        ("iCloud Mail", None, link_icloud(&name, &cfg, req.can_sign_in, notice)?, None)
    } else {
        let (version, addresses) = link_hey(&name, &cfg, &provider_name, req.can_sign_in, notice)?;
        ("HEY", Some(version), addresses, None)
    };

    if let Some(secret) = cfg.client_secret.take().filter(|s| !s.trim().is_empty()) {
        crate::keyring::set(
            &config::client_secret_name(&name),
            &format!("Cloudmail {name} OAuth client secret"),
            secret.trim(),
        )?;
    }
    let path = config::path();
    let mut file = config::read_file(&path)?.unwrap_or_default();
    let replaced = file.accounts.insert(name.clone(), cfg.clone()).is_some();
    let config_path = config::save(&file)?;

    let screened_in = match mail {
        Some(mail) => {
            let p = provider::open(&name, &account_config(&name)?.unwrap_or(cfg))?;
            p.screened_by_worker().then(|| mail.screen_in_correspondents(p.as_ref()).map_err(|e| e.message))
        }
        None => None,
    };
    Ok(Linked { name, provider: provider_name, label, addresses, version, gws_dir, replaced, config_path, screened_in })
}

fn link_hey(
    name: &str,
    cfg: &AccountConfig,
    provider_name: &str,
    can_sign_in: bool,
    notice: &dyn Fn(&str),
) -> std::result::Result<(String, Vec<String>), LinkError> {
    let hey = Hey::new(name, cfg);
    let version =
        hey.version().map_err(|e| LinkError::NotInstalled { message: e.message, hint: HEY_INSTALL.into() })?;
    if !hey.signed_in()? {
        if !can_sign_in {
            return Err(LinkError::NotSignedIn {
                message: "HEY isn't signed in on this computer".into(),
                hint: format!(
                    "run `{} auth login` (one browser sign-in), then `cloudmail account add {provider_name}` again",
                    hey.command()
                ),
            });
        }
        notice(&format!("Signing in to HEY in your browser (`{} auth login`)…", hey.command()));
        hey.login()?;
        if !hey.signed_in()? {
            return Err(LinkError::NotSignedIn {
                message: "HEY still isn't signed in".into(),
                hint: format!("run `{} auth login` and try again", hey.command()),
            });
        }
    }
    Ok((version, hey.identities().unwrap_or_default().into_iter().map(|a| a.email).collect()))
}

/// Checks gws, signs in when needed (or when the saved sign-in no longer works) and reads the
/// account's addresses.
fn link_gmail(
    name: &str,
    cfg: &AccountConfig,
    can_sign_in: bool,
    notice: &dyn Fn(&str),
) -> std::result::Result<(String, Vec<String>, PathBuf), LinkError> {
    let mut g = Gmail::new(name, cfg);
    // No gws and none named (--command, CLOUDMAIL_GWS_COMMAND): install it, for this user only.
    if cfg.command.is_none()
        && std::env::var(gmail::COMMAND_ENV).map_or(true, |c| c.trim().is_empty())
        && g.is_missing()
    {
        install_gws(notice)?;
        g = Gmail::new(name, cfg);
    }
    let version =
        g.version().map_err(|e| LinkError::NotInstalled { message: e.message, hint: gmail::INSTALL_HINT.into() })?;
    let sign_in = |why: &str| -> std::result::Result<(), LinkError> {
        if !g.client_configured() {
            return Err(LinkError::NotConfigured(Gmail::no_client_error().message));
        }
        if !can_sign_in {
            return Err(LinkError::NotSignedIn {
                message: format!("Gmail {why}"),
                hint: format!("run `cloudmail account add {name}` at a terminal: one Google sign-in in your browser"),
            });
        }
        notice("Signing in to Google in your browser, for Gmail only (read, label, archive and send).");
        notice(
            "While Cloudmail's Google app is unverified, Google says so: choose Advanced, then \"Go to Cloudmail\".",
        );
        g.login()?;
        Ok(())
    };
    if !g.signed_in() {
        sign_in("isn't signed in on this computer")?;
    }
    let addresses = match g.identities() {
        Ok(a) => a,
        Err(e) if e.kind == ErrorKind::AccountAuth => {
            sign_in("needs signing in again (the saved sign-in expired or was revoked)")?;
            g.identities()?
        }
        Err(e) => return Err(e.into()),
    };
    Ok((version, addresses.into_iter().map(|a| a.email).collect(), g.dir().to_path_buf()))
}

/// `npm install` of Google's Workspace CLI into [`gmail::bundled_prefix`], which `Gmail` then runs.
fn install_gws(notice: &dyn Fn(&str)) -> std::result::Result<(), LinkError> {
    let prefix = gmail::bundled_prefix();
    notice(&format!("Installing Google's Workspace CLI (gws), which Gmail needs, into {}", prefix.display()));
    let mut npm = std::process::Command::new("npm");
    npm.args(["install", "--global", "--no-audit", "--no-fund", "--prefix"]).arg(&prefix).arg(gmail::PACKAGE);
    let failed = |message: String| LinkError::NotInstalled { message, hint: gmail::INSTALL_HINT.into() };
    match provider::run_command(&mut npm, None, Duration::from_secs(600)) {
        provider::Run::Done { status, .. } if status.success() && gmail::bundled_command().is_file() => Ok(()),
        provider::Run::Done { status, stderr, .. } => {
            let tail = stderr.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>();
            Err(failed(format!("`npm install {}` failed ({status}): {}", gmail::PACKAGE, tail.join(" "))))
        }
        provider::Run::Missing => Err(LinkError::NotInstalled {
            message: "Gmail needs Google's Workspace CLI, and installing it needs npm, which isn't installed".into(),
            hint: "install npm (Arch: `sudo pacman -S npm`), then run this again".into(),
        }),
        provider::Run::TimedOut => Err(failed(format!("`npm install {}` took over 10 minutes", gmail::PACKAGE))),
        provider::Run::Failed(e) => Err(failed(format!("could not run npm: {e}"))),
    }
}

/// Checks icloud-session is there and signed in (opening its sign-in window when allowed), then
/// reads the account's addresses from Mail.
fn link_icloud(
    name: &str,
    cfg: &AccountConfig,
    can_sign_in: bool,
    notice: &dyn Fn(&str),
) -> std::result::Result<Vec<String>, LinkError> {
    let ic = Icloud::new(name, cfg);
    let status = ic
        .session()
        .status()
        .map_err(|e| LinkError::NotInstalled { message: e.message, hint: ICLOUD_SESSION_INSTALL.into() })?;
    if !status.signed_in {
        if !can_sign_in {
            return Err(LinkError::NotSignedIn {
                message: "iCloud isn't signed in on this computer".into(),
                hint: format!(
                    "run `cloudmail account add {name}` at a terminal (it opens icloud-session's sign-in window), or `icloud-session sign-in`"
                ),
            });
        }
        notice("Opening icloud-session's sign-in window: sign in to iCloud there.");
        ic.session().sign_in()?;
        if !session::wait_for_sign_in(ic.session(), SIGN_IN_TIMEOUT)? {
            return Err(LinkError::NotSignedIn {
                message: "iCloud still isn't signed in".into(),
                hint: "finish the sign-in in icloud-session's window, then try again".into(),
            });
        }
    }
    Ok(ic.identities()?.into_iter().map(|a| a.email).collect())
}

/// Signs a linked account in again (its own browser sign-in, or icloud-session's window) and
/// checks it answers.
pub fn sign_in(name: &str) -> std::result::Result<AccountStatus, LinkError> {
    let p = open(name)?;
    p.sign_in()?;
    let status = p.status();
    if !status.ok {
        return Err(LinkError::NotSignedIn {
            message: format!("{} still isn't signed in: {}", p.label(), status.detail),
            hint: format!("try `cloudmail account login {name}` again"),
        });
    }
    Ok(status)
}

/// What unlinking took with it.
#[derive(Debug, Clone)]
pub struct Unlinked {
    pub provider: String,
    /// Gmail: whether cloudmail's own sign-in was removed from this computer.
    pub signed_out: Option<bool>,
    pub gws_dir: Option<PathBuf>,
}

/// Unlinks an account; nothing changes in the account itself. Gmail's sign-in is cloudmail's own,
/// so it goes; HEY's CLI and icloud-session, which other apps use, stay signed in.
pub fn unlink(name: &str) -> std::result::Result<Unlinked, Error> {
    let path = config::path();
    let mut file = config::read_file(&path)?.unwrap_or_default();
    let cfg = file
        .accounts
        .remove(name)
        .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("no linked account {name}")))?;
    config::save(&file)?;
    if cfg.client_id.is_some() {
        crate::keyring::delete(&config::client_secret_name(name))?;
    }
    let provider = cfg.provider(name).to_string();
    if provider == "gmail" {
        let g = Gmail::new(name, &cfg);
        let removed = g.forget().map_err(|e| {
            Error::new(ErrorKind::Config, format!("unlinked {name}, but could not remove {}: {e}", g.dir().display()))
        })?;
        return Ok(Unlinked { provider, signed_out: Some(removed), gws_dir: Some(g.dir().to_path_buf()) });
    }
    Ok(Unlinked { provider, signed_out: None, gws_dir: None })
}
