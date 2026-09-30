//! `cloudmail account`: link other mail accounts (HEY) next to your worker. Opt-in; nothing
//! changes for anyone who never runs `account add`.

use serde_json::json;

use cloudmail_api::config::{self, AccountConfig};
use cloudmail_api::hey::Hey;
use cloudmail_api::provider::{self, KNOWN_PROVIDERS, Provider};
use cloudmail_api::AccountStatus;

use crate::Ctx;
use crate::cli::AccountCommand;
use crate::output::{CliError, CliResult, Response, crumb, dim, exit};

const HEY_INSTALL: &str = "install the hey CLI (https://github.com/basecamp/hey-cli, e.g. `mise use -g github:basecamp/hey-cli`), or pass --command <path>";

pub fn account(ctx: &Ctx, cmd: AccountCommand) -> CliResult {
    match cmd {
        AccountCommand::List => list(),
        AccountCommand::Add { provider, name, command, account, no_login } => add(ctx, &provider, name, command, account, no_login),
        AccountCommand::Remove { name } => remove(&name),
    }
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
        human.push_str(&format!("\n\n{}", dim("No linked accounts. `cloudmail account add hey` shows your HEY mail here too.")));
    }
    let summary = match statuses.len() {
        0 => "No linked accounts".to_string(),
        n => format!("{n} linked account{}; {} signed in", if n == 1 { "" } else { "s" }, statuses.iter().filter(|s| s.ok).count()),
    };
    let ids = statuses.iter().map(|s| s.name.clone()).collect();
    let mut crumbs = vec![crumb("add", "cloudmail account add hey", "Link your HEY account")];
    if !statuses.is_empty() {
        crumbs = vec![
            crumb("inbox", "cloudmail inbox", "Your Inbox with HEY's Imbox merged in"),
            crumb("feed", "cloudmail threads list --folder feed", "HEY's The Feed (also paper-trail, set-aside, reply-later)"),
            crumb("remove", "cloudmail account remove <name>", "Unlink an account"),
        ];
    }
    Ok(Response::new(json!({ "worker": worker, "accounts": statuses }), summary).human(human).ids(ids).crumbs(crumbs))
}

fn add(ctx: &Ctx, provider_name: &str, name: Option<String>, command: Option<String>, account: Option<String>, no_login: bool) -> CliResult {
    let provider_name = provider_name.to_ascii_lowercase();
    if !KNOWN_PROVIDERS.iter().any(|(p, _)| *p == provider_name) {
        let known = KNOWN_PROVIDERS.iter().map(|(p, d)| format!("{p} ({d})")).collect::<Vec<_>>().join(", ");
        return Err(CliError::usage(format!("unknown provider \"{provider_name}\"")).hint(format!("known: {known}")));
    }
    let name = name.unwrap_or_else(|| provider_name.clone());
    if !provider::valid_name(&name) {
        return Err(CliError::usage(format!("\"{name}\" can't be an account name")).hint("use lowercase letters, digits and dashes (it prefixes the account's IDs)"));
    }
    let cfg = AccountConfig { provider: (name != provider_name).then(|| provider_name.clone()), command, account };
    let hey = Hey::new(&name, &cfg);
    let version = hey.version().map_err(|e| CliError::new("not_installed", exit::GENERIC, e.message).hint(HEY_INSTALL))?;
    if !hey.signed_in()? {
        if no_login || !ctx.interactive() {
            return Err(CliError::new("not_logged_in", exit::AUTH, "HEY isn't signed in on this computer")
                .hint(format!("run `{} auth login` (one browser sign-in), then `cloudmail account add {provider_name}` again", hey.command())));
        }
        eprintln!("Signing in to HEY in your browser (`{} auth login`)…", hey.command());
        hey.login()?;
        if !hey.signed_in()? {
            return Err(CliError::new("not_logged_in", exit::AUTH, "HEY still isn't signed in").hint(format!("run `{} auth login` and try again", hey.command())));
        }
    }
    let addresses: Vec<String> = hey.identities().unwrap_or_default().into_iter().map(|a| a.email).collect();

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
        "{} HEY{}; its mail now shows next to yours",
        if replaced { "Updated" } else { "Linked" },
        if addresses.is_empty() { String::new() } else { format!(" ({})", addresses.join(", ")) }
    );
    if let Some(f) = &forwarding {
        summary.push_str(&format!(". Your worker forwards to {f}, so HEY's copies of that mail are hidden"));
    }
    Ok(Response::new(
        json!({ "account": name, "provider": provider_name, "addresses": addresses, "hey_version": version, "config_path": written, "forwarding_to_this_account": forwarding }),
        summary,
    )
    .crumbs(vec![
        crumb("inbox", "cloudmail inbox", "Your Inbox with HEY's Imbox merged in"),
        crumb("screener", "cloudmail screener", "Both Screeners"),
        crumb("feed", "cloudmail threads list --folder feed", "HEY's The Feed (also paper-trail, set-aside, reply-later)"),
        crumb("list", "cloudmail account list", "Linked accounts"),
    ]))
}

fn remove(name: &str) -> CliResult {
    let path = config::path();
    let mut file = config::read_file(&path)?.unwrap_or_default();
    if file.accounts.remove(name).is_none() {
        return Err(CliError::not_found(format!("no linked account {name}")).hint("see `cloudmail account list`"));
    }
    config::save(&file)?;
    Ok(Response::new(json!({ "account": name, "removed": true }), format!("Unlinked {name}; nothing changed in the account itself, and its CLI is still signed in")))
}
