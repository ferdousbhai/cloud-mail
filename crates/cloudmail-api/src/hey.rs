//! HEY (hey.com) through the official `hey` CLI: every call is `hey … --json`, mapped into
//! cloudmail's types. The CLI owns the login (`hey auth login`, one browser OAuth); cloudmail
//! never sees a token.
//!
//! IDs: a thread is `hey:<topic_id>:<box_item_id>` (HEY reads and replies by topic, but moves and
//! marks seen by box item; a search hit outside any box is just `hey:<topic_id>`). A message is
//! `hey:<topic_id>/<entry_id>`, an attachment `hey:<attachment id>`, a Screener sender
//! `hey:<clearance_id>`.
//!
//! Folders: Imbox is the Inbox, Paper Trail stands in for the Archive (HEY has none), The Feed,
//! Set Aside and Reply Later are extra folders. HEY's CLI has no Sent box and no blocked list.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use crate::client::ThreadQuery;
use crate::config::AccountConfig;
use crate::error::{Error, ErrorKind, Result};
use crate::provider::{AccountStatus, ExtraFolder, PrivateDir, Provider, Run, run_command};
use crate::text::bare_email;
use crate::types::*;

pub const COMMAND_ENV: &str = "CLOUDMAIL_HEY_COMMAND";
const TIMEOUT: Duration = Duration::from_secs(90);
/// How long a browser sign-in may take.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);
/// How many threads one box listing asks for. HEY pages by cursor, not by time, so paging back
/// past this many in a merged view isn't possible.
const BOX_LIMIT: u32 = 100;

pub const EXTRA_FOLDERS: &[ExtraFolder] = &[
    ExtraFolder { folder: "feed", title: "The Feed" },
    ExtraFolder { folder: "paper_trail", title: "Paper Trail" },
    ExtraFolder { folder: "set_aside", title: "Set Aside" },
    ExtraFolder { folder: "reply_later", title: "Reply Later" },
];

/// The HEY box (by kind) a cloudmail folder reads from or moves to.
pub fn box_for(folder: &str) -> Option<&'static str> {
    Some(match folder {
        "inbox" | "imbox" => "imbox",
        "archive" | "paper_trail" => "trailbox",
        "feed" => "feedbox",
        "set_aside" => "asidebox",
        "reply_later" => "laterbox",
        _ => return None,
    })
}

/// The cloudmail folder name for a HEY box kind.
pub fn folder_for(kind: &str) -> String {
    match kind {
        "imbox" => "inbox",
        "trailbox" => "paper_trail",
        "feedbox" => "feed",
        "asidebox" => "set_aside",
        "laterbox" => "reply_later",
        "bubblebox" => "bubble_up",
        other => other,
    }
    .to_string()
}

pub struct Hey {
    name: String,
    command: String,
    account: Option<String>,
    /// Listings seen so far, by topic: `thread read` has no subject or box of its own.
    seen: Mutex<HashMap<i64, ThreadSummary>>,
    own: Mutex<Option<Vec<Address>>>,
}

fn millis(v: &Value) -> i64 {
    let Some(s) = v.as_str() else { return 0 };
    // Listings carry RFC 3339; `thread read` gives UTC to the minute ("2026-05-30T14:51").
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.timestamp_millis())
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M").map(|d| d.and_utc().timestamp_millis()))
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").map(|d| d.and_utc().timestamp_millis()))
        .unwrap_or(0)
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().trim().to_string()
}

fn nonempty(s: String) -> Option<String> {
    Some(s).filter(|s| !s.is_empty())
}

fn contact(v: &Value) -> Address {
    Address { name: nonempty(text(&v["name"])), email: text(&v["email_address"]) }
}

fn contacts(v: &Value) -> Vec<Address> {
    v.as_array().into_iter().flatten().map(contact).filter(|a| !a.email.is_empty()).collect()
}

/// `(topic, box item)` from `hey:<topic>[:<box item>][/<entry>]`.
fn parse_id(local: &str) -> Option<(i64, Option<i64>)> {
    let local = local.split('/').next()?;
    let mut parts = local.split(':');
    let topic = parts.next()?.parse().ok()?;
    let item = parts.next().and_then(|p| p.parse().ok());
    Some((topic, item))
}

/// Plain text as the HTML HEY sends: escaped, line breaks kept.
fn text_html(text: &str) -> String {
    let escaped = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!("<div>{}</div>", escaped.trim_end().replace('\n', "<br>"))
}

