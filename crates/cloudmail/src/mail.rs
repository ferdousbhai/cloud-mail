//! Reading, screening and sending mail.

use serde_json::json;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use cloudmail_api::text::{bare_email, html_to_text, human_size, quote, reply_subject, short_time, split_addresses, unused_path};
use cloudmail_api::{ErrorKind, SendRequest, ThreadDetail, ThreadQuery, ThreadSummary};

use crate::Ctx;
use crate::cli::*;
use crate::output::{self, CliError, CliResult, Response, crumb, exit};
use crate::render;

/// Names the missing object in a not-found error.
fn missing<'a>(what: &'a str, id: &'a str, list_hint: &'a str) -> impl FnOnce(cloudmail_api::Error) -> CliError + 'a {
    move |e| {
        if e.kind == ErrorKind::NotFound {
            CliError::not_found(format!("no {what} {id}")).hint(list_hint)
        } else {
            e.into()
        }
    }
}

/// "1 thread", "2 threads".
fn plural(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

fn thread_crumbs() -> Vec<output::Breadcrumb> {
    vec![
        crumb("read", "cloudmail thread read <thread-id>", "Read a thread"),
        crumb("archive", "cloudmail thread archive <thread-id>", "Archive a thread"),
        crumb("reply", "cloudmail reply <thread-id> -m <text>", "Reply to a thread"),
        crumb("compose", "cloudmail compose --to <email> --subject <subject> -m <text>", "Write a new message"),
    ]
}

pub fn list(ctx: &Ctx, folder: Folder, a: &ListArgs) -> CliResult {
    let client = ctx.client()?;
    let mut threads = client.list_threads(&ThreadQuery {
        folder: folder.as_str().into(),
        q: None,
        before: a.before,
        since: a.since,
        limit: a.limit,
        unread: a.unread,
    })?;
    // The page as the worker returned it decides whether there is more; an older worker that
    // ignores `unread=1` is filtered here too.
    let full_page = threads.len() as u32 >= a.limit;
    let oldest = threads.last().map(|t| t.last_at);
    if a.unread {
        threads.retain(|t| t.unread);
    }
    let where_ = match folder {
        Folder::All => "all folders",
        Folder::Sent => "Sent",
        f => f.as_str(),
    };
    let summary = format!("{} in {where_}", plural(threads.len(), "thread"));
    let mut crumbs = thread_crumbs();
    if full_page && let Some(before) = oldest {
        let mut next = format!("cloudmail threads list --folder {} --before {before} --limit {}", folder.as_str(), a.limit);
        if a.unread {
            next.push_str(" --unread");
        }
        if let Some(since) = a.since {
            next.push_str(&format!(" --since {since}"));
        }
        crumbs.push(crumb("more", &next, "Next page"));
    }
    if folder == Folder::Screener {
        crumbs.insert(0, crumb("approve", "cloudmail screener approve <email>", "Screen a sender in"));
    }
    let human = render::threads(&threads, folder == Folder::All);
    let ids = threads.iter().map(|t| t.id.clone()).collect();
    Ok(Response::new(&threads, summary).human(human).ids(ids).crumbs(crumbs).meta("folder", folder.as_str()))
}

pub fn search(ctx: &Ctx, a: &SearchArgs) -> CliResult {
    let query = a.query.join(" ");
    let threads = ctx.client()?.list_threads(&ThreadQuery { folder: "all".into(), q: Some(query.clone()), limit: a.limit, ..Default::default() })?;
    let summary = format!("{} matching \"{query}\"", plural(threads.len(), "thread"));
    let human = render::threads(&threads, true);
    let ids = threads.iter().map(|t| t.id.clone()).collect();
    Ok(Response::new(&threads, summary).human(human).ids(ids).crumbs(thread_crumbs()).meta("query", query))
}

pub fn thread(ctx: &Ctx, cmd: ThreadCommand) -> CliResult {
    let client = ctx.client()?;
    match cmd {
        ThreadCommand::Read { id, html, mark_read } => {
            let mut detail = client.thread(&id).map_err(missing("thread", &id, "list threads with `cloudmail inbox` or `cloudmail search <words>`"))?;
            if mark_read && detail.thread.unread {
                client.set_unread(&id, false)?;
                detail.thread.unread = false;
            }
            let t = &detail.thread;
            let from = t.sender().unwrap_or_default();
            let summary = render::clean(&format!("{} · {} · {}", t.subject, from, plural(detail.messages.len(), "message")));
            let mut crumbs = vec![
                crumb("reply", &format!("cloudmail reply {} -m <text>", output::shell_arg(&id)), "Reply to the latest message"),
                crumb("archive", &format!("cloudmail thread archive {}", output::shell_arg(&id)), "Archive this thread"),
            ];
            if detail.messages.iter().any(|m| m.attachments.iter().any(|a| !a.inline)) {
                crumbs.push(crumb("attachments", &format!("cloudmail attachment list {}", output::shell_arg(&id)), "List attachments"));
            }
            if t.folder == "screener"
                && let Some(f) = &t.from {
                    crumbs.insert(0, crumb("approve", &format!("cloudmail screener approve {}", output::shell_arg(&f.email)), "Screen this sender in"));
                    crumbs.insert(1, crumb("block", &format!("cloudmail screener block {}", output::shell_arg(&f.email)), "Screen this sender out"));
                }
            let ids = detail.messages.iter().map(|m| m.id.clone()).collect();
            let mut data = serde_json::to_value(&detail).unwrap_or_default();
            if !html {
                // Keep the envelope small for agents; --html includes the original markup.
                for m in data["messages"].as_array_mut().into_iter().flatten() {
                    if m["text"].as_str().is_none_or(|t| t.trim().is_empty())
                        && let Some(h) = m["html"].as_str() {
                            m["text"] = json!(html_to_text(h));
                        }
                    m["has_html"] = json!(!m["html"].is_null());
                    m.as_object_mut().map(|o| o.remove("html"));
                }
            }
            Ok(Response { data, ..Response::new((), summary) }.human(render::thread(&detail, html)).ids(ids).crumbs(crumbs))
        }
        ThreadCommand::Archive { ids } => each(&ids, "archived", |id| client.move_thread(id, "archive")),
        ThreadCommand::Unarchive { ids } => each(&ids, "moved to the Inbox", |id| client.move_thread(id, "inbox")),
        ThreadCommand::Unread { ids } => each(&ids, "marked unread", |id| client.set_unread(id, true)),
        ThreadCommand::Markread { ids } => each(&ids, "marked read", |id| client.set_unread(id, false)),
        ThreadCommand::Delete { ids, yes } => {
            confirm(yes, &format!("Permanently delete {}?", plural(ids.len(), "thread")))?;
            each(&ids, "deleted", |id| client.delete_thread(id))
        }
    }
}

/// Applies an action to several IDs, continuing past failures and reporting each.
fn each(ids: &[String], verb: &str, f: impl Fn(&str) -> cloudmail_api::Result<()>) -> CliResult {
    let mut done = Vec::new();
    let mut failed = Vec::new();
    for id in ids {
        match f(id) {
            Ok(()) => done.push(id.clone()),
            Err(e) => failed.push((id.clone(), e)),
        }
    }
    let summary = format!("{} {verb}", plural(done.len(), "thread"));
    let Some((_, first)) = failed.first() else {
        return Ok(Response::new(json!({ "done": done, "failed": [] }), summary).ids(done));
    };
    if done.is_empty() {
        return Err(first.clone().into());
    }
    // A partial failure exits with the first failure's code; the message says what did succeed.
    let mut err = CliError::from(first.clone());
    let failures = failed.iter().map(|(id, e)| format!("{id}: {e}")).collect::<Vec<_>>().join("; ");
    err.message = format!("{summary}, {} failed; {}", failed.len(), failures.replace('\n', "; "));
    err.hint = Some(format!("done: {}", done.join(", ")));
    Err(err)
}

pub fn confirm(yes: bool, question: &str) -> CliResult<()> {
    if yes {
        return Ok(());
    }
    if !(output::stdin_is_tty() && output::stdout_is_tty()) {
        return Err(CliError::new("confirmation_required", exit::USAGE, "refusing to do this without confirmation").hint("add --yes"));
    }
    if output::ask_yes(question)? { Ok(()) } else { Err(CliError::cancelled("cancelled")) }
}

pub fn screener(ctx: &Ctx, cmd: Option<ScreenerCommand>) -> CliResult {
    let client = ctx.client()?;
    match cmd.unwrap_or(ScreenerCommand::List) {
        ScreenerCommand::List => {
            let senders = client.screener()?;
            let summary = if senders.is_empty() {
                "The Screener is empty".to_string()
            } else {
                format!("{} waiting in the Screener", plural(senders.len(), "sender"))
            };
            let ids = senders.iter().map(|s| s.email.clone()).collect();
            Ok(Response::new(&senders, summary).human(render::screener(&senders)).ids(ids).crumbs(vec![
                crumb("approve", "cloudmail screener approve <email>", "Screen a sender in; their mail moves to the Inbox"),
                crumb("block", "cloudmail screener block <email>", "Screen a sender out"),
                crumb("threads", "cloudmail threads list --folder screener", "See the waiting threads"),
            ]))
        }
        ScreenerCommand::Approve { emails } => decide(client, &emails, "approved"),
        ScreenerCommand::Block { emails } => decide(client, &emails, "blocked"),
    }
}

fn decide(client: &cloudmail_api::Client, emails: &[String], status: &str) -> CliResult {
    // Check every address before deciding any, so a typo late in the list changes nothing.
    if let Some(bad) = emails.iter().find(|e| !bare_email(e).contains('@')) {
        return Err(CliError::usage(format!("not an email address: {bad}")));
    }
    let mut results = Vec::new();
    let mut done = Vec::new();
    let mut moved_total = 0;
    for e in emails {
        let email = bare_email(e);
        let moved = client.decide_sender(&email, status)?;
        moved_total += moved;
        results.push(json!({ "email": email, "status": status, "moved": moved }));
        done.push(email);
    }
    let verb = if status == "approved" { "Approved" } else { "Blocked" };
    let summary = format!("{verb} {}; {} moved", plural(done.len(), "sender"), plural(moved_total as usize, "thread"));
    let crumbs = if status == "approved" {
        vec![crumb("inbox", "cloudmail inbox", "See their mail in the Inbox")]
    } else {
        vec![crumb("undo", &format!("cloudmail screener approve {}", output::shell_arg(&done[0])), "Undo by approving")]
    };
    Ok(Response::new(results, summary).crumbs(crumbs).ids(done))
}

pub fn senders(ctx: &Ctx, status: SenderStatus) -> CliResult {
    let list = ctx.client()?.senders(status.as_str())?;
    let summary = format!("{} {}", plural(list.len(), "sender"), status.as_str());
    let ids = list.iter().map(|s| s.email.clone()).collect();
    Ok(Response::new(&list, summary).human(render::senders(&list)).ids(ids))
}

// ---------- sending ----------

fn read_stdin() -> CliResult<String> {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s)?;
    Ok(s)
}

