//! Status, orientation, mailboxes, settings, local config and the command reference.

use clap::CommandFactory;
use serde_json::json;

use cloudmail_api::{MailboxUpdate, config};

use crate::Ctx;
use crate::cli::*;
use crate::docs;
use crate::mail::confirm;
use crate::output::{CliError, CliResult, Response, bold, crumb, dim, shell_arg};
use crate::render;
use crate::setup;

const TOP_COMMANDS: &[(&str, &str)] = &[
    ("cloudmail inbox", "List the Inbox"),
    ("cloudmail screener", "Senders waiting for a yes or no"),
    ("cloudmail thread read <id>", "Read a thread"),
    ("cloudmail reply <id> -m <text>", "Reply"),
    ("cloudmail compose --to <email> --subject <s> -m <text>", "Write a new message"),
    ("cloudmail search <words>", "Search all mail"),
    ("cloudmail watch", "Stream new mail"),
    ("cloudmail commands", "Every command, with examples"),
    ("cloudmail agent-guide", "How to script cloudmail / use it from an AI agent"),
];

pub fn orientation() -> Response {
    let cfg = config::load();
    let configured = cfg.is_ok();
    let path = config::path();
    let status_line = match &cfg {
        Ok(c) => format!("Configured: {} (config {})", c.api_url, path.display()),
        Err(_) => format!("Not configured yet: run `cloudmail setup`, or create {}", path.display()),
    };
    let mut human = format!(
        "{} {}\nYour own email on Cloudflare: read, screen and send mail from the terminal.\n\n{status_line}\n\n{}\n",
        bold("cloudmail"),
        env!("CARGO_PKG_VERSION"),
        bold("Common commands:")
    );
    for (c, d) in TOP_COMMANDS {
        human.push_str(&format!("  {c:<52} {}\n", dim(d)));
    }
    human.push_str("\nOutput is JSON when piped (or with --json). Run `cloudmail <command> --help` for details.");
    let crumbs = TOP_COMMANDS.iter().map(|(c, d)| crumb(c.split_whitespace().nth(1).unwrap_or(""), c, d)).collect();
    Response::new(
        json!({
            "name": "cloudmail",
            "version": env!("CARGO_PKG_VERSION"),
            "configured": configured,
            "config_path": path,
            "api_url": cfg.as_ref().ok().map(|c| c.api_url.clone()),
        }),
        if configured { "cloudmail is configured" } else { "cloudmail is not configured; run `cloudmail setup`" },
    )
    .human(human.trim_end())
    .crumbs(crumbs)
}

pub fn status(ctx: &Ctx) -> CliResult {
    let client = ctx.client()?;
    let counts = client.counts()?;
    let mailboxes = client.mailboxes()?;
    let settings = client.settings()?;
    let summary = format!(
        "{} unread of {} in the Inbox, {} in the Screener",
        counts.inbox_unread,
        counts.inbox,
        counts.screener
    );
    let human = format!(
        "Worker:     {} (ok)\nConfig:     {}\nInbox:      {}, {} unread\nScreener:   {} waiting\nMailboxes:  {}\nForwarding: {}",
        client.base_url(),
        config::path().display(),
        crate::mail::plural(counts.inbox as usize, "thread"),
        counts.inbox_unread,
        crate::mail::plural(counts.screener as usize, "sender"),
        mailboxes.len(),
        if settings.forward_to.is_empty() { "off".to_string() } else { format!("a copy of every message goes to {}", settings.forward_to) },
    );
    let mut crumbs = vec![crumb("inbox", "cloudmail inbox", "List the Inbox")];
    if counts.screener > 0 {
        crumbs.insert(0, crumb("screener", "cloudmail screener", "Decide on waiting senders"));
    }
    let mut data = json!({
        "api_url": client.base_url(),
        "config_path": config::path(),
        "healthy": true,
        "counts": counts,
        "mailboxes": mailboxes.len(),
        "forward_to": settings.forward_to,
    });
    let mut human = human;
    // Linked accounts only appear for people who added one.
    let mail = ctx.mail()?;
    if mail.has_accounts() {
        let statuses: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = mail.accounts.iter().map(|p| s.spawn(move || p.status())).collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        for st in &statuses {
            human.push_str(&format!("\n{:<12}{}", format!("{}:", st.label), if st.ok { format!("signed in ({})", st.addresses.join(", ")) } else { st.detail.clone() }));
        }
        data["accounts"] = json!(statuses);
    }
    Ok(Response::new(data, summary).human(human).crumbs(crumbs))
}