impl Hey {
    pub fn new(name: &str, cfg: &AccountConfig) -> Self {
        let command = std::env::var(COMMAND_ENV)
            .ok()
            .filter(|c| !c.trim().is_empty())
            .or_else(|| cfg.command.clone().filter(|c| !c.trim().is_empty()))
            .unwrap_or_else(|| "hey".into());
        Self { name: name.to_string(), command, account: cfg.account.clone(), seen: Default::default(), own: Default::default() }
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    /// The installed CLI's version line, or an error when it isn't installed.
    pub fn version(&self) -> Result<String> {
        let out = self.exec(&["version".to_string()], None, false)?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    /// Whether the CLI is signed in (`hey auth status`).
    pub fn signed_in(&self) -> Result<bool> {
        match self.run(&["auth", "status"], None) {
            Ok(data) => Ok(data["authenticated"] == json!(true)),
            Err(e) if e.kind == ErrorKind::AccountAuth => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Runs `hey auth login`, its output on the terminal: HEY's own browser sign-in (OAuth with
    /// PKCE). Gives up after `LOGIN_TIMEOUT`, so a sign-in abandoned in the browser can't hang the app.
    pub fn login(&self) -> Result<()> {
        let mut child = Command::new(&self.command)
            .args(["auth", "login"])
            .stdin(std::process::Stdio::null())
            .spawn()
            .map_err(|e| self.fail(ErrorKind::AccountUnavailable, format!("could not run {}: {e}", self.command)))?;
        let deadline = std::time::Instant::now() + LOGIN_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(self.fail(ErrorKind::AccountAuth, "the HEY sign-in wasn't finished within 10 minutes"));
                }
                Err(e) => return Err(self.fail(ErrorKind::AccountUnavailable, e.to_string())),
            }
        };
        if status.success() { Ok(()) } else { Err(self.fail(ErrorKind::AccountAuth, format!("`hey auth login` didn't finish ({status})"))) }
    }

    fn local<'a>(&self, id: &'a str) -> Result<&'a str> {
        id.strip_prefix(self.name.as_str())
            .and_then(|r| r.strip_prefix(':'))
            .ok_or_else(|| Error::new(ErrorKind::BadRequest, format!("{id} is not a {} ID", self.label())))
    }

    fn ids(&self, id: &str) -> Result<(i64, Option<i64>)> {
        parse_id(self.local(id)?).ok_or_else(|| Error::new(ErrorKind::NotFound, format!("no HEY thread {id}")))
    }

    fn fail(&self, kind: ErrorKind, message: impl AsRef<str>) -> Error {
        Error::new(kind, format!("{}: {}", self.label(), message.as_ref()))
    }

    /// Runs `hey <args>` and returns its stdout, mapping the exit status and JSON error envelope.
    fn exec(&self, args: &[String], stdin: Option<&str>, json: bool) -> Result<Vec<u8>> {
        let mut cmd = Command::new(&self.command);
        cmd.args(args);
        if json {
            cmd.arg("--json");
        }
        if let Some(a) = &self.account {
            cmd.args(["--account", a]);
        }
        cmd.env("HEY_NONINTERACTIVE", "1");
        let (status, stdout, stderr) = match run_command(&mut cmd, stdin, TIMEOUT) {
            Run::Done { status, stdout, stderr } => (status, stdout, stderr),
            Run::Missing => {
                return Err(self.fail(ErrorKind::AccountUnavailable, format!("the hey CLI isn't installed (no `{}` on PATH); see https://github.com/basecamp/hey-cli", self.command)));
            }
            Run::TimedOut => {
                return Err(self.fail(ErrorKind::AccountUnavailable, format!("`hey {}` took longer than {}s", args.first().map(String::as_str).unwrap_or(""), TIMEOUT.as_secs())));
            }
            Run::Failed(e) => return Err(self.fail(ErrorKind::AccountUnavailable, format!("could not run {}: {e}", self.command))),
        };
        if status.success() {
            return Ok(stdout);
        }
        // hey's exit statuses: 1 usage/conflict, 2 not found, 3 auth, 4 forbidden, 5 rate
        // limited, 6 network, 7 API/server, 8 ambiguous.
        let envelope: Value = serde_json::from_slice(&stdout).unwrap_or(Value::Null);
        let mut message = text(&envelope["error"]);
        if message.is_empty() {
            message = stderr.lines().last().unwrap_or("").to_string();
        }
        if message.is_empty() {
            message = format!("`hey {}` failed ({status})", args.first().map(String::as_str).unwrap_or(""));
        }
        let hint = text(&envelope["hint"]);
        let kind = match status.code() {
            Some(2) => ErrorKind::NotFound,
            Some(3) => {
                return Err(self.fail(ErrorKind::AccountAuth, format!("not signed in ({message}); run `cloudmail account login {}`", self.name)));
            }
            Some(1 | 4 | 8) => ErrorKind::BadRequest,
            _ => ErrorKind::AccountUnavailable,
        };
        Err(self.fail(kind, if hint.is_empty() { message } else { format!("{message} ({hint})") }))
    }

    /// `hey <args> --json`, returning the envelope's `data`.
    fn run(&self, args: &[&str], stdin: Option<&str>) -> Result<Value> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let stdout = self.exec(&args, stdin, true)?;
        let envelope: Value = serde_json::from_slice(&stdout)
            .map_err(|e| self.fail(ErrorKind::AccountUnavailable, format!("`hey {}` answered something that isn't JSON ({e})", args.first().map(String::as_str).unwrap_or(""))))?;
        if envelope["ok"] != json!(true) {
            return Err(self.fail(ErrorKind::AccountUnavailable, format!("`hey {}` failed: {}", args[0], text(&envelope["error"]))));
        }
        Ok(envelope["data"].clone())
    }

    fn shape_error(&self, what: &str) -> Error {
        self.fail(ErrorKind::AccountUnavailable, format!("unexpected `hey {what}` output (a newer hey CLI?)"))
    }

    fn summary_from_posting(&self, p: &Value, fallback_kind: &str) -> Option<ThreadSummary> {
        // A bundle row stands for one sender's unseen threads and has no topic of its own.
        let topic = p["topic_id"].as_i64()?;
        let item = p["id"].as_i64();
        let is_user = |c: &Value| c["contactable_type"] == "User";
        let creator = &p["creator"];
        let from = if is_user(creator) {
            p["contacts"].as_array().into_iter().flatten().find(|c| !is_user(c)).map(contact).unwrap_or_else(|| contact(creator))
        } else {
            let mut a = contact(creator);
            if let Some(alt) = nonempty(text(&p["alternative_sender_name"])) {
                a.name = Some(alt);
            }
            a
        };
        let to_address = p["addressed_contacts"].as_array().into_iter().flatten().find(|c| is_user(c)).map(|c| text(&c["email_address"])).and_then(nonempty);
        let kind = p["box"]["kind"].as_str().unwrap_or(fallback_kind);
        Some(ThreadSummary {
            id: match item {
                Some(item) => format!("{}:{topic}:{item}", self.name),
                None => format!("{}:{topic}", self.name),
            },
            subject: text(&p["name"]),
            folder: folder_for(kind),
            snippet: text(&p["summary"]),
            from: Some(from),
            to_address,
            message_count: p["visible_entry_count"].as_i64().unwrap_or(1),
            unread: p["seen"] == json!(false),
            has_attachments: p["includes_attachments"] == json!(true),
            // active_at is the latest message; updated_at also moves when it is merely read.
            last_at: Some(millis(&p["active_at"])).filter(|t| *t > 0).unwrap_or_else(|| millis(&p["created_at"])),
            account: Some(self.name.clone()),
        })
    }

    fn remember(&self, list: &[ThreadSummary]) {
        let mut seen = self.seen.lock().unwrap();
        for t in list {
            if let Some((topic, _)) = self.local(&t.id).ok().and_then(parse_id) {
                seen.insert(topic, t.clone());
            }
        }
    }

    fn box_threads(&self, kind: &str, limit: u32) -> Result<Vec<ThreadSummary>> {
        let limit = limit.max(1).to_string();
        let data = self.run(&["box", "view", kind, "--limit", &limit], None)?;
        let postings = data["postings"].as_array().ok_or_else(|| self.shape_error("box view"))?;
        let box_kind = data["kind"].as_str().unwrap_or(kind).to_string();
        Ok(postings.iter().filter_map(|p| self.summary_from_posting(p, &box_kind)).collect())
    }

    fn clearances(&self) -> Result<Vec<Value>> {
        let data = self.run(&["screener", "list", "--all"], None)?;
        data.as_array().cloned().ok_or_else(|| self.shape_error("screener list"))
    }

    fn own_addresses(&self) -> Vec<Address> {
        if let Some(own) = self.own.lock().unwrap().clone() {
            return own;
        }
        let Ok(data) = self.run(&["account", "senders"], None) else { return Vec::new() };
        let own: Vec<Address> = data
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| Address { name: nonempty(text(&s["name"])), email: text(&s["email"]) })
            .filter(|a| !a.email.is_empty())
            .collect();
        *self.own.lock().unwrap() = Some(own.clone());
        own
    }

    /// The subject HEY would reply with: `hey reply --dry-run` is read-only and reads no body.
    fn subject_of(&self, topic: i64) -> Option<String> {
        let data = self.run(&["reply", &topic.to_string(), "--dry-run"], None).ok()?;
        nonempty(text(&data["subject"])).map(|s| s.trim_start_matches("Re: ").to_string())
    }

    /// HEY's original HTML per entry, from `thread read --html` (one `<article>` per entry).
    fn entry_html(&self, topic: i64) -> HashMap<i64, String> {
        let args = vec!["thread".to_string(), "read".to_string(), topic.to_string(), "--html".to_string()];
        let Ok(bytes) = self.exec(&args, None, false) else { return HashMap::new() };
        split_articles(&String::from_utf8_lossy(&bytes))
    }

    fn attachments(&self, topic: i64) -> Vec<(i64, Attachment)> {
        let Ok(data) = self.run(&["attachment", "list", &topic.to_string()], None) else { return Vec::new() };
        data.as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| {
                let id = text(&a["id"]);
                let token = id.split_once(':').map(|(_, t)| t).unwrap_or("");
                (!id.is_empty()).then(|| {
                    (
                        a["message_id"].as_i64().unwrap_or(0),
                        Attachment {
                            id: format!("{}:{id}", self.name),
                            filename: text(&a["filename"]),
                            mime_type: text(&a["content_type"]),
                            size: a["byte_size"].as_i64().unwrap_or(0),
                            // Named images embedded in the body carry "e-…" tokens; files are numbered.
                            inline: token.starts_with("e-"),
                        },
                    )
                })
            })
            .collect()
    }
}