fn edit(template: &str) -> CliResult<String> {
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
    // The draft quotes private mail: create it fresh (never through a planted link) and owner-only.
    let path = std::env::temp_dir().join(format!("cloudmail-{}.txt", std::process::id()));
    let mut file = std::fs::OpenOptions::new();
    file.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
    file.open(&path)?.write_all(template.as_bytes())?;
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&path)
        .status()?;
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    if !status.success() {
        return Err(CliError::cancelled("editor exited with an error; nothing sent"));
    }
    Ok(text)
}

/// Body from -m, --message-file, piped stdin, or $EDITOR on a terminal (in that order).
fn body(b: &BodyArgs, template: &str) -> CliResult<(String, bool)> {
    let text = match (&b.message, &b.message_file) {
        (Some(m), _) if m == "-" => read_stdin()?,
        (Some(m), _) => return Ok((m.clone(), false)),
        (None, Some(p)) if p.as_os_str() == "-" => read_stdin()?,
        (None, Some(p)) => std::fs::read_to_string(p).map_err(|e| CliError::generic(format!("could not read {}: {e}", p.display())))?,
        (None, None) if !output::stdin_is_tty() => read_stdin()?,
        (None, None) if output::stdout_is_tty() => return edit(template).map(|t| (t, true)),
        (None, None) => {
            return Err(CliError::usage("no message body").hint("pass -m <text>, --message-file <path>, or pipe the body on stdin"));
        }
    };
    Ok((text, false))
}

