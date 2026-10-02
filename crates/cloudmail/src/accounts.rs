//! `cloudmail account`: link other mail accounts (HEY, Gmail) next to your worker. Opt-in;
//! nothing changes for anyone who never runs `account add`.

use serde_json::json;

use cloudmail_api::config::{self, AccountConfig};
use cloudmail_api::gmail::{self, Gmail};
use cloudmail_api::hey::Hey;
use cloudmail_api::provider::{self, KNOWN_PROVIDERS, Provider};
use cloudmail_api::{AccountStatus, ErrorKind};

use crate::Ctx;
use crate::cli::AccountCommand;
use crate::output::{Breadcrumb, CliError, CliResult, Response, crumb, dim, exit};

const HEY_INSTALL: &str = "install the hey CLI (https://github.com/basecamp/hey-cli, e.g. `mise use -g github:basecamp/hey-cli`), or pass --command <path>";

/// What `account add` was asked for.
struct AddArgs {
    command: Option<String>,
    account: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    no_login: bool,
    login: bool,
}

pub fn account(ctx: &Ctx, cmd: AccountCommand) -> CliResult {
    match cmd {
        AccountCommand::List => list(),
        AccountCommand::Add { provider, name, command, account, client_id, client_secret, no_login, login } => {
            add(ctx, &provider, name, AddArgs { command, account, client_id, client_secret, no_login, login })
        }
        AccountCommand::Login { name } => login(&name),
        AccountCommand::Remove { name } => remove(&name),
    }
}

/// Signs an already linked account in again in the browser, then checks it answers.
fn login(name: &str) -> CliResult {
    let file = config::read_file(&config::path())?.unwrap_or_default();
    let Some(cfg) = file.accounts.get(name) else {
        return Err(CliError::not_found(format!("no linked account {name}")).hint("see `cloudmail account list`, or link one with `cloudmail account add`"));
    };
    let p = provider::open(name, cfg)?;
    eprintln!("Signing in to {} in your browser…", p.label());
    p.sign_in()?;
    let status = p.status();
    if !status.ok {
        return Err(CliError::new("not_logged_in", exit::AUTH, format!("{} still isn't signed in: {}", p.label(), status.detail)));
    }
    let summary = format!("Signed in to {} again{}", p.label(), if status.addresses.is_empty() { String::new() } else { format!(" ({})", status.addresses.join(", ")) });
    Ok(Response::new(json!({ "account": name, "addresses": status.addresses }), summary).crumbs(vec![crumb("inbox", "cloudmail inbox", "Your Inbox")]))
}