/// Splits `hey thread read --html` into each entry's body HTML.
fn split_articles(doc: &str) -> HashMap<i64, String> {
    let mut out = HashMap::new();
    for part in doc.split("<article id=\"entry-").skip(1) {
        let Some((id, rest)) = part.split_once('"') else { continue };
        let Ok(id) = id.parse::<i64>() else { continue };
        let Some((_, body)) = rest.split_once("</header>\n") else { continue };
        let body = body.rfind("</article>").map(|i| &body[..i]).unwrap_or(body);
        out.insert(id, unwrap_trix(body.trim()));
    }
    out
}

/// HEY keeps a received HTML email as a Trix attachment: the email sits in the figure's JSON
/// attribute, inside `<shadow-content><template>`, which a browser never renders. Puts each such
/// email back in place of its figure; other attachments (images) stay as they are.
fn unwrap_trix(body: &str) -> String {
    const OPEN: &str = "<figure data-trix-attachment=\"";
    const CLOSE: &str = "</figure>";
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(i) = rest.find(OPEN) {
        let after = &rest[i + OPEN.len()..];
        let Some(q) = after.find('"') else { break };
        let Some(end) = after[q..].find(CLOSE).map(|e| q + e) else { break };
        let attr = after[..q].replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&#39;", "'").replace("&amp;", "&");
        let email = serde_json::from_str::<Value>(&attr)
            .ok()
            .filter(|j| j["contentType"] == "text/html")
            .and_then(|j| j["content"].as_str().map(str::to_string));
        out.push_str(&rest[..i]);
        match email {
            Some(html) => out.push_str(
                html.trim().trim_start_matches("<shadow-content><template>").trim_end_matches("</template></shadow-content>"),
            ),
            None => out.push_str(&rest[i..i + OPEN.len() + end + CLOSE.len()]),
        }
        rest = &after[end + CLOSE.len()..];
    }
    out.push_str(rest);
    out
}

