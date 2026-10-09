//! `cloudmail account`: link other mail accounts (HEY, Gmail, iCloud Mail) next to your worker. Opt-in;
//! nothing changes for anyone who never runs `account add`.

use serde_json::json;

use cloudmail_api::config::{self, AccountConfig};
use cloudmail_api::gmail::{self, Gmail};
use cloudmail_api::hey::Hey;
use cloudmail_api::icloud::{self, Icloud};
use cloudmail_api::provider::{self, KNOWN_PROVIDERS, Provider};
use cloudmail_api::text::bare_email;
use cloudmail_api::{AccountStatus, ErrorKind};

use crate::Ctx;
use crate::cli::AccountCommand;
use crate::output::{Breadcrumb, CliError, CliResult, Response, crumb, dim, exit, prompt};

const HEY_INSTALL: &str = "install the hey CLI (https://github.com/basecamp/hey-cli, e.g. `mise use -g github:basecamp/hey-cli`), or pass --command <path>";

/// What `account add` was asked for.
struct AddArgs {
    command: Option<String>,
    account: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    email: Option<String>,
    aliases: Vec<String>,
    password_stdin: bool,
    no_login: bool,
    login: bool,
}

pub fn account(ctx: &Ctx, cmd: AccountCommand) -> CliResult {
    match cmd {
        AccountCommand::List => list(),
        AccountCommand::Add { provider, name, command, account, client_id, client_secret, email, aliases, password_stdin, no_login, login } => {
            add(ctx, &provider, name, AddArgs { command, account, client_id, client_secret, email, aliases, password_stdin, no_login, login })
        }
        AccountCommand::Login { name, password_stdin } => login(ctx, &name, password_stdin),
        AccountCommand::Remove { name } => remove(&name),
    }
}