fn addresses(list: &[String]) -> Vec<String> {
    list.iter().flat_map(|s| split_addresses(s)).collect()
}

fn send_or_preview(ctx: &Ctx, req: SendRequest, dry_run: bool) -> CliResult {
    let to_text = req.to.join(", ");
    if dry_run {
        let summary = format!("Would send \"{}\" to {to_text} (dry run)", req.subject);
        let human = format!(
            "From: {}\nTo: {to_text}{}{}\nSubject: {}\n\n{}\n\n{summary}",
            req.from.as_deref().unwrap_or("(default mailbox)"),
            if req.cc.is_empty() { String::new() } else { format!("\nCc: {}", req.cc.join(", ")) },
            if req.bcc.is_empty() { String::new() } else { format!("\nBcc: {}", req.bcc.join(", ")) },
            req.subject,
            req.text.trim_end()
        );
        return Ok(Response::new(json!({ "dry_run": true, "request": req }), summary).human(human));
    }
    let resp = ctx.client()?.send(&req)?;
    let mut summary = format!("Sent \"{}\" to {to_text}", req.subject);
    if let Some(w) = &resp.warning {
        // The mail went out: retrying would send it twice.
        summary.push_str(&format!(" (warning: {w})"));
    }
    let mut crumbs = vec![crumb("sent", "cloudmail sent", "List sent threads")];
    let ids = resp.thread_id.clone().into_iter().collect::<Vec<_>>();
    if let Some(id) = &resp.thread_id {
        crumbs.insert(0, crumb("read", &format!("cloudmail thread read {}", output::shell_arg(id)), "See the conversation"));
    }
    Ok(Response::new(&resp, summary).ids(ids).crumbs(crumbs))
}

