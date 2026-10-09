//! `cloudmail account`: link other mail accounts (HEY, Gmail, iCloud Mail) next to your worker. Opt-in;
//! nothing changes for anyone who never runs `account add`.

use serde_json::json;

use cloudmail_api::accounts::{self, Link, LinkError};
use cloudmail_api::config;
use cloudmail_api::{AccountStatus, Error, ErrorKind};

use crate::Ctx;
use crate::cli::AccountCommand;
use crate::output::{Breadcrumb, CliError, CliResult, Response, crumb, dim, exit};

pub fn account(ctx: &Ctx, cmd: AccountCommand) -> CliResult {
    match cmd {
        AccountCommand::List => list(),
        AccountCommand::Add { provider, name, command, account, client_id, client_secret, no_login, login } => {
            let can_sign_in = !no_login && (login || ctx.interactive());
            add(ctx, Link { provider, name, command, account, client_id, client_secret, can_sign_in })
        }
        AccountCommand::Login { name } => sign_in(&name),
        AccountCommand::Remove { name } => remove(&name),
    }
}

fn link_error(e: LinkError) -> CliError {
    match e {
        LinkError::Usage { message, hint } => match hint {
            Some(h) => CliError::usage(message).hint(h),
            None => CliError::usage(message),
        },
        LinkError::NotInstalled { message, hint } => CliError::new("not_installed", exit::GENERIC, message).hint(hint),
        LinkError::NotSignedIn { message, hint } => CliError::new("not_logged_in", exit::AUTH, message).hint(hint),
        LinkError::NotConfigured(message) => CliError::new("not_configured", exit::AUTH, message),
        LinkError::Failed(e) => e.into(),
    }
}

fn not_linked(e: Error) -> CliError {
    match e.kind {
        ErrorKind::NotFound => CliError::not_found(e.message)
            .hint("see `cloudmail account list`, or link one with `cloudmail account add`"),
        _ => e.into(),
    }
}

/// Signs an already linked account in again, then checks it answers.
fn sign_in(name: &str) -> CliResult {
    let p = accounts::open(name).map_err(not_linked)?;
    eprintln!("Signing in to {}…", p.label());
    let status = accounts::sign_in(name).map_err(link_error)?;
    let summary = format!(
        "Signed in to {} again{}",
        status.label,
        if status.addresses.is_empty() { String::new() } else { format!(" ({})", status.addresses.join(", ")) }
    );
    Ok(Response::new(json!({ "account": name, "addresses": status.addresses }), summary).crumbs(vec![crumb(
        "inbox",
        "cloudmail inbox",
        "Your Inbox",
    )]))
}

