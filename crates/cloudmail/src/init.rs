//! `cloudmail init`: getting started in one command. At a terminal it asks, in order: an invite link,
//! your own addresses (setup, which also moves a domain onto Cloudflare), then Gmail, iCloud Mail
//! and HEY. Piped, it does what its flags say, and with none reports what's set up and what's next.

use serde_json::{Value, json};

use cloudmail_api::accounts::{self, Link, Readiness};
use cloudmail_api::config;

use crate::Ctx;
use crate::cli::{ConnectArgs, InitArgs, SetupArgs};
use crate::output::{Breadcrumb, CliResult, ask_yes, bold, crumb, dim, prompt};
use crate::{accounts as account_cmd, admin, setup};

/// One thing init did, or tried to.
fn step(name: &str, result: CliResult) -> Value {
    match result {
        Ok(r) => json!({ "step": name, "ok": true, "summary": r.summary, "data": r.data }),
        Err(e) => json!({ "step": name, "ok": false, "error": e.json()["error"] }),
    }
}

fn say(interactive: bool, v: &Value) {
    if !interactive {
        return;
    }
    match v["ok"].as_bool() {
        Some(true) => eprintln!("  ✓ {}", v["summary"].as_str().unwrap_or("done")),
        _ => {
            eprintln!("  ✗ {}", v["error"]["message"].as_str().unwrap_or("failed"));
            if let Some(h) = v["error"]["hint"].as_str() {
                eprintln!("    {}", dim(h));
            }
        }
    }
}

fn has_worker() -> bool {
    config::load().is_ok_and(|c| c.has_worker())
}

/// Whether an account of this provider is linked (whatever it's named).
fn is_linked(provider: &str) -> bool {
    config::read_file(&config::path())
        .ok()
        .flatten()
        .is_some_and(|f| f.accounts.iter().any(|(name, cfg)| cfg.provider(name) == provider))
}

fn link(ctx: &Ctx, r: &Readiness) -> Value {
    let request = Link {
        provider: r.provider.to_string(),
        // Asked for by name, so a sign-in may open even without a terminal (on this desktop).
        can_sign_in: true,
        ..Default::default()
    };
    step(&format!("link {}", r.provider), account_cmd::add(ctx, request))
}

fn setup_args(args: &InitArgs, mailboxes: Vec<String>) -> SetupArgs {
    SetupArgs {
        mailboxes,
        forward_to: args.forward_to.clone(),
        yes: args.yes,
        account: args.account.clone(),
        dry_run: false,
        force: false,
        mailbox_flags: Vec::new(),
        name: "cloudmail".into(),
        worker_dir: None,
        cf: "npx cf".into(),
    }
}