impl Provider for Hey {
    fn name(&self) -> &str {
        &self.name
    }

    fn label(&self) -> &str {
        "HEY"
    }

    fn extra_folders(&self) -> &[ExtraFolder] {
        EXTRA_FOLDERS
    }

    fn archive_folder(&self) -> &str {
        "paper_trail"
    }

    fn has_folder(&self, folder: &str) -> bool {
        box_for(folder).is_some() || matches!(folder, "screener" | "all")
    }

    fn sign_in(&self) -> Result<()> {
        self.login()
    }

    fn status(&self) -> AccountStatus {
        let mut status = AccountStatus { name: self.name.clone(), provider: "hey".into(), label: "HEY".into(), ..Default::default() };
        match self.run(&["auth", "status"], None) {
            Ok(data) if data["authenticated"] == json!(true) => {
                status.ok = true;
                status.addresses = self.own_addresses().into_iter().map(|a| a.email).collect();
                status.detail = format!("signed in via {}", self.command);
            }
            Ok(_) => status.detail = format!("not signed in: run `cloudmail account login {}`", self.name),
            Err(e) => status.detail = e.message,
        }
        status
    }

    fn threads(&self, q: &ThreadQuery) -> Result<Vec<ThreadSummary>> {
        let folder = if q.folder.is_empty() { "inbox" } else { q.folder.as_str() };
        // No time cursor in HEY: ask for enough to cover this page (or the most a box gives) and filter here.
        let want = if q.before.is_some() { BOX_LIMIT } else { q.limit.clamp(1, BOX_LIMIT) };
        let mut list = match folder {
            "screener" => self.screener_threads()?,
            "all" => {
                let boxes = ["imbox", "feedbox", "trailbox", "asidebox", "laterbox"];
                let results: Vec<Result<Vec<ThreadSummary>>> =
                    std::thread::scope(|s| boxes.map(|b| s.spawn(move || self.box_threads(b, want))).into_iter().map(|h| h.join().unwrap_or_else(|_| Err(self.shape_error("box view")))).collect());
                let mut all = Vec::new();
                for r in results {
                    all.extend(r?);
                }
                all
            }
            f => match box_for(f) {
                Some(kind) => self.box_threads(kind, want)?,
                None => Vec::new(),
            },
        };
        self.remember(&list);
        list.retain(|t| q.before.is_none_or(|b| t.last_at < b) && q.since.is_none_or(|s| t.last_at > s) && (!q.unread || t.unread));
        list.sort_by_key(|t| std::cmp::Reverse(t.last_at));
        list.truncate(q.limit.max(1) as usize);
        Ok(list)
    }