fn list() -> CliResult {
    let file = config::read_file(&config::path())?.unwrap_or_default();
    let worker = std::env::var("CLOUDMAIL_API_URL").ok().filter(|u| !u.trim().is_empty()).or(file.api_url.clone());
    let statuses: Vec<AccountStatus> = std::thread::scope(|s| {
        let handles: Vec<_> = file
            .accounts
            .iter()
            .map(|(name, cfg)| {
                s.spawn(move || match accounts::open(name) {
                    Ok(p) => p.status(),
                    Err(e) => AccountStatus {
                        name: name.clone(),
                        provider: cfg.provider(name).to_string(),
                        label: name.clone(),
                        ok: false,
                        addresses: Vec::new(),
                        detail: e.message,
                    },
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    let mut human =
        format!("cloudmail  {}", worker.as_deref().unwrap_or("(worker not configured: run `cloudmail setup`)"));
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
        n => format!(
            "{n} linked account{}; {} signed in",
            if n == 1 { "" } else { "s" },
            statuses.iter().filter(|s| s.ok).count()
        ),
    };
    let ids = statuses.iter().map(|s| s.name.clone()).collect();
    let mut crumbs = vec![
        crumb("add-gmail", "cloudmail account add gmail", "Link your Gmail account"),
        crumb("add-icloud", "cloudmail account add icloud", "Link your iCloud Mail (through icloud-session's sign-in)"),
        crumb("add-hey", "cloudmail account add hey", "Link your HEY account"),
    ];
    if !statuses.is_empty() {
        crumbs = vec![crumb("inbox", "cloudmail inbox", "Your Inbox with the linked accounts' merged in")];
        if statuses.iter().any(|s| s.provider == "hey") {
            crumbs.push(crumb(
                "feed",
                "cloudmail threads list --folder feed",
                "HEY's The Feed (also paper-trail, set-aside, reply-later)",
            ));
        }
        crumbs.push(crumb("remove", "cloudmail account remove <name>", "Unlink an account"));
    }
    Ok(Response::new(json!({ "worker": worker, "accounts": statuses }), summary).human(human).ids(ids).crumbs(crumbs))
}

fn add(ctx: &Ctx, link: Link) -> CliResult {
    // Your worker's Screener decides for Gmail and iCloud Mail, so linking one screens in the
    // people it already corresponds with when the worker is set up.
    let mail = ctx.mail().ok();
    let linked = accounts::link(&link, mail, &|line| eprintln!("{line}")).map_err(link_error)?;
    let label = linked.label;

    // Worth saying: forwarding to the address just linked would show everything twice, if not for dedupe.
    let forwarding = mail
        .and_then(|m| m.client.settings().ok())
        .map(|s| s.forward_to)
        .filter(|f| !f.is_empty() && linked.addresses.iter().any(|a| a.eq_ignore_ascii_case(f)));
    let mut summary = format!(
        "{} {label}{}; its mail now shows next to yours",
        if linked.replaced { "Updated" } else { "Linked" },
        if linked.addresses.is_empty() { String::new() } else { format!(" ({})", linked.addresses.join(", ")) }
    );
    match &linked.screened_in {
        Some(Ok(n)) => summary.push_str(&format!(
            ". Your Screener decides its new senders; {n} people it already corresponds with were screened in"
        )),
        Some(Err(why)) => summary
            .push_str(&format!(". Its correspondents couldn't be screened in ({why}), so they wait in the Screener")),
        None if linked.provider != "hey" => {
            summary.push_str(". Your Screener decides its new senders once your worker is set up")
        }
        None => {}
    }
    if let Some(f) = &forwarding {
        summary.push_str(&format!(". Your worker forwards to {f}, so {label}'s copies of that mail are hidden"));
    }
    let mut data = json!({
        "account": linked.name, "provider": linked.provider, "addresses": linked.addresses, "config_path": linked.config_path,
        "forwarding_to_this_account": forwarding,
        "screened_in": linked.screened_in.as_ref().and_then(|r| r.as_ref().ok()),
    });
    match linked.provider.as_str() {
        "gmail" => {
            data["gws_version"] = json!(linked.version);
            data["gws_dir"] = json!(linked.gws_dir);
        }
        "hey" => data["hey_version"] = json!(linked.version),
        _ => {}
    }
    let mut crumbs: Vec<Breadcrumb> =
        vec![crumb("inbox", "cloudmail inbox", &format!("Your Inbox with {label}'s merged in"))];
    match linked.provider.as_str() {
        "gmail" => crumbs.push(crumb(
            "search",
            "cloudmail search <words>",
            "Search your mail and Gmail together (Gmail reads its own search syntax)",
        )),
        "icloud" => {
            crumbs.push(crumb("search", "cloudmail search <words>", "Search your mail and iCloud Mail together"))
        }
        _ => crumbs.push(crumb(
            "feed",
            "cloudmail threads list --folder feed",
            "HEY's The Feed (also paper-trail, set-aside, reply-later)",
        )),
    }
    crumbs.push(crumb("screener", "cloudmail screener", "Senders waiting for a yes or no"));
    crumbs.push(crumb("list", "cloudmail account list", "Linked accounts"));
    Ok(Response::new(data, summary).crumbs(crumbs))
}

fn remove(name: &str) -> CliResult {
    let gone = accounts::unlink(name).map_err(|e| match e.kind {
        ErrorKind::NotFound => CliError::not_found(e.message).hint("see `cloudmail account list`"),
        _ => e.into(),
    })?;
    let summary = match (gone.provider.as_str(), gone.signed_out) {
        ("gmail", Some(true)) => format!(
            "Unlinked {name} and signed Cloudmail out of Gmail on this computer (removed {}); nothing changed in Gmail itself. To withdraw Cloudmail's access too, remove it at https://myaccount.google.com/connections",
            gone.gws_dir.as_deref().map(|d| d.display().to_string()).unwrap_or_default()
        ),
        ("gmail", _) => format!("Unlinked {name}; nothing changed in Gmail itself"),
        ("icloud", _) => format!(
            "Unlinked {name}; nothing changed in iCloud, and icloud-session, which other apps use, stays signed in"
        ),
        _ => format!("Unlinked {name}; nothing changed in the account itself, and its CLI is still signed in"),
    };
    Ok(Response::new(
        json!({ "account": name, "removed": true, "signed_out": gone.signed_out.unwrap_or(false) }),
        summary,
    ))
}
