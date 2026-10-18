//! Status, orientation, mailboxes, settings, local config and the command reference.

use clap::CommandFactory;
use serde_json::json;

use cloudmail_api::text::long_time;
use cloudmail_api::{Client, ErrorKind, MailboxUpdate, config, keyring};

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
    (
        "cloudmail compose --to <email> --subject <s> -m <text>",
        "Write a new message",
    ),
    ("cloudmail search <words>", "Search all mail"),
    ("cloudmail watch", "Stream new mail"),
    ("cloudmail commands", "Every command, with examples"),
    (
        "cloudmail agent-guide",
        "How to script cloudmail / use it from an AI agent",
    ),
];

pub fn orientation() -> Response {
    let cfg = config::load();
    let configured = cfg.is_ok();
    let path = config::path();
    let status_line = match &cfg {
        Ok(c) if c.has_worker() => format!("Configured: {} (config {})", c.api_url, path.display()),
        Ok(_) => format!(
            "Linked accounts only, no Cloudmail worker (config {}); `cloudmail connect <invite link>` or `cloudmail setup` adds one",
            path.display()
        ),
        Err(_) => format!(
            "Not configured yet: run `cloudmail connect <invite link>` if someone invited you, or `cloudmail setup` to deploy your own (config: {})",
            path.display()
        ),
    };
    let mut human = format!(
        "{} {}\nYour own email on Cloudflare, with Gmail, iCloud Mail and HEY alongside: read, screen and send mail from the terminal.\n\n{status_line}\n\n{}\n",
        bold("cloudmail"),
        env!("CARGO_PKG_VERSION"),
        bold("Common commands:")
    );
    for (c, d) in TOP_COMMANDS {
        human.push_str(&format!("  {c:<52} {}\n", dim(d)));
    }
    human.push_str("\nOutput is JSON when piped (or with --json). Run `cloudmail <command> --help` for details.");
    let crumbs = TOP_COMMANDS
        .iter()
        .map(|(c, d)| crumb(c.split_whitespace().nth(1).unwrap_or(""), c, d))
        .collect();
    Response::new(
        json!({
            "name": "cloudmail",
            "version": env!("CARGO_PKG_VERSION"),
            "configured": configured,
            "config_path": path,
            "api_url": cfg.as_ref().ok().filter(|c| c.has_worker()).map(|c| c.api_url.clone()),
        }),
        if configured {
            "cloudmail is configured"
        } else {
            "cloudmail is not configured; run `cloudmail connect <invite link>` (invited), `cloudmail setup` (your own), or `cloudmail account add gmail|icloud|hey` (linked accounts only)"
        },
    )
    .human(human.trim_end())
    .crumbs(crumbs)
}