    fn search(&self, query: &str, limit: u32) -> Result<Vec<ThreadSummary>> {
        let data = self.run(&["search", query], None)?;
        let hits = data.as_array().ok_or_else(|| self.shape_error("search"))?;
        let known = self.seen.lock().unwrap().clone();
        let mut list: Vec<ThreadSummary> = hits
            .iter()
            .filter_map(|h| {
                let topic = h["topic_id"].as_i64()?;
                if let Some(t) = known.get(&topic) {
                    return Some(t.clone());
                }
                let latest = h["messages"].as_array().and_then(|m| m.last()).cloned().unwrap_or(Value::Null);
                let mut from = contact(&latest["creator"]);
                if let Some(alt) = nonempty(text(&latest["alternative_sender_name"])) {
                    from.name = Some(alt);
                }
                Some(ThreadSummary {
                    id: match h["id"].as_i64() {
                        Some(item) => format!("{}:{topic}:{item}", self.name),
                        None => format!("{}:{topic}", self.name),
                    },
                    subject: text(&h["subject"]),
                    folder: String::new(),
                    snippet: text(&latest["summary"]),
                    from: Some(from).filter(|a| !a.email.is_empty()),
                    to_address: None,
                    message_count: h["messages"].as_array().map(|m| m.len() as i64).unwrap_or(1).max(1),
                    unread: false,
                    has_attachments: false,
                    last_at: millis(&h["updated_at"]).max(millis(&latest["created_at"])),
                    account: Some(self.name.clone()),
                })
            })
            .collect();
        list.truncate(limit.max(1) as usize);
        Ok(list)
    }