pub fn run(ctx: &Ctx, args: &InitArgs) -> CliResult {
    let interactive = ctx.interactive();
    let asked = args.connect.is_some() || !args.mailboxes.is_empty() || !args.link.is_empty();
    let guided = interactive && !asked;
    let mut steps = Vec::new();

    if guided {
        eprintln!(
            "{}\nEnter skips any question; you can do each later.\n",
            bold("Let's set up Cloudmail.")
        );
    }

    // 1. Someone else's worker, with their invite link.
    let invite = match &args.connect {
        Some(link) => Some(link.clone()),
        None if guided && !has_worker() => Some(prompt(
            "Did someone invite you? Paste the invite link (or press Enter):",
        )?)
        .filter(|l| !l.is_empty()),
        None => None,
    };
    if let Some(invite) = invite {
        let v = step("connect", admin::connect(&ConnectArgs { invite, force: false }));
        say(interactive, &v);
        steps.push(v);
    }

    // 2. Your own domains, on your own Cloudflare account.
    let mailboxes = if !args.mailboxes.is_empty() {
        args.mailboxes.clone()
    } else if guided && !has_worker() {
        eprintln!(
            "\nYour own domain: Cloudmail runs on your Cloudflare account (free to receive; sending needs Workers Paid).\nA domain not on Cloudflare yet is moved there, keeping its website and current mail working."
        );
        prompt("Addresses to receive mail at, e.g. you@example.com support@example.com:direct (or press Enter):")?
            .split_whitespace()
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };
    if !mailboxes.is_empty() {
        let v = step("setup", setup::run(&setup_args(args, mailboxes), interactive));
        say(interactive, &v);
        steps.push(v);
    }

    // 3. Gmail, iCloud Mail and HEY alongside.
    let ready = accounts::readiness();
    let wanted = |p: &str| {
        args.link
            .iter()
            .any(|l| l.eq_ignore_ascii_case(p) || l.eq_ignore_ascii_case("all"))
    };
    if guided {
        eprintln!("\nYour other mail, shown alongside (each opens its own sign-in once):");
    }
    for r in &ready {
        if is_linked(r.provider) {
            if guided {
                eprintln!("  ✓ {} is linked", r.label);
            }
            continue;
        }
        let go = if guided {
            match &r.hint {
                Some(h) if !r.ready => {
                    eprintln!("  · {}: not ready here; {h}", r.label);
                    false
                }
                _ => ask_yes(&format!(
                    "  Link {}{}?",
                    r.label,
                    if r.signed_in { " (already signed in)" } else { "" }
                ))?,
            }
        } else {
            wanted(r.provider) && (r.ready || !args.link.iter().any(|l| l.eq_ignore_ascii_case("all")))
        };
        if go {
            let v = link(ctx, r);
            say(interactive, &v);
            steps.push(v);
        }
    }

    let worker = has_worker();
    let accounts: Vec<Value> = ready
        .iter()
        .map(|r| {
            let mut v = serde_json::to_value(r).unwrap_or_default();
            v["linked"] = json!(is_linked(r.provider));
            v
        })
        .collect();
    let failed = steps.iter().filter(|s| s["ok"] == false).count();
    let linked_now: Vec<&str> = ready
        .iter()
        .filter(|r| is_linked(r.provider))
        .map(|r| r.label)
        .collect();
    let mut summary = match (worker, linked_now.is_empty()) {
        (true, true) => "Your Cloudmail worker is ready".to_string(),
        (true, false) => format!(
            "Your Cloudmail worker is ready, with {} alongside",
            linked_now.join(", ")
        ),
        (false, false) => format!("Reading {} (no worker of your own)", linked_now.join(", ")),
        (false, true) => "Nothing set up yet".to_string(),
    };
    if failed > 0 {
        summary.push_str(&format!("; {failed} step{} failed", if failed == 1 { "" } else { "s" }));
    }

    let mut crumbs: Vec<Breadcrumb> = Vec::new();
    if worker || !linked_now.is_empty() {
        crumbs.push(crumb("inbox", "cloudmail inbox", "Your Inbox"));
        crumbs.push(crumb(
            "screener",
            "cloudmail screener",
            "Senders waiting for a yes or no",
        ));
    }
    if !worker {
        crumbs.push(crumb(
            "connect",
            "cloudmail init --connect '<invite link>'",
            "Invited? Connect with the link",
        ));
        crumbs.push(crumb(
            "setup",
            "cloudmail setup you@yourdomain.com --dry-run",
            "Your own domains on your Cloudflare account (preview first; a domain not on Cloudflare is added)",
        ));
    }
    let unlinked: Vec<&Readiness> = ready.iter().filter(|r| !is_linked(r.provider)).collect();
    if unlinked.iter().any(|r| r.ready) {
        let names: Vec<&str> = unlinked.iter().filter(|r| r.ready).map(|r| r.provider).collect();
        crumbs.push(crumb(
            "link",
            &format!("cloudmail init --link {}", names.join(",")),
            "Link these (each opens its sign-in on this desktop unless already signed in)",
        ));
    }

    let mut human = String::new();
    if !guided {
        for s in &steps {
            let mark = if s["ok"] == true { "✓" } else { "✗" };
            let text = s["summary"].as_str().or(s["error"]["message"].as_str()).unwrap_or("");
            human.push_str(&format!("{mark} {}: {text}\n", s["step"].as_str().unwrap_or("")));
        }
    }
    human.push_str(&format!("\n{summary}."));
    if worker || !linked_now.is_empty() {
        human.push_str("\nOpen Cloudmail from your app launcher, or run `cloudmail inbox`.");
    } else {
        human.push_str("\nRun `cloudmail init` again whenever you're ready.");
    }
    Ok(crate::output::Response::new(
        json!({ "worker": worker, "steps": steps, "accounts": accounts }),
        summary,
    )
    .human(human.trim_start().to_string())
    .crumbs(crumbs))
}