pub fn status(ctx: &Ctx) -> CliResult {
    let mail = ctx.mail()?;
    let Some(client) = &mail.client else {
        // Linked accounts only: they are the whole status.
        let (statuses, human) = account_statuses(mail);
        let data = json!({
            "api_url": null,
            "config_path": config::path(),
            "healthy": true,
            "user": null,
            "accounts": statuses,
        });
        let human = format!(
            "Worker:      none (linked accounts only)\nConfig:      {}{human}",
            config::path().display()
        );
        return Ok(Response::new(data, "Linked accounts only, no Cloudmail worker")
            .human(human)
            .crumbs(vec![
                crumb("inbox", "cloudmail inbox", "List the Inbox"),
                crumb("connect", "cloudmail connect <invite link>", "Join someone's Cloudmail"),
                crumb(
                    "setup",
                    "cloudmail setup you@yourdomain.com",
                    "Mail on your own domains",
                ),
            ]));
    };
    let counts = client.counts()?;
    let mailboxes = client.mailboxes(false)?;
    let settings = client.settings(None)?;
    // A worker from before users has no /api/me.
    let me = client.me().ok();
    let summary = format!(
        "{} unread of {} in the Inbox, {} in the Screener",
        counts.inbox_unread, counts.inbox, counts.screener
    );
    let human = format!(
        "Worker:      {} (ok){}\nConfig:      {}\nInbox:       {}, {} unread\nScreener:    {} waiting\nMailboxes:   {}\nForwarding:  {}",
        client.base_url(),
        me.as_ref()
            .map(|m| format!("\nSigned in:   {} ({})", m.name, m.role))
            .unwrap_or_default(),
        config::path().display(),
        crate::mail::plural(counts.inbox as usize, "thread"),
        counts.inbox_unread,
        crate::mail::plural(counts.screener as usize, "sender"),
        mailboxes.len(),
        if settings.forward_to.is_empty() {
            "off".to_string()
        } else {
            format!("a copy of every message goes to {}", settings.forward_to)
        },
    );
    let mut crumbs = vec![crumb("inbox", "cloudmail inbox", "List the Inbox")];
    if counts.screener > 0 {
        crumbs.insert(0, crumb("screener", "cloudmail screener", "Decide on waiting senders"));
    }
    let mut data = json!({
        "api_url": client.base_url(),
        "config_path": config::path(),
        "healthy": true,
        "user": me,
        "counts": counts,
        "mailboxes": mailboxes.len(),
        "forward_to": settings.forward_to,
    });
    let mut human = human;
    // Linked accounts only appear for people who added one.
    if mail.has_accounts() {
        let (statuses, lines) = account_statuses(mail);
        human.push_str(&lines);
        data["accounts"] = json!(statuses);
    }
    Ok(Response::new(data, summary).human(human).crumbs(crumbs))
}

/// Each linked account's status, and a line each for people.
fn account_statuses(mail: &cloudmail_api::Mail) -> (Vec<cloudmail_api::AccountStatus>, String) {
    let statuses: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = mail.accounts.iter().map(|p| s.spawn(move || p.status())).collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    let lines = statuses
        .iter()
        .map(|st| {
            format!(
                "\n{:<13}{}",
                format!("{}:", st.label),
                if st.ok {
                    format!("signed in ({})", st.addresses.join(", "))
                } else {
                    st.detail.clone()
                }
            )
        })
        .collect();
    (statuses, lines)
}

pub fn mailbox(ctx: &Ctx, cmd: MailboxCommand) -> CliResult {
    let client = ctx.client()?;
    match cmd {
        MailboxCommand::List { all } => {
            let list = client.mailboxes(all)?;
            let summary = format!("{} mailbox{}", list.len(), if list.len() == 1 { "" } else { "es" });
            let human = if list.is_empty() {
                "No mailboxes yet; add one with `cloudmail mailbox add <address>`".into()
            } else {
                render::mailboxes(&list, all)
            };
            let ids = list.iter().map(|m| m.email.clone()).collect();
            Ok(Response::new(&list, summary).human(human).ids(ids).crumbs(vec![
                crumb("add", "cloudmail mailbox add <address> --route", "Add an address"),
                crumb(
                    "set",
                    "cloudmail mailbox set <address> --screen false",
                    "Deliver an address straight to the Inbox",
                ),
            ]))
        }
        MailboxCommand::Add {
            email,
            name,
            direct,
            user,
            route,
        } => {
            let (addr, spec_screen) = setup::parse_mailbox_spec(&email)?;
            let direct = direct || !spec_screen;
            let mailbox = client.put_mailbox(
                &addr,
                &MailboxUpdate {
                    user: user.clone(),
                    name,
                    screen: Some(!direct),
                    position: None,
                },
            )?;
            let mut data = json!({ "mailbox": mailbox });
            let mut summary = format!("Added {addr} ({})", if direct { "direct" } else { "screened" });
            if let Some(u) = &user {
                summary.push_str(&format!(" for {u}"));
            }
            if route.route {
                let r = setup::route_for_mailbox(&addr, &route, ctx.interactive())?;
                summary.push_str(&format!("; routing: {}", r["status"].as_str().unwrap_or("")));
                data["route"] = r;
            }
            let mut crumbs = vec![crumb("list", "cloudmail mailbox list", "List mailboxes")];
            if !route.route {
                crumbs.push(crumb(
                    "route",
                    &format!("cloudmail mailbox add {} --route", shell_arg(&addr)),
                    "Point the address's Email Routing rule at the worker",
                ));
            }
            Ok(Response::new(data, summary).crumbs(crumbs))
        }
        MailboxCommand::Set {
            email,
            name,
            screen,
            position,
        } => {
            if name.is_none() && screen.is_none() && position.is_none() {
                return Err(CliError::usage("nothing to change").hint("pass --name, --screen or --position"));
            }
            let email = email.to_ascii_lowercase();
            if !client.mailboxes(false)?.iter().any(|m| m.email == email) {
                return Err(
                    CliError::not_found(format!("no mailbox {email}")).hint("add it with `cloudmail mailbox add`")
                );
            }
            let mailbox = client.put_mailbox(
                &email,
                &MailboxUpdate {
                    user: None,
                    name,
                    screen,
                    position,
                },
            )?;
            Ok(Response::new(&mailbox, format!("Updated {email}")))
        }
        MailboxCommand::Remove { email, yes } => {
            confirm(
                yes,
                &format!("Remove mailbox {email}? Mail to it will be screened as an unknown address."),
            )?;
            client.delete_mailbox(&email)?;
            Ok(Response::new(
                json!({ "email": email }),
                format!("Removed {email}; its Email Routing rule was left in place"),
            ))
        }
    }
}