pub fn compose(ctx: &Ctx, a: &ComposeArgs) -> CliResult {
    let to = addresses(&a.to);
    if to.is_empty() {
        return Err(CliError::usage("--to needs at least one address"));
    }
    let (text, _) = body(&a.body, "")?;
    if text.trim().is_empty() {
        return Err(CliError::usage("the message body is empty; nothing sent"));
    }
    let req = SendRequest {
        from: a.from.clone(),
        to,
        cc: addresses(&a.cc),
        bcc: addresses(&a.bcc),
        subject: a.subject.clone(),
        text,
        reply_to_message_id: None,
    };
    send_or_preview(ctx, req, a.dry_run)
}

/// Builds a reply: recipients, From mailbox, subject and quoted text.
pub fn build_reply(detail: &ThreadDetail, own: &[String], default_from: Option<&str>, all: bool, from_override: Option<&str>) -> CliResult<(SendRequest, String)> {
    let latest = detail.reply_target().ok_or_else(|| CliError::not_found("the thread has no messages"))?;
    let is_own = |e: &str| own.iter().any(|o| o.eq_ignore_ascii_case(e));

    let to: Vec<String> = latest.reply_recipients().iter().map(|a| a.formatted()).collect();
    let cc: Vec<String> = if all {
        let mut seen: Vec<String> = to.iter().map(|t| bare_email(t)).collect();
        latest
            .to
            .iter()
            .chain(&latest.cc)
            .filter(|a| {
                let e = a.email.to_ascii_lowercase();
                if is_own(&e) || seen.contains(&e) {
                    return false;
                }
                seen.push(e);
                true
            })
            .map(|a| a.formatted())
            .collect()
    } else {
        Vec::new()
    };
    let to_address = detail.thread.to_address.as_deref().filter(|t| is_own(t));
    let from = from_override.or(to_address).or(default_from).map(str::to_string);
    let quoted = quote(&latest.plain_text(), &latest.from.display(), latest.date);
    let req = SendRequest {
        from,
        to,
        cc,
        bcc: Vec::new(),
        subject: reply_subject(&latest.subject),
        text: String::new(),
        reply_to_message_id: Some(latest.id.clone()),
    };
    Ok((req, quoted))
}