fn list() -> CliResult {
    let path = config::path();
    let file = config::read_file(&path)?.unwrap_or_default();
    let worker = config::load().ok().map(|c| c.api_url);
    let statuses: Vec<AccountStatus> = std::thread::scope(|s| {
        let handles: Vec<_> = file
            .accounts
            .iter()
            .map(|(name, cfg)| {
                s.spawn(move || match provider::open(name, cfg) {
                    Ok(p) => p.status(),
                    Err(e) => AccountStatus { name: name.clone(), provider: cfg.provider(name).to_string(), label: name.clone(), ok: false, addresses: Vec::new(), detail: e.message },
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    let mut human = format!("cloudmail  {}", worker.as_deref().unwrap_or("(worker not configured: run `cloudmail setup`)"));
    for s in &statuses {
        let state = if s.ok { "signed in" } else { "needs attention" };
        let addrs = if s.addresses.is_empty() { String::new() } else { format!("  {}", s.addresses.join(", ")) };
        human.push_str(&format!("\n{:<10} {} ({state}){addrs}\n           {}", s.name, s.label, dim(&s.detail)));
    }
    if statuses.is_empty() {
        human.push_str(&format!("\n\n{}", dim("No linked accounts. `cloudmail account add gmail` or `cloudmail account add hey` shows that mail here too.")));
    }
    let summary = match statuses.len() {
        0 => "No linked accounts".to_string(),
        n => format!("{n} linked account{}; {} signed in", if n == 1 { "" } else { "s" }, statuses.iter().filter(|s| s.ok).count()),
    };
    let ids = statuses.iter().map(|s| s.name.clone()).collect();
    let mut crumbs = vec![crumb("add-gmail", "cloudmail account add gmail", "Link your Gmail account"), crumb("add-hey", "cloudmail account add hey", "Link your HEY account")];
    if !statuses.is_empty() {
        crumbs = vec![crumb("inbox", "cloudmail inbox", "Your Inbox with the linked accounts' merged in")];
        if statuses.iter().any(|s| s.provider == "hey") {
            crumbs.push(crumb("feed", "cloudmail threads list --folder feed", "HEY's The Feed (also paper-trail, set-aside, reply-later)"));
        }
        crumbs.push(crumb("remove", "cloudmail account remove <name>", "Unlink an account"));
    }
    Ok(Response::new(json!({ "worker": worker, "accounts": statuses }), summary).human(human).ids(ids).crumbs(crumbs))
}

fn add(ctx: &Ctx, provider_name: &str, name: Option<String>, a: AddArgs) -> CliResult {
    let provider_name = provider_name.to_ascii_lowercase();
    if !KNOWN_PROVIDERS.iter().any(|(p, _)| *p == provider_name) {
        let known = KNOWN_PROVIDERS.iter().map(|(p, d)| format!("{p} ({d})")).collect::<Vec<_>>().join(", ");
        return Err(CliError::usage(format!("unknown provider \"{provider_name}\"")).hint(format!("known: {known}")));
    }
    let name = name.unwrap_or_else(|| provider_name.clone());
    if !provider::valid_name(&name) {
        return Err(CliError::usage(format!("\"{name}\" can't be an account name")).hint("use lowercase letters, digits and dashes (it prefixes the account's IDs)"));
    }
    let gmail = provider_name == "gmail";
    if gmail && a.account.is_some() {
        return Err(CliError::usage("--account picks one of HEY's linked accounts; for another Gmail account, add it under another --name"));
    }
    if !gmail && a.client_id.is_some() {
        return Err(CliError::usage("--client-id and --client-secret are for Gmail"));
    }
    let cfg = AccountConfig { provider: (name != provider_name).then(|| provider_name.clone()), command: a.command.clone(), account: a.account.clone(), client_id: a.client_id.clone(), client_secret: a.client_secret.clone() };
    let can_login = !a.no_login && (a.login || ctx.interactive());
    let (label, version, addresses, extra) = if gmail { link_gmail(&name, &cfg, can_login)? } else { link_hey(&cfg, &name, &provider_name, can_login)? };

    let path = config::path();
    let mut file = config::read_file(&path)?.unwrap_or_default();
    let replaced = file.accounts.insert(name.clone(), cfg).is_some();
    let written = config::save(&file)?;

    // Worth saying: forwarding to the address just linked would show everything twice, if not for dedupe.
    let forwarding = ctx
        .client()
        .ok()
        .and_then(|c| c.settings().ok())
        .map(|s| s.forward_to)
        .filter(|f| !f.is_empty() && addresses.iter().any(|a| a.eq_ignore_ascii_case(f)));
    let mut summary = format!(
        "{} {label}{}; its mail now shows next to yours",
        if replaced { "Updated" } else { "Linked" },
        if addresses.is_empty() { String::new() } else { format!(" ({})", addresses.join(", ")) }
    );
    if gmail {
        summary.push_str(". Gmail has no Screener, so its mail goes straight to your Inbox");
    }
    if let Some(f) = &forwarding {
        summary.push_str(&format!(". Your worker forwards to {f}, so {label}'s copies of that mail are hidden"));
    }
    let mut data = json!({ "account": name, "provider": provider_name, "addresses": addresses, "config_path": written, "forwarding_to_this_account": forwarding });
    for (k, v) in extra {
        data[k] = v;
    }
    data[if gmail { "gws_version" } else { "hey_version" }] = json!(version);
    let mut crumbs: Vec<Breadcrumb> = vec![crumb("inbox", "cloudmail inbox", &format!("Your Inbox with {label}'s merged in"))];
    if gmail {
        crumbs.push(crumb("search", "cloudmail search <words>", "Search your mail and Gmail together (Gmail reads its own search syntax)"));
    } else {
        crumbs.push(crumb("screener", "cloudmail screener", "Both Screeners"));
        crumbs.push(crumb("feed", "cloudmail threads list --folder feed", "HEY's The Feed (also paper-trail, set-aside, reply-later)"));
    }
    crumbs.push(crumb("list", "cloudmail account list", "Linked accounts"));
    Ok(Response::new(data, summary).crumbs(crumbs))
}

type Linked = (&'static str, String, Vec<String>, Vec<(&'static str, serde_json::Value)>);

fn link_hey(cfg: &AccountConfig, name: &str, provider_name: &str, can_login: bool) -> CliResult<Linked> {
    let hey = Hey::new(name, cfg);
    let version = hey.version().map_err(|e| CliError::new("not_installed", exit::GENERIC, e.message).hint(HEY_INSTALL))?;
    if !hey.signed_in()? {
        if !can_login {
            return Err(CliError::new("not_logged_in", exit::AUTH, "HEY isn't signed in on this computer")
                .hint(format!("run `{} auth login` (one browser sign-in), then `cloudmail account add {provider_name}` again", hey.command())));
        }
        eprintln!("Signing in to HEY in your browser (`{} auth login`)…", hey.command());
        hey.login()?;
        if !hey.signed_in()? {
            return Err(CliError::new("not_logged_in", exit::AUTH, "HEY still isn't signed in").hint(format!("run `{} auth login` and try again", hey.command())));
        }
    }
    let addresses = hey.identities().unwrap_or_default().into_iter().map(|a| a.email).collect();
    Ok(("HEY", version, addresses, Vec::new()))
}

/// Checks gws, signs in when needed (or when the saved sign-in no longer works) and reads the
/// account's addresses.
fn link_gmail(name: &str, cfg: &AccountConfig, can_login: bool) -> CliResult<Linked> {
    let g = Gmail::new(name, cfg);
    let version = g.version().map_err(|e| CliError::new("not_installed", exit::GENERIC, e.message).hint(gmail::INSTALL_HINT))?;
    let sign_in = |why: &str| -> CliResult<()> {
        if !g.client_configured() {
            return Err(CliError::new("not_configured", exit::AUTH, Gmail::no_client_error().message));
        }
        if !can_login {
            return Err(CliError::new("not_logged_in", exit::AUTH, format!("Gmail {why}")).hint(format!("run `cloudmail account add {name}` at a terminal: one Google sign-in in your browser")));
        }
        eprintln!("Signing in to Google in your browser, for Gmail only (read, label, archive and send).");
        eprintln!("While Cloudmail's Google app is unverified, Google says so: choose Advanced, then \"Go to Cloudmail\".");
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
    let addresses = addresses.into_iter().map(|a| a.email).collect();
    Ok(("Gmail", version, addresses, vec![("gws_dir", json!(g.dir()))]))
}

fn remove(name: &str) -> CliResult {
    let path = config::path();
    let mut file = config::read_file(&path)?.unwrap_or_default();
    let Some(cfg) = file.accounts.remove(name) else {
        return Err(CliError::not_found(format!("no linked account {name}")).hint("see `cloudmail account list`"));
    };
    config::save(&file)?;
    if cfg.provider(name) == "gmail" {
        // The sign-in is cloudmail's own, so it goes with the account.
        let g = Gmail::new(name, &cfg);
        let removed = g.forget().map_err(|e| CliError::generic(format!("unlinked {name}, but could not remove {}: {e}", g.dir().display())))?;
        let summary = if removed {
            format!("Unlinked {name} and signed Cloudmail out of Gmail on this computer (removed {}); nothing changed in Gmail itself. To withdraw Cloudmail's access too, remove it at https://myaccount.google.com/connections", g.dir().display())
        } else {
            format!("Unlinked {name}; nothing changed in Gmail itself")
        };
        return Ok(Response::new(json!({ "account": name, "removed": true, "signed_out": removed }), summary));
    }
    Ok(Response::new(json!({ "account": name, "removed": true }), format!("Unlinked {name}; nothing changed in the account itself, and its CLI is still signed in")))
}