/// Signs an already linked account in again in the browser (iCloud: with a new app-specific
/// password), then checks it answers.
fn login(ctx: &Ctx, name: &str, password_stdin: bool) -> CliResult {
    let file = config::read_file(&config::path())?.unwrap_or_default();
    let Some(cfg) = file.accounts.get(name) else {
        return Err(CliError::not_found(format!("no linked account {name}")).hint("see `cloudmail account list`, or link one with `cloudmail account add`"));
    };
    if cfg.provider(name) == "icloud" {
        let addresses = sign_in_icloud(ctx, name, cfg, password_stdin, &format!("cloudmail account login {name}"))?;
        let summary = format!("Signed in to iCloud Mail again ({})", addresses.join(", "));
        return Ok(Response::new(json!({ "account": name, "addresses": addresses }), summary).crumbs(vec![crumb("inbox", "cloudmail inbox", "Your Inbox")]));
    }
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
        human.push_str(&format!("\n\n{}", dim("No linked accounts. `cloudmail account add gmail`, `… add icloud` or `… add hey` shows that mail here too.")));
    }
    let summary = match statuses.len() {
        0 => "No linked accounts".to_string(),
        n => format!("{n} linked account{}; {} signed in", if n == 1 { "" } else { "s" }, statuses.iter().filter(|s| s.ok).count()),
    };
    let ids = statuses.iter().map(|s| s.name.clone()).collect();
    let mut crumbs = vec![
        crumb("add-gmail", "cloudmail account add gmail", "Link your Gmail account"),
        crumb("add-icloud", "cloudmail account add icloud --email <you@icloud.com>", "Link your iCloud Mail account"),
        crumb("add-hey", "cloudmail account add hey", "Link your HEY account"),
    ];
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
    let icloud = provider_name == "icloud";
    if provider_name != "hey" && a.account.is_some() {
        return Err(CliError::usage(format!("--account picks one of HEY's linked accounts; for another {} account, add it under another --name", provider::account_label(&provider_name))));
    }
    if !gmail && a.client_id.is_some() {
        return Err(CliError::usage("--client-id and --client-secret are for Gmail"));
    }
    if icloud && a.command.is_some() {
        return Err(CliError::usage("--command is for HEY and Gmail; iCloud Mail needs no other program"));
    }
    if !icloud && (a.email.is_some() || !a.aliases.is_empty() || a.password_stdin) {
        return Err(CliError::usage("--email, --alias and --password-stdin are for iCloud Mail"));
    }
    let mut cfg = AccountConfig {
        provider: (name != provider_name).then(|| provider_name.clone()),
        command: a.command.clone(),
        account: a.account.clone(),
        client_id: a.client_id.clone(),
        client_secret: a.client_secret.clone(),
        ..Default::default()
    };
    let can_login = !a.no_login && (a.login || ctx.interactive());
    let (label, version, addresses, extra) = if gmail {
        link_gmail(&name, &cfg, can_login)?
    } else if icloud {
        link_icloud(ctx, &name, &mut cfg, &a)?
    } else {
        link_hey(&cfg, &name, &provider_name, can_login)?
    };

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
    if gmail || icloud {
        summary.push_str(&format!(". {label} has no Screener, so its mail goes straight to your Inbox"));
    }
    if let Some(f) = &forwarding {
        summary.push_str(&format!(". Your worker forwards to {f}, so {label}'s copies of that mail are hidden"));
    }
    let mut data = json!({ "account": name, "provider": provider_name, "addresses": addresses, "config_path": written, "forwarding_to_this_account": forwarding });
    for (k, v) in extra {
        data[k] = v;
    }
    if !icloud {
        data[if gmail { "gws_version" } else { "hey_version" }] = json!(version);
    }
    let mut crumbs: Vec<Breadcrumb> = vec![crumb("inbox", "cloudmail inbox", &format!("Your Inbox with {label}'s merged in"))];
    if gmail {
        crumbs.push(crumb("search", "cloudmail search <words>", "Search your mail and Gmail together (Gmail reads its own search syntax)"));
    } else if icloud {
        crumbs.push(crumb("search", "cloudmail search <words>", "Search your mail and iCloud Mail together"));
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

/// Checks the iCloud address, gets an app-specific password (stdin, or asked at the terminal after
/// opening the page that makes one), signs in with it and keeps it.
fn link_icloud(ctx: &Ctx, name: &str, cfg: &mut AccountConfig, a: &AddArgs) -> CliResult<Linked> {
    let email = match a.email.as_deref().map(bare_email).filter(|e| !e.is_empty()) {
        Some(e) => e,
        None if ctx.interactive() && !a.password_stdin => bare_email(&prompt("Your iCloud Mail address (…@icloud.com, …@me.com or …@mac.com):")?),
        None => return Err(CliError::usage("which iCloud Mail address? Pass --email you@icloud.com")),
    };
    if !icloud::is_icloud_address(&email) {
        return Err(CliError::usage(format!("{email} isn't an iCloud Mail address"))
            .hint("sign in with your …@icloud.com (or @me.com, @mac.com) address, and add the addresses you also send from (Hide My Email, a custom domain) with --alias"));
    }
    cfg.email = Some(email);
    cfg.aliases = a.aliases.iter().map(|x| bare_email(x)).filter(|x| x.contains('@')).collect();
    let addresses = sign_in_icloud(ctx, name, cfg, a.password_stdin, &format!("cloudmail account add icloud --email {} --password-stdin", cfg.email.as_deref().unwrap_or_default()))?;
    Ok(("iCloud Mail", String::new(), addresses, vec![("password_file", json!(icloud::password_path(name)))]))
}

/// Reads an app-specific password, checks it signs in, and keeps it; the account's addresses.
fn sign_in_icloud(ctx: &Ctx, name: &str, cfg: &AccountConfig, password_stdin: bool, retry: &str) -> CliResult<Vec<String>> {
    let password = if password_stdin {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        line.trim().to_string()
    } else if ctx.interactive() {
        eprintln!("iCloud Mail signs in with an app-specific password, not your Apple Account password.");
        eprintln!("Opening {} : sign in, then Sign-In and Security → App-Specific Passwords → +, and name it Cloudmail.", icloud::PASSWORD_URL);
        icloud::open_password_page();
        rpassword::prompt_password("App-specific password (xxxx-xxxx-xxxx-xxxx): ")?.trim().to_string()
    } else {
        return Err(CliError::new("not_logged_in", exit::AUTH, "iCloud Mail needs an app-specific password")
            .hint(format!("make one at {} (Sign-In and Security → App-Specific Passwords), then pipe it to `{retry}`, or run this at a terminal", icloud::PASSWORD_URL)));
    };
    if password.is_empty() {
        return Err(CliError::usage("no app-specific password was given"));
    }
    let addresses = Icloud::new(name, cfg).with_password(&password).verify().map_err(|e| {
        let refused = e.kind == ErrorKind::AccountAuth;
        let err: CliError = e.into();
        if refused { err.hint(format!("check the address, and make a new app-specific password at {} if this one was revoked", icloud::PASSWORD_URL)) } else { err }
    })?;
    icloud::save_password(name, &password).map_err(|e| CliError::generic(format!("signed in, but could not save the password in {}: {e}", icloud::password_path(name).display())))?;
    Ok(addresses)
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
    if cfg.provider(name) == "icloud" {
        let removed = icloud::forget_password(name).map_err(|e| CliError::generic(format!("unlinked {name}, but could not remove {}: {e}", icloud::password_path(name).display())))?;
        let summary = format!(
            "Unlinked {name}{}; nothing changed in iCloud itself. To withdraw the password too, revoke it at {}",
            if removed { " and removed its saved app-specific password" } else { "" },
            icloud::PASSWORD_URL
        );
        return Ok(Response::new(json!({ "account": name, "removed": true, "signed_out": removed }), summary));
    }
    Ok(Response::new(json!({ "account": name, "removed": true }), format!("Unlinked {name}; nothing changed in the account itself, and its CLI is still signed in")))
}