pub fn reply(ctx: &Ctx, a: &ReplyArgs) -> CliResult {
    let client = ctx.client()?;
    let detail = client.thread(&a.thread_id).map_err(missing("thread", &a.thread_id, "list threads with `cloudmail inbox`"))?;
    let ids = client.identities()?;
    let own: Vec<String> = ids.identities.iter().map(|i| i.email.to_ascii_lowercase()).collect();
    if let Some(f) = &a.from
        && !own.contains(&bare_email(f)) {
            return Err(CliError::usage(format!("{f} is not one of your mailboxes")).hint("see `cloudmail mailbox list`"));
        }
    let default_from = ids.default.as_ref().map(|d| d.email.as_str());
    let (mut req, quoted) = build_reply(&detail, &own, default_from, a.all, a.from.as_deref())?;
    let template = if a.no_quote { String::new() } else { format!("\n{quoted}") };
    let (text, edited) = body(&a.body, &template)?;
    if text.trim().is_empty() || (edited && text.trim() == template.trim()) {
        return Err(CliError::usage("the reply is empty; nothing sent"));
    }
    req.text = if edited || a.no_quote { text } else { format!("{}{quoted}", text.trim_end()) };
    send_or_preview(ctx, req, a.dry_run)
}

// ---------- files ----------

pub fn attachment(ctx: &Ctx, cmd: AttachmentCommand) -> CliResult {
    let client = ctx.client()?;
    match cmd {
        AttachmentCommand::List { thread_id } => {
            let detail = client.thread(&thread_id).map_err(missing("thread", &thread_id, "list threads with `cloudmail inbox`"))?;
            let atts: Vec<_> = detail.messages.iter().flat_map(|m| m.attachments.iter().filter(|a| !a.inline).map(move |a| (m, a))).collect();
            let human = atts
                .iter()
                .map(|(_, a)| render::clean_line(&format!("{}  {}  {}  {}", a.id, a.filename, a.mime_type, human_size(a.size))))
                .collect::<Vec<_>>()
                .join("\n");
            let ids = atts.iter().map(|(_, a)| a.id.clone()).collect::<Vec<_>>();
            let summary = render::clean(&format!("{} in {}", plural(ids.len(), "attachment"), detail.thread.subject));
            let data: Vec<_> = atts.iter().map(|(m, a)| json!({ "message_id": m.id, "attachment": a })).collect();
            Ok(Response::new(data, summary)
                .human(human)
                .ids(ids)
                .crumbs(vec![crumb("save", "cloudmail attachment save <attachment-id> -o <path>", "Download an attachment")]))
        }
        AttachmentCommand::Save { id, output } => {
            let dl = client.download_attachment(&id).map_err(missing("attachment", &id, "list them with `cloudmail attachment list <thread-id>`"))?;
            let name = safe_attachment_name(dl.filename.as_deref(), &id);
            let size = dl.bytes.len();
            if output.as_deref().is_some_and(|p| p.as_os_str() == "-") {
                std::io::stdout().write_all(&dl.bytes)?;
                return Ok(Response::silent());
            }
            // A name chosen by the sender never replaces an existing file; an explicit -o path may.
            let path = match output {
                Some(p) if !is_dir_target(&p) => p,
                dir => unused_path(target_path(dir, &name)),
            };
            std::fs::write(&path, &dl.bytes).map_err(|e| CliError::generic(format!("could not write {}: {e}", path.display())))?;
            let summary = format!("Saved {} ({})", path.display(), human_size(size as i64));
            Ok(Response::new(json!({ "id": id, "path": path, "size": size, "content_type": dl.content_type }), summary))
        }
    }
}

/// The sender's filename, reduced to a plain, visible file name.
fn safe_attachment_name(filename: Option<&str>, id: &str) -> String {
    let base = filename
        .and_then(|n| Path::new(n).file_name())
        .map(|n| n.to_string_lossy().chars().map(|c| if c.is_control() { '_' } else { c }).collect::<String>())
        .map(|n| n.trim_start_matches('.').trim().to_string())
        .unwrap_or_default();
    if base.is_empty() { format!("{id}.bin") } else { base }
}

/// `-o` names a directory to save into rather than a file.
fn is_dir_target(p: &Path) -> bool {
    p.is_dir() || p.to_string_lossy().ends_with('/')
}