    fn thread(&self, id: &str, html: bool) -> Result<ThreadDetail> {
        let (topic, item) = self.ids(id)?;
        let known = self.seen.lock().unwrap().get(&topic).cloned();
        let (entries, bodies, atts, own, subject) = std::thread::scope(|s| {
            let bodies = s.spawn(|| if html { self.entry_html(topic) } else { HashMap::new() });
            let atts = s.spawn(|| self.attachments(topic));
            let own = s.spawn(|| self.own_addresses());
            let subject = s.spawn(|| if known.is_some() { None } else { self.subject_of(topic) });
            let entries = self.run(&["thread", "read", &topic.to_string()], None);
            (entries, bodies.join().unwrap_or_default(), atts.join().unwrap_or_default(), own.join().unwrap_or_default(), subject.join().unwrap_or_default())
        });
        let entries = entries?;
        let entries = entries.as_array().ok_or_else(|| self.shape_error("thread read"))?;
        let own: Vec<String> = own.iter().map(|a| a.email.to_ascii_lowercase()).collect();
        let mut summary = known.unwrap_or_else(|| ThreadSummary {
            id: id.to_string(),
            subject: subject.unwrap_or_default(),
            account: Some(self.name.clone()),
            ..Default::default()
        });
        if item.is_some() {
            summary.id = id.to_string();
        }
        let thread_id = summary.id.clone();
        let mut messages: Vec<Message> = entries
            .iter()
            .map(|e| {
                let entry = e["id"].as_i64().unwrap_or(0);
                let creator = contact(&e["creator"]);
                let sender = e.get("sender").map(contact).filter(|a| !a.email.is_empty());
                let outgoing = own.contains(&creator.email.to_ascii_lowercase()) || sender.as_ref().is_some_and(|s| own.contains(&s.email.to_ascii_lowercase()));
                let mut from = sender.unwrap_or(creator);
                if !outgoing && let Some(alt) = nonempty(text(&e["alternative_sender_name"])) {
                    from.name = Some(alt);
                }
                let html = bodies.get(&entry).cloned().filter(|h| !h.is_empty());
                // With the HTML at hand, the text is its plain reading; otherwise HEY's Markdown.
                let body = html
                    .as_deref()
                    .map(crate::text::html_to_text)
                    .or_else(|| e["body"].as_str().map(str::to_string))
                    .filter(|b| !b.trim().is_empty())
                    .or_else(|| nonempty(text(&e["summary"])));
                Message {
                    id: format!("{}:{topic}/{entry}", self.name),
                    thread_id: thread_id.clone(),
                    outgoing,
                    from,
                    to: contacts(&e["recipients"]["to"]),
                    cc: contacts(&e["recipients"]["cc"]),
                    reply_to: Vec::new(),
                    subject: summary.subject.clone(),
                    date: millis(&e["created_at"]),
                    text: body,
                    html,
                    message_id: None,
                    attachments: Vec::new(),
                    auth: None,
                }
            })
            .collect();
        for (message_id, a) in atts {
            let target = messages.iter().position(|m| m.id.ends_with(&format!("/{message_id}"))).or(messages.len().checked_sub(1));
            if let Some(i) = target {
                messages[i].attachments.push(a);
            }
        }
        if let Some(latest) = messages.iter().rev().find(|m| !m.outgoing).or(messages.last()) {
            if summary.from.is_none() {
                summary.from = Some(latest.from.clone());
            }
            if summary.last_at == 0 {
                summary.last_at = latest.date;
            }
        }
        if summary.to_address.is_none() {
            summary.to_address = own.first().cloned();
        }
        summary.message_count = messages.len() as i64;
        summary.has_attachments = messages.iter().any(|m| m.attachments.iter().any(|a| !a.inline));
        Ok(ThreadDetail { thread: summary, messages })
    }

    fn move_thread(&self, id: &str, folder: &str) -> Result<()> {
        let (_, item) = self.ids(id)?;
        let item = item.ok_or_else(|| self.fail(ErrorKind::BadRequest, format!("{id} isn't in a HEY box, so it can't be moved (list a box to get its full ID)")))?;
        let kind = box_for(folder).ok_or_else(|| self.fail(ErrorKind::BadRequest, format!("HEY has no \"{folder}\" box")))?;
        self.run(&["move", &item.to_string(), "--to", kind], None).map(drop)
    }

    fn set_unread(&self, id: &str, unread: bool) -> Result<()> {
        let (_, item) = self.ids(id)?;
        let item = item.ok_or_else(|| self.fail(ErrorKind::BadRequest, format!("{id} isn't in a HEY box, so it can't be marked (list a box to get its full ID)")))?;
        self.run(&[if unread { "unseen" } else { "seen" }, &item.to_string()], None).map(drop)
    }

    fn screener(&self) -> Result<Vec<PendingSender>> {
        let mut out: Vec<PendingSender> = Vec::new();
        for c in self.clearances()? {
            let Some(clearance) = c["id"].as_i64() else { continue };
            let email = text(&c["email_address"]).to_ascii_lowercase();
            if email.is_empty() {
                continue;
            }
            out.push(PendingSender {
                email,
                name: nonempty(text(&c["name"])),
                thread_count: 1,
                last_subject: nonempty(text(&c["subject"])),
                last_at: 0,
                account: Some(self.name.clone()),
                id: Some(format!("{}:{clearance}", self.name)),
            });
        }
        Ok(out)
    }