pub fn settings(ctx: &Ctx, cmd: SettingsCommand) -> CliResult {
    let client = ctx.client()?;
    let settings = match cmd {
        SettingsCommand::Get { user } => client.settings(user.as_deref())?,
        SettingsCommand::Set {
            key: SettingKey::ForwardTo,
            value,
            user,
        } => client.update_settings(user.as_deref(), &json!({ "forward_to": value }))?,
    };
    let human = format!(
        "forward_to: {}",
        if settings.forward_to.is_empty() {
            "(off)"
        } else {
            &settings.forward_to
        }
    );
    Ok(Response::new(&settings, "Worker settings").human(human))
}

pub fn user(ctx: &Ctx, cmd: UserCommand) -> CliResult {
    let client = ctx.client()?;
    match cmd {
        UserCommand::List => {
            let list = client.users()?;
            let w = list.iter().map(|u| u.name.len()).max().unwrap_or(0);
            let human = list
                .iter()
                .map(|u| {
                    let state = u.state();
                    format!(
                        "{}  {}  {}  {}",
                        render::pad(&render::clean_line(&u.name), w),
                        render::pad(&u.role, 5),
                        render::pad(&state, 13),
                        dim(&u.mailboxes.join(", "))
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let ids = list.iter().map(|u| u.name.clone()).collect();
            let summary = crate::mail::plural(list.len(), "user");
            Ok(Response::new(&list, summary).human(human).ids(ids).crumbs(vec![crumb(
                "invite",
                "cloudmail user invite <name> --mailbox <address>",
                "Let someone read their own mail here",
            )]))
        }
        UserCommand::Invite { name, mailboxes } => {
            let name = name.trim().to_ascii_lowercase();
            let (_, invite) = client.invite_user(&name, &mailboxes)?;
            let human = format!(
                "Invite for {name}, valid once until {}:\n\n  {}\n\nSend them the link. On their computer, with Cloudmail installed:\n\n  cloudmail connect {}",
                long_time(invite.expires_at),
                invite.url,
                shell_arg(&invite.url)
            );
            let mut crumbs = vec![crumb("list", "cloudmail user list", "See who's connected")];
            for m in &mailboxes {
                crumbs.push(crumb(
                    "route",
                    &format!(
                        "cloudmail mailbox add {} --user {} --route",
                        shell_arg(m),
                        shell_arg(&name)
                    ),
                    "Point the address's Email Routing rule at the worker",
                ));
            }
            Ok(Response::new(
                json!({ "user": name, "invite": invite }),
                format!("Invite for {name}: {}", invite.url),
            )
            .human(human)
            .crumbs(crumbs))
        }
        UserCommand::Revoke { name, purge, yes } => {
            let question = if purge {
                format!(
                    "Sign {name} out and delete all their mail, decisions, settings and mailboxes? This can't be undone."
                )
            } else {
                format!(
                    "Sign {name} out everywhere? Their mail stays; `cloudmail user invite {name}` lets them back in."
                )
            };
            confirm(yes, &question)?;
            client.revoke_user(&name, purge)?;
            let summary = if purge {
                format!("Removed {name} and their mail; remove their addresses' Email Routing rules too")
            } else {
                format!("Signed {name} out; their mail keeps arriving and is kept")
            };
            Ok(Response::new(json!({ "user": name, "purged": purge }), summary))
        }
    }
}

/// Splits an invite link into the worker's URL and the code.
fn parse_invite(link: &str) -> Option<(&str, &str)> {
    let (url, code) = link.trim().split_once("/invite/")?;
    let code = code.trim_end_matches('/');
    let web = url.starts_with("https://") || url.starts_with("http://");
    (web && code.starts_with("inv_") && !code.contains(['/', '?', '#'])).then_some((url, code))
}

pub fn connect(args: &ConnectArgs) -> CliResult {
    let (api_url, code) = parse_invite(&args.invite).ok_or_else(|| {
        CliError::usage("that isn't a Cloudmail invite link")
            .hint("it looks like https://cloudmail.<name>.workers.dev/invite/inv_…")
    })?;
    let path = config::path();
    let existing = config::read_file(&path)?.unwrap_or_default();
    if let Some(current) = existing.api_url.as_deref().filter(|u| !u.trim().is_empty())
        && !args.force
    {
        return Err(
            CliError::generic(format!("this machine is already connected to {current}"))
                .hint("`cloudmail connect <link> --force` replaces it and its token"),
        );
    }
    // The invite works once: make sure there's a keyring to keep the token in before using it up.
    keyring::get(keyring::API_TOKEN)?;
    let connected = Client::connect(api_url, code).map_err(|e| {
        let unusable = e.kind == ErrorKind::Unauthorized;
        let err = CliError::from(e);
        if unusable {
            err.hint("ask whoever invited you for a new link (`cloudmail user invite <name>`)")
        } else {
            err
        }
    })?;
    keyring::set(keyring::API_TOKEN, "Cloudmail API token", &connected.token)?;
    let file = config::FileConfig {
        api_url: Some(api_url.to_string()),
        poll_seconds: existing.poll_seconds.or(Some(config::DEFAULT_POLL_SECONDS)),
        ..existing
    };
    config::save(&file)?;
    let summary = format!("Connected to {api_url} as {}", connected.user.name);
    Ok(
        Response::new(json!({ "api_url": api_url, "user": connected.user }), summary).crumbs(vec![
            crumb("status", "cloudmail status", "Check the connection"),
            crumb("inbox", "cloudmail inbox", "List the Inbox"),
        ]),
    )
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
                data["poll_seconds"]
                    .as_u64()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| format!("(default {})", config::DEFAULT_POLL_SECONDS)),
                if env_override.is_empty() {
                    String::new()
                } else {
                    format!("\nenv overrides: {}", env_override.join(", "))
                }
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
                    cloudmail_api::keyring::set(
                        cloudmail_api::keyring::API_TOKEN,
                        "Cloudmail API token",
                        value.trim(),
                    )?;
                    return Ok(
                        Response::new(json!({ "kept_in": "keyring" }), "Updated the API token in the keyring")
                            .crumbs(vec![crumb("status", "cloudmail status", "Check the connection")]),
                    );
                }
                ConfigKey::PollSeconds => {
                    file.poll_seconds = Some(
                        value
                            .parse()
                            .map_err(|_| CliError::usage("poll_seconds must be a number"))?,
                    );
                }
            }
            let written = config::save(&file)?;
            Ok(
                Response::new(json!({ "path": written }), format!("Updated {}", written.display()))
                    .crumbs(vec![crumb("status", "cloudmail status", "Check the connection")]),
            )
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
    Response::new(tree, format!("{n} commands"))
        .human(text)
        .ids(ids)
        .crumbs(vec![crumb(
            "guide",
            "cloudmail agent-guide",
            "Scripting and agent guide",
        )])
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