pub fn mailbox(ctx: &Ctx, cmd: MailboxCommand) -> CliResult {
    let client = ctx.client()?;
    match cmd {
        MailboxCommand::List => {
            let list = client.mailboxes()?;
            let summary = format!("{} mailbox{}", list.len(), if list.len() == 1 { "" } else { "es" });
            let human = if list.is_empty() { "No mailboxes yet; add one with `cloudmail mailbox add <address>`".into() } else { render::mailboxes(&list) };
            let ids = list.iter().map(|m| m.email.clone()).collect();
            Ok(Response::new(&list, summary).human(human).ids(ids).crumbs(vec![
                crumb("add", "cloudmail mailbox add <address> --route", "Add an address"),
                crumb("set", "cloudmail mailbox set <address> --screen false", "Deliver an address straight to the Inbox"),
            ]))
        }
        MailboxCommand::Add { email, name, direct, route } => {
            let (addr, spec_screen) = setup::parse_mailbox_spec(&email)?;
            let direct = direct || !spec_screen;
            let mailbox = client.put_mailbox(&addr, &MailboxUpdate { name, screen: Some(!direct), position: None })?;
            let mut data = json!({ "mailbox": mailbox });
            let mut summary = format!("Added {addr} ({})", if direct { "direct" } else { "screened" });
            if route.route {
                let r = setup::route_for_mailbox(&addr, &route, ctx.interactive())?;
                summary.push_str(&format!("; routing: {}", r["status"].as_str().unwrap_or("")));
                data["route"] = r;
            }
            let mut crumbs = vec![crumb("list", "cloudmail mailbox list", "List mailboxes")];
            if !route.route {
                crumbs.push(crumb("route", &format!("cloudmail mailbox add {} --route", shell_arg(&addr)), "Point the address's Email Routing rule at the worker"));
            }
            Ok(Response::new(data, summary).crumbs(crumbs))
        }
        MailboxCommand::Set { email, name, screen, position } => {
            if name.is_none() && screen.is_none() && position.is_none() {
                return Err(CliError::usage("nothing to change").hint("pass --name, --screen or --position"));
            }
            let email = email.to_ascii_lowercase();
            if !client.mailboxes()?.iter().any(|m| m.email == email) {
                return Err(CliError::not_found(format!("no mailbox {email}")).hint("add it with `cloudmail mailbox add`"));
            }
            let mailbox = client.put_mailbox(&email, &MailboxUpdate { name, screen, position })?;
            Ok(Response::new(&mailbox, format!("Updated {email}")))
        }
        MailboxCommand::Remove { email, yes } => {
            confirm(yes, &format!("Remove mailbox {email}? Mail to it will be screened as an unknown address."))?;
            client.delete_mailbox(&email)?;
            Ok(Response::new(json!({ "email": email }), format!("Removed {email}; its Email Routing rule was left in place")))
        }
    }
}

pub fn settings(ctx: &Ctx, cmd: SettingsCommand) -> CliResult {
    let client = ctx.client()?;
    let settings = match cmd {
        SettingsCommand::Get => client.settings()?,
        SettingsCommand::Set { key: SettingKey::ForwardTo, value } => client.update_settings(&json!({ "forward_to": value }))?,
    };
    let human = format!("forward_to: {}", if settings.forward_to.is_empty() { "(off)" } else { &settings.forward_to });
    Ok(Response::new(&settings, "Worker settings").human(human))
}