    fn decide_sender(&self, id: &str, status: &str) -> Result<i64> {
        let clearance = self.local(id)?.parse::<i64>().map_err(|_| self.fail(ErrorKind::NotFound, format!("no Screener sender {id}")))?;
        let verb = match status {
            "approved" => "approve",
            "blocked" => "deny",
            other => return Err(self.fail(ErrorKind::BadRequest, format!("unknown decision {other}"))),
        };
        self.run(&["screener", verb, &clearance.to_string()], None)?;
        Ok(1)
    }

    fn identities(&self) -> Result<Vec<Address>> {
        let own = self.own_addresses();
        if own.is_empty() {
            // Distinguish "no senders" from "hey isn't working".
            self.run(&["account", "senders"], None)?;
        }
        Ok(own)
    }

    fn send(&self, req: &SendRequest) -> Result<SendResponse> {
        let text = req.text.as_str();
        let html = text_html(text);
        // The body goes as HTML (HEY reads -m as Markdown, which would join lines); a huge one
        // goes on stdin as Markdown instead, since an argument can't be that long.
        let (body_args, stdin): (Vec<String>, Option<&str>) =
            if html.len() < 96 * 1024 { (vec!["--message-html".into(), html], None) } else { (Vec::new(), Some(text)) };
        let bare = |list: &[String]| list.iter().map(|a| bare_email(a)).filter(|a| !a.is_empty()).collect::<Vec<_>>();
        let mut args: Vec<String>;
        let thread_id;
        match req.reply_to_message_id.as_deref() {
            Some(target) => {
                let (topic, _) = self.ids(target)?;
                thread_id = format!("{}:{topic}", self.name);
                args = vec!["reply".into(), topic.to_string(), "--replace-recipients".into()];
                for (flag, list) in [("--to", &req.to), ("--cc", &req.cc), ("--bcc", &req.bcc)] {
                    for a in bare(list) {
                        args.push(flag.into());
                        args.push(a);
                    }
                }
            }
            None => {
                thread_id = String::new();
                args = vec!["compose".into(), "--to".into(), bare(&req.to).join(","), "--subject".into(), req.subject.clone()];
                for (flag, list) in [("--cc", &req.cc), ("--bcc", &req.bcc)] {
                    if !list.is_empty() {
                        args.push(flag.into());
                        args.push(bare(list).join(","));
                    }
                }
                if let Some(from) = req.from.as_deref().filter(|f| !f.is_empty()) {
                    args.push("--from".into());
                    args.push(bare_email(from));
                }
            }
        }
        args.extend(body_args);
        // `hey` attaches files by path: each goes in a private directory, under its own name.
        let files = if req.attachments.is_empty() {
            None
        } else {
            let dir = PrivateDir::new(&std::env::temp_dir(), "cloudmail-hey-send").map_err(|e| self.fail(ErrorKind::AccountUnavailable, format!("could not create a temporary directory: {e}")))?;
            for (i, a) in req.attachments.iter().enumerate() {
                let path = dir
                    .write(&i.to_string(), &crate::attach::safe_filename(&a.filename), &a.content)
                    .map_err(|e| self.fail(ErrorKind::AccountUnavailable, format!("could not write {} for hey: {e}", a.filename)))?;
                args.push("--attach".into());
                args.push(path.to_string_lossy().into_owned());
            }
            Some(dir)
        };
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let sent = self.run(&refs, stdin);
        drop(files);
        sent?;
        Ok(SendResponse { thread_id: nonempty(thread_id), message: None, warning: None })
    }

    fn attachment_limit(&self) -> u64 {
        crate::attach::HEY_LIMIT
    }

    fn download_attachment(&self, id: &str) -> Result<Download> {
        let local = self.local(id)?.to_string();
        let dir = PrivateDir::new(&std::env::temp_dir(), "cloudmail-hey").map_err(|e| self.fail(ErrorKind::AccountUnavailable, format!("could not create a temporary directory: {e}")))?;
        let result = (|| {
            let target = format!("{}/", dir.path().display());
            let data = self.run(&["attachment", "save", &local, "--output", &target, "--force"], None)?;
            let path = data["path"].as_str().map(std::path::PathBuf::from).ok_or_else(|| self.shape_error("attachment save"))?;
            let bytes = std::fs::read(&path).map_err(|e| self.fail(ErrorKind::AccountUnavailable, format!("could not read the saved attachment: {e}")))?;
            let filename = nonempty(text(&data["filename"])).or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()));
            Ok(Download { bytes, content_type: None, filename })
        })();
        drop(dir);
        result
    }
}