fn target_path(output: Option<PathBuf>, name: &str) -> PathBuf {
    match output {
        Some(p) if is_dir_target(&p) => p.join(name),
        Some(p) => p,
        None => PathBuf::from(name),
    }
}

pub fn raw(ctx: &Ctx, a: &RawArgs) -> CliResult {
    let bytes = ctx.client()?.raw_message(&a.id).map_err(missing("raw message", &a.id, "message IDs (m_…) are in `cloudmail thread read <id> --json`; sent messages have no raw copy"))?;
    match &a.output {
        Some(p) if p.as_os_str() != "-" => {
            let path = target_path(Some(p.clone()), &format!("{}.eml", a.id));
            std::fs::write(&path, &bytes)?;
            Ok(Response::new(json!({ "id": a.id, "path": path, "size": bytes.len() }), format!("Saved {}", path.display())))
        }
        // Without -o, raw bytes go to a terminal only when asked for explicitly with `-o -`.
        None if output::stdout_is_tty() => Err(CliError::usage(
            "raw mail is exactly what the sender wrote, escape sequences included, so it isn't printed to a terminal",
        )
        .hint(format!("redirect it (`cloudmail raw {id} > message.eml`), save it with `-o message.eml`, or force it with `-o -`", id = a.id))),
        _ => {
            std::io::stdout().write_all(&bytes)?;
            Ok(Response::silent())
        }
    }
}

// ---------- watch ----------

pub fn watch(ctx: &Ctx, a: &WatchArgs) -> CliResult {
    let client = ctx.client()?;
    let folder = a.folder.as_str();
    let fetch = |since: Option<i64>, before: Option<i64>, limit: u32| {
        client.list_threads(&ThreadQuery { folder: folder.into(), since, before, limit, ..Default::default() })
    };
    // Everything that changed after `since`, paging back so a burst larger than one page isn't lost.
    let changes_since = |since: i64| -> cloudmail_api::Result<Vec<ThreadSummary>> {
        const PAGE: u32 = 100;
        let mut all: Vec<ThreadSummary> = Vec::new();
        let mut before: Option<i64> = None;
        loop {
            let page = fetch(Some(since), before, PAGE)?;
            let full = page.len() == PAGE as usize;
            // `before` is exclusive, so step back to just past the oldest one seen; ids dedupe the overlap.
            let next = page.iter().map(|t| t.last_at).min().map(|oldest| oldest + 1);
            for t in page {
                if !all.iter().any(|x| x.id == t.id) {
                    all.push(t);
                }
            }
            match next {
                Some(n) if full && before.is_none_or(|b| n < b) => before = Some(n),
                _ => return Ok(all),
            }
        }
    };
    // Start from the newest activity the worker knows about, so clock skew can't drop or repeat mail.
    let mut since = match a.since {
        Some(s) => s,
        None => fetch(None, None, 1)?.first().map(|t| t.last_at).unwrap_or(0),
    };
    let machine = ctx.mode.is_machine();
    if !machine {
        eprintln!("Watching {folder} every {}s (Ctrl+C to stop)…", a.interval);
    }
    let mut polls = 0u64;
    let mut failures = 0u32;
    loop {
        match changes_since(since) {
            Ok(mut threads) => {
                failures = 0;
                threads.sort_by_key(|t| t.last_at);
                for t in &threads {
                    emit(t, machine);
                    since = since.max(t.last_at);
                }
            }
            Err(e) if matches!(e.kind, ErrorKind::Unauthorized | ErrorKind::Config) => return Err(e.into()),
            Err(e) => {
                failures += 1;
                if machine {
                    println!("{}", output::json_safe(&json!({ "event": "error", "code": e.kind.code(), "message": e.message }).to_string()));
                } else {
                    eprintln!("{}", output::terminal_safe(&format!("warning: {e} (retrying)")));
                }
            }
        }
        polls += 1;
        if a.max_polls.is_some_and(|m| polls >= m) {
            return Ok(Response::silent());
        }
        let backoff = a.interval.max(1) * u64::from(failures.clamp(1, 6));
        std::thread::sleep(std::time::Duration::from_secs(backoff));
    }
}