pub fn config_cmd(cmd: ConfigCommand) -> CliResult {
    let path = config::path();
    match cmd {
        ConfigCommand::Path => Ok(Response::new(json!({ "path": path }), path.display().to_string())),
        ConfigCommand::Show { show_token } => {
            let file = config::read_file(&path)?.unwrap_or_default();
            let effective = config::load();
            if let Err(e) = &effective
                && e.kind == cloudmail_api::ErrorKind::Config
                && e.message.contains("keyring")
            {
                return Err(e.clone().into());
            }
            let effective = effective.ok();
            let token = effective.as_ref().map(|c| c.api_token.clone());
            let shown_token = token.as_ref().map(|t| if show_token { t.clone() } else { redact(t) });
            let env_override = config::env_overrides();
            let data = json!({
                "path": path,
                "exists": path.exists(),
                "api_url": effective.as_ref().map(|c| c.api_url.clone()).or(file.api_url),
                "api_token": shown_token,
                "api_token_kept_in": "keyring",
                "poll_seconds": effective.as_ref().map(|c| c.poll_seconds).or(file.poll_seconds),
                "env_overrides": env_override,
            });
            let human = format!(
                "path:         {}\napi_url:      {}\napi_token:    {}\npoll_seconds: {}{}",
                path.display(),
                data["api_url"].as_str().unwrap_or("(not set)"),
                data["api_token"].as_str().unwrap_or("(not set)"),
                data["poll_seconds"].as_u64().map(|p| p.to_string()).unwrap_or_else(|| format!("(default {})", config::DEFAULT_POLL_SECONDS)),
                if env_override.is_empty() { String::new() } else { format!("\nenv overrides: {}", env_override.join(", ")) }
            );
            Ok(Response::new(data, format!("Config at {}", path.display())).human(human))
        }
        ConfigCommand::Set { key, value } => {
            let mut file = config::read_file(&path)?.unwrap_or_default();
            config::migrate_secrets(&mut file)?;
            match key {
                ConfigKey::ApiUrl => {
                    if !value.starts_with("http://") && !value.starts_with("https://") {
                        return Err(CliError::usage("api_url must start with https://"));
                    }
                    file.api_url = Some(value.trim_end_matches('/').to_string());
                }
                ConfigKey::ApiToken => {
                    cloudmail_api::keyring::set(cloudmail_api::keyring::API_TOKEN, "Cloudmail API token", value.trim())?;
                    return Ok(Response::new(json!({ "kept_in": "keyring" }), "Updated the API token in the keyring")
                        .crumbs(vec![crumb("status", "cloudmail status", "Check the connection")]));
                }
                ConfigKey::PollSeconds => {
                    file.poll_seconds = Some(value.parse().map_err(|_| CliError::usage("poll_seconds must be a number"))?);
                }
            }
            let written = config::save(&file)?;
            Ok(Response::new(json!({ "path": written }), format!("Updated {}", written.display()))
                .crumbs(vec![crumb("status", "cloudmail status", "Check the connection")]))
        }
    }
}

fn redact(t: &str) -> String {
    let chars: Vec<char> = t.chars().collect();
    if chars.len() <= 8 {
        return "********".into();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

pub fn commands() -> Response {
    let tree = docs::commands_json(&Cli::command());
    let n = tree["commands"].as_array().map(|a| a.len()).unwrap_or(0);
    let text = docs::commands_text(&tree);
    let ids = tree["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["command"].as_str().map(str::to_string))
        .collect();
    Response::new(tree, format!("{n} commands")).human(text).ids(ids).crumbs(vec![crumb("guide", "cloudmail agent-guide", "Scripting and agent guide")])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_tokens() {
        assert_eq!(redact("0123456789abcdef"), "0123…cdef");
        assert_eq!(redact("short"), "********");
        assert_eq!(redact("aéééééééééz"), "aééé…éééz");
    }
}