impl Hey {
    fn screener_threads(&self) -> Result<Vec<ThreadSummary>> {
        Ok(self
            .clearances()?
            .iter()
            .filter_map(|c| {
                let topic = c["topic_id"].as_i64()?;
                Some(ThreadSummary {
                    id: format!("{}:{topic}", self.name),
                    subject: text(&c["subject"]),
                    folder: "screener".into(),
                    snippet: text(&c["summary"]),
                    from: Some(Address { name: nonempty(text(&c["name"])), email: text(&c["email_address"]).to_ascii_lowercase() }),
                    to_address: None,
                    message_count: 1,
                    unread: true,
                    has_attachments: false,
                    last_at: 0,
                    account: Some(self.name.clone()),
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_parse() {
        assert_eq!(parse_id("12:34"), Some((12, Some(34))));
        assert_eq!(parse_id("12"), Some((12, None)));
        assert_eq!(parse_id("12/99"), Some((12, None)));
        assert_eq!(parse_id("x"), None);
    }

    #[test]
    fn folders_map_both_ways() {
        for f in ["inbox", "paper_trail", "feed", "set_aside", "reply_later"] {
            assert_eq!(folder_for(box_for(f).unwrap()), f);
        }
        assert_eq!(box_for("archive"), Some("trailbox"));
        assert_eq!(box_for("sent"), None);
    }

    #[test]
    fn articles_split_per_entry() {
        let doc = "<!doctype html>\n<html><body>\n<article id=\"entry-7\" data-entry-id=\"7\">\n<header>\n<div>From: A</div>\n</header>\n<p>one</p>\n</article>\n<article id=\"entry-9\" data-entry-id=\"9\">\n<header>\n<div>From: B</div>\n</header>\n<p>two</article></p>\n</article>\n</body></html>";
        let m = split_articles(doc);
        assert_eq!(m[&7], "<p>one</p>");
        assert_eq!(m[&9], "<p>two</article></p>");
    }

    #[test]
    fn thread_read_times_are_utc_to_the_minute() {
        assert_eq!(millis(&json!("2026-09-30T16:46")), 1790786760000);
        assert_eq!(millis(&json!("2026-09-30T16:46:36Z")), 1790786796000);
        assert_eq!(millis(&json!("")), 0);
    }

    #[test]
    fn received_html_comes_out_of_its_trix_attachment() {
        let email = "<shadow-content><template><div style=\"color: red\">Hi &amp; bye</div></template></shadow-content>";
        let attr = serde_json::to_string(&json!({ "contentType": "text/html", "content": email, "data": "{}" })).unwrap();
        let attr = attr.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;");
        let body = format!("<div><figure data-trix-attachment=\"{attr}\"></figure></div>");
        assert_eq!(unwrap_trix(&body), "<div><div style=\"color: red\">Hi &amp; bye</div></div>");
        let image = "<figure data-trix-attachment=\"{&quot;contentType&quot;:&quot;image/png&quot;}\"><img src=\"x\"></figure>";
        assert_eq!(unwrap_trix(image), image, "other attachments stay");
        assert_eq!(unwrap_trix("<p>plain</p>"), "<p>plain</p>");
    }

    #[test]
    fn plain_text_keeps_its_lines_as_html() {
        assert_eq!(text_html("a <b>\n> c & d\n"), "<div>a &lt;b&gt;<br>&gt; c &amp; d</div>");
    }

    #[test]
    fn postings_map_to_threads() {
        let h = Hey::new("hey", &AccountConfig::default());
        let p = json!({
            "id": 5, "topic_id": 6, "name": "Hello", "summary": "Hi there", "seen": false, "visible_entry_count": 2,
            "includes_attachments": true, "active_at": "2026-09-30T09:50:31Z", "alternative_sender_name": "Ann Example",
            "creator": { "contactable_type": "Person", "name": "ann@example.com", "email_address": "ann@example.com" },
            "addressed_contacts": [{ "contactable_type": "User", "name": "Me", "email_address": "me@hey.com" }],
            "contacts": []
        });
        let t = h.summary_from_posting(&p, "imbox").unwrap();
        assert_eq!(t.id, "hey:6:5");
        assert_eq!(t.folder, "inbox");
        assert!(t.unread && t.has_attachments);
        assert_eq!(t.from.as_ref().unwrap().display(), "Ann Example");
        assert_eq!(t.to_address.as_deref(), Some("me@hey.com"));
        assert_eq!(t.last_at, 1790761831000);
        assert!(h.summary_from_posting(&json!({ "id": 1, "kind": "bundle" }), "imbox").is_none());
    }
}