fn emit(t: &ThreadSummary, machine: bool) {
    let line = if machine {
        output::json_safe(&json!({ "event": "thread", "thread": t }).to_string())
    } else {
        let from = t.sender().unwrap_or_default();
        let line = format!("{}  {:<8}  {}  {}  ", short_time(t.last_at), t.folder, render::truncate(&from, 24), t.subject);
        format!("{}{}", render::clean_line(&line), output::dim(&render::clean(&t.id)))
    };
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
        std::process::exit(exit::OK); // reader went away
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloudmail_api::{Address, Message, MessageAuth};

    #[test]
    fn attachment_names_are_plain_and_never_clobber() {
        assert_eq!(safe_attachment_name(Some(".bashrc"), "a1"), "bashrc");
        assert_eq!(safe_attachment_name(Some("../../etc/passwd"), "a1"), "passwd");
        assert_eq!(safe_attachment_name(Some(".."), "a1"), "a1.bin");
        assert_eq!(safe_attachment_name(None, "a1"), "a1.bin");
        assert_eq!(safe_attachment_name(Some("x\x1b]0;T\x07.pdf"), "a1"), "x_]0;T_.pdf");
        let dir = std::env::temp_dir().join(format!("cm-att-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("invoice.pdf"), "old").unwrap();
        assert_eq!(unused_path(dir.join("invoice.pdf")), dir.join("invoice (1).pdf"));
        assert_eq!(unused_path(dir.join("new.pdf")), dir.join("new.pdf"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn addr(name: &str, email: &str) -> Address {
        Address { name: Some(name.into()), email: email.into() }
    }

    fn detail() -> ThreadDetail {
        ThreadDetail {
            thread: ThreadSummary { id: "t_1".into(), to_address: Some("support@example.org".into()), ..Default::default() },
            messages: vec![
                Message {
                    id: "m_1".into(),
                    from: addr("Joe", "joe@x.com"),
                    to: vec![addr("", "support@example.org"), addr("Ops", "ops@x.com")],
                    cc: vec![addr("", "hi@example.com"), addr("Ann", "ann@x.com")],
                    subject: "Order".into(),
                    text: Some("Where is it?".into()),
                    auth: Some(MessageAuth { dmarc: Some("pass".into()), ..Default::default() }),
                    ..Default::default()
                },
                Message { id: "m_2".into(), outgoing: true, from: addr("", "support@example.org"), ..Default::default() },
            ],
        }
    }

    #[test]
    fn reply_targets_latest_incoming_and_right_mailbox() {
        let own = vec!["hi@example.com".to_string(), "support@example.org".to_string()];
        let (req, quoted) = build_reply(&detail(), &own, Some("hi@example.com"), false, None).unwrap();
        assert_eq!(req.from.as_deref(), Some("support@example.org"));
        assert_eq!(req.to, vec!["Joe <joe@x.com>"]);
        assert!(req.cc.is_empty());
        assert_eq!(req.subject, "Re: Order");
        assert_eq!(req.reply_to_message_id.as_deref(), Some("m_1"));
        assert!(quoted.contains("> Where is it?"));
    }

    #[test]
    fn reply_all_excludes_own_addresses() {
        let own = vec!["hi@example.com".to_string(), "support@example.org".to_string()];
        let (req, _) = build_reply(&detail(), &own, None, true, None).unwrap();
        assert_eq!(req.cc, vec!["Ops <ops@x.com>", "Ann <ann@x.com>"]);
    }

    #[test]
    fn reply_honours_reply_to_and_from_override() {
        let mut d = detail();
        d.messages[0].reply_to = vec![addr("", "replies@x.com")];
        let own = vec!["hi@example.com".to_string(), "support@example.org".to_string()];
        let (req, _) = build_reply(&d, &own, None, false, Some("hi@example.com")).unwrap();
        assert_eq!(req.to, vec!["replies@x.com"]);
        assert_eq!(req.from.as_deref(), Some("hi@example.com"));
    }

    #[test]
    fn target_paths() {
        assert_eq!(target_path(None, "a.pdf"), PathBuf::from("a.pdf"));
        assert_eq!(target_path(Some(PathBuf::from("out/")), "a.pdf"), PathBuf::from("out/a.pdf"));
        assert_eq!(target_path(Some(PathBuf::from("x.pdf")), "a.pdf"), PathBuf::from("x.pdf"));
    }
}
