//! iCloud Mail the way icloud.com's own Mail app reaches it: its web services, with the signed-in
//! iCloud web session icloud-session holds (see `session.rs`). Nothing to sign in to here: when the
//! session ends, `cloudmail account login icloud` (or the app's Sign in button) asks icloud-session
//! for its sign-in window. Advanced Data Protection doesn't cover Mail, so this works with it on.
//!
//! The protocol below is read from icloud.com's Mail app itself, build `2636Hotfix65`
//! (`https://www.icloud.com/applications/mail2/current/en-us/main.js`, fetched 2026-10-09), which
//! decodes every answer with io-ts codecs, so the field names here are the ones it checks:
//!
//! - Mail calls go to the `mccgateway` webservice, `POST <mccgateway>/mailws2/v1/<path>`, with
//!   `Content-Type`/`Accept: application/json` and the request's fields plus `sessionHeaders`
//!   (`folder`, `modseq`, `threadmodseq`, `condstore: 1`, `qresync: 1`, `threadmode: 1`); `modseq`
//!   and `threadmodseq` may be `null`, which is what the app sends after `MODSEQ_TOO_OLD`, so this
//!   client, which keeps no sync state, always does. Failures carry `{errorCode, errorDescription}`.
//!   The `mccgateway` key also shows in a `/validate` reply quoted on CSDN (blog.csdn.net, 2024).
//! - `thread/search` `{responseType: "THREAD_DIGEST", includeFolderStatus, maxResults, before?,
//!   since?, searchText?, searchType: "anyfield", filters?: {unseen}}` → `threadList[{threadId,
//!   timestamp, senders?, subject?, flags?, count?, preview?}]`; a thread is unread unless its flags
//!   hold `\Seen`. Go-iClient (github.com/Johnw7789/Go-iClient, `icloud/mail.go`) captured the same.
//! - `thread/get` `{threadId, includeLabelIds}` → `messageMetadataList[{uid, folder, messageId,
//!   from[], to[], cc[], bcc[], subject, date, flags[], parts[{partId, contentType, isAttach,
//!   size, fileName, disposition}]}]`: the conversation, across mailboxes.
//! - `message/get` `{uid, parts: [partId], dontMarkAsRead: true}` (session folder: the message's)
//!   → `{longHeader, parts[{guid: "messagepart:<folder>/<uid>-<partId>", content}]}`. The app always sends `dontMarkAsRead`; marking is
//!   its own call, `thread/flag` `{method: "ADD"|"REMOVE", flags: ["SEEN"], threadIds}`.
//! - `thread/move` `{moveMethod: "MOVE", destFolder, threadIds}` (session folder: the source).
//! - Mailboxes by full path: `INBOX`, `Archive`, `Sent Messages` (the app's own constants).
//! - Raw messages: `GET <mccgateway>/mailws2/v1/message/download?guid=message:<folder>/<uid>&dsid=…&filename=…`;
//!   attachments: `GET <mccgateway>/mailws2/v1/message/part?guid=messagepart:<folder>/<uid>-<partId>&type=…&name=…`.
//! - Sending: `message/savedraft` `{date, from: "Name <address>", to[], cc[], bcc[],
//!   headerInReplyTo?, headerReferences?, subject, textBody, htmlBody, attachments[],
//!   webmailClientBuild, isHME}` → `{uid}`, then `draft/send` `{messageGuid: "Drafts/<uid>"}`
//!   (the `Drafts/<uid>` form is Go-iClient's capture). Files go first to `message/part`
//!   (`POST ?X-id=&X-type=&X-size=&X-name=`, the file as the body) → `{messagePart: {guid, url}}`,
//!   then into `attachments` as `{guid, name, url, contentID, size, datatype, …}`.
//! - Your addresses and name: `GET <mcc>/cc/mail/v1/account/<dsid>/preference/web/all?userEntryPoint=/mail/load`
//!   → `account{emailId, supportedDomains[{domain, allowSendFrom}], aliases[{emailId,
//!   supportedDomains, isActive, fullName}], customDomains[{emailId, domain, allowSendFrom,
//!   fullName}], fullName}`. A name left empty there is the signed-in user's, as in the app: here
//!   icloud-session's `FullName`.
//!
//! IDs are opaque base64url of the mailbox and its ids: a thread `icloud:t…` (the mailbox it was
//! listed in and its threadId), a message `icloud:t…/m…` (its mailbox and uid), an attachment
//! `icloud:a…`.
//!
//! The Screener: iCloud has none, so your worker's decides (see `unified.rs`).

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::client::ThreadQuery;
use crate::config::AccountConfig;
use crate::error::{Error, ErrorKind, Result};
use crate::gmail::{address_from, addresses, format_address, normalize_message_id};
use crate::provider::{AccountStatus, Provider};
use crate::session::{self, Session};
use crate::text::{bare_email, html_to_text, split_addresses};
use crate::types::*;

pub const INBOX: &str = "INBOX";
pub const ARCHIVE: &str = "Archive";
pub const SENT: &str = "Sent Messages";
/// iCloud Mail's Trash, which empties itself after 30 days.
pub const TRASH: &str = "Deleted Messages";
/// How long a sign-in in icloud-session's window may take.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(600);
/// Most threads one listing asks for.
const LIST_LIMIT: u32 = 200;

fn mailbox_for(folder: &str) -> Option<&'static str> {
    Some(match folder {
        "inbox" => INBOX,
        "archive" => ARCHIVE,
        "sent" => SENT,
        _ => return None,
    })
}

fn folder_for(mailbox: &str) -> &'static str {
    match mailbox {
        INBOX => "inbox",
        SENT => "sent",
        _ => "archive",
    }
}

#[derive(Serialize, Deserialize)]
struct ThreadRef {
    f: String,
    t: String,
}

#[derive(Serialize, Deserialize)]
struct MessageRef {
    f: String,
    u: String,
}

#[derive(Serialize, Deserialize)]
struct PartRef {
    f: String,
    u: String,
    p: String,
    t: String,
    n: String,
}

fn encode(tag: char, v: &impl Serialize) -> String {
    format!("{tag}{}", URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap_or_default()))
}

fn decode<T: for<'d> Deserialize<'d>>(tag: char, s: &str) -> Option<T> {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(s.strip_prefix(tag)?).ok()?).ok()
}

/// `toUTCString()` with GMT spelled `-0000`, as the app dates a draft.
fn draft_date() -> String {
    chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S -0000").to_string()
}

/// Plain text as HTML: escaped, lines kept.
fn text_html(text: &str) -> String {
    let escaped = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!("<div>{}</div>", escaped.trim_end().replace('\n', "<br>"))
}

fn str_of(v: &Value) -> String {
    v.as_str().unwrap_or_default().trim().to_string()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

fn has_flag(v: &Value, flag: &str) -> bool {
    v.as_array().into_iter().flatten().any(|f| f.as_str() == Some(flag))
}

pub struct Icloud {
    name: String,
    session: Session,
    own: Mutex<Option<Vec<Address>>>,
    /// Message-IDs of each thread as last read, for spotting copies of worker mail.
    message_ids: Mutex<HashMap<String, Vec<String>>>,
}

impl Icloud {
    pub fn new(name: &str, _cfg: &AccountConfig) -> Self {
        Self {
            name: name.to_string(),
            session: Session::new(),
            own: Default::default(),
            message_ids: Default::default(),
        }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    fn fail(&self, kind: ErrorKind, message: impl AsRef<str>) -> Error {
        Error::new(kind, format!("{}: {}", self.label(), message.as_ref()))
    }

    fn wrap(&self, e: Error) -> Error {
        if e.message.starts_with(self.label()) {
            e
        } else {
            Error::new(e.kind, format!("{}: {}", self.label(), e.message))
        }
    }

    fn unexpected(&self, what: &str) -> Error {
        self.fail(
            ErrorKind::AccountUnavailable,
            format!(
                "iCloud answered {what} with something this version doesn't understand (has icloud.com's Mail changed?)"
            ),
        )
    }

    /// `POST <mccgateway>/mailws2/v1/<path>` with `sessionHeaders`.
    fn call(&self, path: &str, folder: Option<&str>, mut body: Value) -> Result<Value> {
        let base = self.session.webservice("mccgateway").map_err(|e| self.wrap(e))?;
        body["sessionHeaders"] = json!({ "folder": folder, "modseq": null, "threadmodseq": null, "condstore": 1, "qresync": 1, "threadmode": 1 });
        let reply =
            self.session.send("POST", &format!("{base}/mailws2/v1/{path}"), Some(&body)).map_err(|e| self.wrap(e))?;
        let parsed: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
        if !(200..300).contains(&reply.status) {
            let code = str_of(&parsed["errorCode"]);
            let what = if code.is_empty() {
                format!("HTTP {}", reply.status)
            } else {
                format!("{code}: {}", str_of(&parsed["errorDescription"]))
            };
            let kind = if reply.status == 404 { ErrorKind::NotFound } else { ErrorKind::AccountUnavailable };
            return Err(self.fail(kind, format!("{path} failed ({what})")));
        }
        if !parsed.is_object() {
            return Err(self.unexpected(path));
        }
        Ok(parsed)
    }

    fn local<'a>(&self, id: &'a str) -> Result<&'a str> {
        id.strip_prefix(self.name.as_str())
            .and_then(|r| r.strip_prefix(':'))
            .ok_or_else(|| Error::new(ErrorKind::BadRequest, format!("{id} is not an {} ID", self.label())))
    }

    fn thread_ref(&self, id: &str) -> Result<(String, ThreadRef)> {
        let key = self.local(id)?.split('/').next().unwrap_or_default().to_string();
        let r = decode::<ThreadRef>('t', &key)
            .ok_or_else(|| self.fail(ErrorKind::NotFound, format!("{id} isn't an iCloud Mail thread ID")))?;
        Ok((key, r))
    }

    fn message_ref(&self, id: &str) -> Result<(String, MessageRef)> {
        let (key, msg) = self
            .local(id)?
            .split_once('/')
            .ok_or_else(|| self.fail(ErrorKind::BadRequest, format!("{id} is a thread, not a message")))?;
        let r = decode::<MessageRef>('m', msg)
            .ok_or_else(|| self.fail(ErrorKind::NotFound, format!("{id} isn't an iCloud Mail message ID")))?;
        Ok((key.to_string(), r))
    }

    fn summary(&self, t: &Value, mailbox: &str, own: &[String]) -> Option<ThreadSummary> {
        let thread_id = t["threadId"].as_str()?;
        // "STALE" stands in for a sender the server hasn't resolved yet; the app drops it too.
        let senders: Vec<Address> = strings(&t["senders"])
            .iter()
            .filter(|s| s.as_str() != "STALE")
            .map(|s| address_from(s))
            .filter(|a| a.email.contains('@'))
            .collect();
        let from =
            senders.iter().rev().find(|a| !own.contains(&a.email.to_ascii_lowercase())).or(senders.last()).cloned();
        Some(ThreadSummary {
            id: format!("{}:{}", self.name, encode('t', &ThreadRef { f: mailbox.into(), t: thread_id.into() })),
            subject: str_of(&t["subject"]),
            folder: folder_for(mailbox).into(),
            snippet: str_of(&t["preview"]),
            from,
            to_address: None,
            message_count: t["count"].as_i64().unwrap_or(1).max(1),
            unread: !has_flag(&t["flags"], "\\Seen"),
            has_attachments: has_flag(&t["flags"], "\\HasAttachment"),
            last_at: t["timestamp"].as_i64().unwrap_or(0),
            account: Some(self.name.clone()),
        })
    }

    fn search_mailbox(&self, mailbox: &str, q: &ThreadQuery, text: Option<&str>) -> Result<Vec<ThreadSummary>> {
        let own: Vec<String> = self.load_identities()?.into_iter().map(|a| a.email.to_ascii_lowercase()).collect();
        let mut body = json!({ "responseType": "THREAD_DIGEST", "includeFolderStatus": true, "maxResults": q.limit.clamp(1, LIST_LIMIT) });
        if let Some(before) = q.before {
            body["before"] = json!(before);
        }
        if let Some(since) = q.since {
            body["since"] = json!(since);
        }
        if let Some(words) = text.map(str::trim).filter(|w| !w.is_empty()) {
            body["searchText"] = json!(words);
            body["searchType"] = json!("anyfield");
        }
        if q.unread {
            body["filters"] = json!({ "unseen": true });
        }
        let data = self.call("thread/search", Some(mailbox), body)?;
        let list = data["threadList"].as_array().ok_or_else(|| self.unexpected("thread/search"))?;
        Ok(list.iter().filter_map(|t| self.summary(t, mailbox, &own)).collect())
    }

    /// Your addresses, as icloud.com's compose From menu lists them, by the app's own rules: the
    /// primary address on each of its domains, active aliases and custom-domain addresses; those
    /// that can send, else the primary one at icloud.com.
    fn load_identities(&self) -> Result<Vec<Address>> {
        if let Some(own) = self.own.lock().unwrap().clone() {
            return Ok(own);
        }
        let status = self.session.status().map_err(|e| self.wrap(e))?;
        let base = self.session.webservice("mcc").map_err(|e| self.wrap(e))?;
        let dsid = self.session.dsid().map_err(|e| self.wrap(e))?;
        let url = format!("{base}/cc/mail/v1/account/{dsid}/preference/web/all?userEntryPoint=%2Fmail%2Fload");
        let reply = self.session.send("GET", &url, None).map_err(|e| self.wrap(e))?;
        if !(200..300).contains(&reply.status) {
            return Err(self.fail(
                ErrorKind::AccountUnavailable,
                format!("reading your Mail settings failed (HTTP {})", reply.status),
            ));
        }
        let data: Value = serde_json::from_slice(&reply.body).map_err(|_| self.unexpected("preference/web/all"))?;
        own_addresses(&data["account"], &status.full_name)
            .ok_or_else(|| self.unexpected("preference/web/all"))
            .and_then(|own| {
                if own.is_empty() {
                    return Err(
                        self.fail(ErrorKind::AccountUnavailable, "this account has no address iCloud Mail sends from")
                    );
                }
                *self.own.lock().unwrap() = Some(own.clone());
                Ok(own)
            })
    }

    /// The conversation's messages, oldest first.
    fn thread_messages(&self, folder: &str, thread_id: &str) -> Result<Vec<Value>> {
        let data = self.call("thread/get", Some(folder), json!({ "threadId": thread_id, "includeLabelIds": false }))?;
        let mut list = data["messageMetadataList"].as_array().cloned().ok_or_else(|| self.unexpected("thread/get"))?;
        list.sort_by_key(|m| m["date"].as_i64().unwrap_or(0));
        Ok(list)
    }

    /// A message's header block and the contents of the given parts, never marking it read.
    fn message_parts(&self, folder: &str, uid: &str, parts: &[String]) -> Result<(String, HashMap<String, String>)> {
        let data =
            self.call("message/get", Some(folder), json!({ "uid": uid, "parts": parts, "dontMarkAsRead": true }))?;
        let contents = data["parts"]
            .as_array()
            .ok_or_else(|| self.unexpected("message/get"))?
            .iter()
            // Each part's guid is `messagepart:<folder>/<uid>-<partId>`.
            .filter_map(|p| {
                let guid = p["guid"].as_str()?;
                let part =
                    guid.strip_prefix("messagepart:").and_then(|g| g.rsplit_once('-')).map_or(guid, |(_, id)| id);
                Some((part.to_string(), p["content"].as_str()?.to_string()))
            })
            .collect();
        Ok((data["longHeader"].as_str().unwrap_or_default().to_string(), contents))
    }

    fn get_bytes(&self, url: &str) -> Result<Download> {
        let reply = self.session.send("GET", url, None).map_err(|e| self.wrap(e))?;
        match reply.status {
            200..=299 => Ok(Download { bytes: reply.body, content_type: reply.content_type, filename: None }),
            404 => Err(self.fail(ErrorKind::NotFound, "that message or attachment is gone")),
            s => Err(self.fail(ErrorKind::AccountUnavailable, format!("download failed (HTTP {s})"))),
        }
    }

    /// Uploads a file for a draft (`message/part`), returning its `attachments` entry.
    fn upload(&self, a: &OutgoingAttachment, n: usize) -> Result<Value> {
        let base = self.session.webservice("mccgateway").map_err(|e| self.wrap(e))?;
        let nanos =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let mut url =
            url::Url::parse(&format!("{base}/mailws2/v1/message/part")).map_err(|_| self.unexpected("message/part"))?;
        url.query_pairs_mut()
            .append_pair("X-id", &format!("cloudmail-{nanos:x}-{n}"))
            .append_pair("X-type", &a.mime_type)
            .append_pair("X-size", &a.content.len().to_string())
            .append_pair("X-name", &a.filename);
        let reply = self.session.post_bytes(url.as_str(), &a.content).map_err(|e| self.wrap(e))?;
        if !(200..300).contains(&reply.status) {
            return Err(self.fail(
                ErrorKind::AccountUnavailable,
                format!("uploading {} failed (HTTP {})", a.filename, reply.status),
            ));
        }
        let data: Value = serde_json::from_slice(&reply.body).map_err(|_| self.unexpected("message/part"))?;
        let part = &data["messagePart"];
        let (guid, part_url) = (str_of(&part["guid"]), str_of(&part["url"]));
        if guid.is_empty() || part_url.is_empty() {
            return Err(self.unexpected("message/part"));
        }
        // As the app lists an uploaded part: its URL on the service, by the part's own query.
        let query = part_url.split_once('?').map(|(_, q)| q).unwrap_or("");
        Ok(json!({
            "guid": guid, "name": a.filename, "url": format!("{base}/mailws2/v1/message/part?{query}"), "contentID": null,
            "size": a.content.len(), "mailDropExpirationDate": null, "isMailDropThumbnail": false,
            "datatype": if a.mime_type.is_empty() { "application/octet-stream" } else { a.mime_type.as_str() },
        }))
    }

    /// Signs in through icloud-session's window and waits for it.
    pub fn login(&self) -> Result<()> {
        if self.session.status().map_err(|e| self.wrap(e))?.signed_in {
            return Ok(());
        }
        self.session.sign_in().map_err(|e| self.wrap(e))?;
        match session::wait_for_sign_in(&self.session, SIGN_IN_TIMEOUT).map_err(|e| self.wrap(e))? {
            true => Ok(()),
            false => Err(self.fail(ErrorKind::AccountAuth, "the iCloud sign-in wasn't finished")),
        }
    }
}

/// The send-from addresses in Mail's account settings (see the module notes); None when the
/// settings don't have the shape the app reads.
fn own_addresses(account: &Value, session_name: &str) -> Option<Vec<Address>> {
    let email_id = str_of(&account["emailId"]);
    if email_id.is_empty() {
        return None;
    }
    let name_of = |v: &Value| {
        Some(str_of(v)).filter(|n| !n.is_empty()).or_else(|| Some(session_name.to_string()).filter(|n| !n.is_empty()))
    };
    let account_name = name_of(&account["fullName"]);
    // (can send, address, is the primary icloud.com one)
    let mut all: Vec<(bool, Address, bool)> = Vec::new();
    let mut on_domains = |id: &str, domains: &Value, name: Option<String>, primary: bool| {
        for d in domains.as_array().into_iter().flatten() {
            let domain = str_of(&d["domain"]);
            if !domain.is_empty() {
                all.push((
                    d["allowSendFrom"] == json!(true),
                    Address { name: name.clone(), email: format!("{id}@{domain}") },
                    primary && domain == "icloud.com",
                ));
            }
        }
    };
    on_domains(&email_id, &account["supportedDomains"], account_name.clone(), true);
    for alias in account["aliases"].as_array().into_iter().flatten().filter(|a| a["isActive"] == json!(true)) {
        on_domains(
            &str_of(&alias["emailId"]),
            &alias["supportedDomains"],
            name_of(&alias["fullName"]).or(account_name.clone()),
            false,
        );
    }
    for c in account["customDomains"].as_array().into_iter().flatten() {
        let (id, domain) = (str_of(&c["emailId"]), str_of(&c["domain"]));
        if !id.is_empty() && !domain.is_empty() {
            all.push((
                c["allowSendFrom"] == json!(true),
                Address { name: name_of(&c["fullName"]).or(account_name.clone()), email: format!("{id}@{domain}") },
                false,
            ));
        }
    }
    if all.len() == 1 {
        all[0].0 = true;
    }
    let can_send: Vec<Address> = all.iter().filter(|(send, _, _)| *send).map(|(_, a, _)| a.clone()).collect();
    Some(if can_send.is_empty() {
        all.into_iter().filter(|(_, _, primary)| *primary).map(|(_, a, _)| a).take(1).collect()
    } else {
        can_send
    })
}

impl Provider for Icloud {
    fn name(&self) -> &str {
        &self.name
    }

    fn label(&self) -> &str {
        "iCloud Mail"
    }

    fn has_folder(&self, folder: &str) -> bool {
        matches!(folder, "inbox" | "archive" | "sent" | "all")
    }

    fn screened_by_worker(&self) -> bool {
        true
    }

    fn sign_in(&self) -> Result<()> {
        self.login()
    }

    fn status(&self) -> AccountStatus {
        let mut status = AccountStatus {
            name: self.name.clone(),
            provider: "icloud".into(),
            label: self.label().into(),
            ..Default::default()
        };
        match self.session.status() {
            Ok(s) if s.signed_in => match self.load_identities() {
                Ok(own) => {
                    status.ok = true;
                    status.addresses = own.into_iter().map(|a| a.email).collect();
                    status.detail = format!("signed in to iCloud as {} (through icloud-session)", s.apple_id);
                }
                Err(e) => status.detail = e.message,
            },
            Ok(_) => status.detail = format!("not signed in: run `cloudmail account login {}`", self.name),
            Err(e) => status.detail = e.message,
        }
        status
    }

    fn threads(&self, q: &ThreadQuery) -> Result<Vec<ThreadSummary>> {
        let folder = if q.folder.is_empty() { "inbox" } else { q.folder.as_str() };
        let mailboxes: Vec<&str> = match folder {
            "all" => vec![INBOX, ARCHIVE, SENT],
            f => match mailbox_for(f) {
                Some(m) => vec![m],
                None => return Ok(Vec::new()),
            },
        };
        let mut list = Vec::new();
        for mailbox in mailboxes {
            list.extend(self.search_mailbox(mailbox, q, q.q.as_deref())?);
        }
        list.sort_by_key(|t| std::cmp::Reverse(t.last_at));
        list.truncate(q.limit.max(1) as usize);
        Ok(list)
    }

    fn search(&self, query: &str, limit: u32) -> Result<Vec<ThreadSummary>> {
        self.threads(&ThreadQuery { folder: "all".into(), q: Some(query.to_string()), limit, ..Default::default() })
    }

    fn thread(&self, id: &str, _html: bool) -> Result<ThreadDetail> {
        let (key, r) = self.thread_ref(id)?;
        let own: Vec<String> = self.load_identities()?.into_iter().map(|a| a.email.to_ascii_lowercase()).collect();
        let list = self.thread_messages(&r.f, &r.t)?;
        if list.is_empty() {
            return Err(self.fail(ErrorKind::NotFound, format!("no iCloud Mail thread {id} (moved or deleted?)")));
        }
        let thread_id = format!("{}:{key}", self.name);
        let mut messages = Vec::new();
        for m in &list {
            let (folder, uid) = (str_of(&m["folder"]), str_of(&m["uid"]));
            let parts = m["parts"].as_array().cloned().unwrap_or_default();
            let is_attachment = |p: &Value| p["isAttach"] == json!(true) || !str_of(&p["fileName"]).is_empty();
            let body_part = |p: &&Value| {
                !is_attachment(p)
                    && matches!(str_of(&p["contentType"]).to_ascii_lowercase().as_str(), "text/plain" | "text/html")
            };
            let body_ids: Vec<String> = parts.iter().filter(body_part).map(|p| str_of(&p["partId"])).collect();
            let (long_header, contents) = self.message_parts(&folder, &uid, &body_ids)?;
            let content_of = |kind: &str| {
                parts
                    .iter()
                    .filter(body_part)
                    .find(|p| str_of(&p["contentType"]).eq_ignore_ascii_case(kind))
                    .and_then(|p| contents.get(&str_of(&p["partId"])).cloned())
                    .filter(|c| !c.trim().is_empty())
            };
            let html = content_of("text/html");
            let text = content_of("text/plain").or_else(|| html.as_deref().map(html_to_text));
            let header = mail_parser::MessageParser::default().parse_headers(long_header.as_bytes());
            let reply_to = header
                .as_ref()
                .and_then(|h| h.reply_to())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| {
                            Some(Address {
                                name: x.name.as_deref().map(str::to_string),
                                email: x.address.as_deref()?.to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let from = strings(&m["from"]).first().map(|s| address_from(s)).unwrap_or_default();
            let attachments = parts
                .iter()
                .filter(|p| is_attachment(p))
                .map(|p| {
                    let (part, kind, name) =
                        (str_of(&p["partId"]), str_of(&p["contentType"]).to_ascii_lowercase(), str_of(&p["fileName"]));
                    Attachment {
                        id: format!(
                            "{}:{}",
                            self.name,
                            encode(
                                'a',
                                &PartRef {
                                    f: folder.clone(),
                                    u: uid.clone(),
                                    p: part,
                                    t: kind.clone(),
                                    n: name.clone()
                                }
                            )
                        ),
                        filename: if name.is_empty() { "attachment".into() } else { name },
                        mime_type: kind,
                        size: p["size"].as_i64().unwrap_or(0),
                        inline: str_of(&p["disposition"]).eq_ignore_ascii_case("INLINE"),
                    }
                })
                .collect();
            let message_id = str_of(&m["messageId"]);
            messages.push(Message {
                id: format!("{}:{key}/{}", self.name, encode('m', &MessageRef { f: folder.clone(), u: uid.clone() })),
                thread_id: thread_id.clone(),
                outgoing: folder == SENT || own.contains(&from.email.to_ascii_lowercase()),
                to: strings(&m["to"]).iter().flat_map(|s| addresses(s)).collect(),
                cc: strings(&m["cc"]).iter().flat_map(|s| addresses(s)).collect(),
                reply_to,
                from,
                subject: str_of(&m["subject"]),
                date: m["date"].as_i64().unwrap_or(0),
                text,
                html,
                message_id: Some(message_id).filter(|i| !i.is_empty()),
                attachments,
                auth: None,
            });
        }
        let latest = messages.last().expect("a thread has a message");
        let latest_in = messages.iter().rev().find(|m| !m.outgoing).unwrap_or(latest);
        let summary = ThreadSummary {
            id: thread_id.clone(),
            subject: messages.first().map(|m| m.subject.clone()).unwrap_or_default(),
            folder: folder_for(&r.f).into(),
            snippet: String::new(),
            from: Some(latest_in.from.clone()),
            to_address: latest_in
                .to
                .iter()
                .chain(&latest_in.cc)
                .find(|a| own.contains(&a.email.to_ascii_lowercase()))
                .map(|a| a.email.to_ascii_lowercase())
                .or_else(|| own.first().cloned()),
            message_count: messages.len() as i64,
            unread: list.iter().any(|m| !has_flag(&m["flags"], "\\Seen")),
            has_attachments: messages.iter().any(|m| m.attachments.iter().any(|a| !a.inline)),
            last_at: latest.date,
            account: Some(self.name.clone()),
        };
        self.message_ids.lock().unwrap().insert(
            thread_id,
            messages.iter().filter_map(|m| m.message_id.as_deref()).map(normalize_message_id).collect(),
        );
        Ok(ThreadDetail { thread: summary, messages })
    }

    fn move_thread(&self, id: &str, folder: &str) -> Result<()> {
        let dest = match folder {
            "archive" => ARCHIVE,
            "inbox" => INBOX,
            other => {
                return Err(self.fail(
                    ErrorKind::BadRequest,
                    format!("iCloud Mail threads move between the Inbox and the Archive, not \"{other}\""),
                ));
            }
        };
        let (_, r) = self.thread_ref(id)?;
        self.call("thread/move", Some(&r.f), json!({ "moveMethod": "MOVE", "destFolder": dest, "threadIds": [r.t] }))
            .map(drop)
    }

    fn delete_thread(&self, id: &str) -> Result<()> {
        let (_, r) = self.thread_ref(id)?;
        self.call("thread/move", Some(&r.f), json!({ "moveMethod": "MOVE", "destFolder": TRASH, "threadIds": [r.t] }))
            .map(drop)
    }

    fn set_unread(&self, id: &str, unread: bool) -> Result<()> {
        let (_, r) = self.thread_ref(id)?;
        self.call(
            "thread/flag",
            Some(&r.f),
            json!({ "method": if unread { "REMOVE" } else { "ADD" }, "flags": ["SEEN"], "threadIds": [r.t] }),
        )
        .map(drop)
    }

    fn screener(&self) -> Result<Vec<PendingSender>> {
        Ok(Vec::new())
    }

    fn decide_sender(&self, id: &str, _status: &str) -> Result<i64> {
        Err(self.fail(
            ErrorKind::BadRequest,
            format!("{id}: iCloud Mail's senders are decided in your Cloudmail Screener, by address"),
        ))
    }

    fn identities(&self) -> Result<Vec<Address>> {
        self.load_identities()
    }

    fn send(&self, req: &SendRequest) -> Result<SendResponse> {
        let own = self.load_identities()?;
        let from = match req.from.as_deref().map(bare_email).filter(|f| !f.is_empty()) {
            Some(f) => own.iter().find(|a| a.email.eq_ignore_ascii_case(&f)).cloned().ok_or_else(|| {
                self.fail(ErrorKind::BadRequest, format!("{f} isn't one of your iCloud Mail addresses"))
            })?,
            None => own[0].clone(),
        };
        let list = |items: &[String]| {
            items
                .iter()
                .flat_map(|i| split_addresses(i))
                .map(|a| format_address(&address_from(&a)))
                .filter(|a| a.contains('@'))
                .collect::<Vec<_>>()
        };
        let mut body = json!({
            "date": draft_date(),
            "from": format_address(&from),
            "to": list(&req.to),
            "cc": list(&req.cc),
            "bcc": list(&req.bcc),
            "subject": req.subject,
            "textBody": req.text,
            "htmlBody": text_html(&req.text),
            "attachments": [],
            "webmailClientBuild": "current",
            "isHME": false,
        });
        let mut thread_id = None;
        if let Some(target) = req.reply_to_message_id.as_deref() {
            let (key, m) = self.message_ref(target)?;
            let (long_header, _) = self.message_parts(&m.f, &m.u, &[])?;
            let header = mail_parser::MessageParser::default()
                .parse_headers(long_header.as_bytes())
                .ok_or_else(|| self.unexpected("message/get"))?;
            let message_id = header
                .message_id()
                .map(|i| format!("<{i}>"))
                .ok_or_else(|| self.fail(ErrorKind::BadRequest, "the message replied to has no Message-ID"))?;
            let mut refs: Vec<String> = match header.references() {
                mail_parser::HeaderValue::Text(t) => vec![format!("<{t}>")],
                mail_parser::HeaderValue::TextList(l) => l.iter().map(|t| format!("<{t}>")).collect(),
                _ => Vec::new(),
            };
            refs.push(message_id.clone());
            body["headerInReplyTo"] = json!(message_id);
            body["headerReferences"] = json!(refs.join(" "));
            thread_id = Some(format!("{}:{key}", self.name));
        }
        let uploaded: Vec<Value> =
            req.attachments.iter().enumerate().map(|(n, a)| self.upload(a, n)).collect::<Result<_>>()?;
        body["attachments"] = json!(uploaded);
        let draft = self.call("message/savedraft", None, body)?;
        let uid = draft["uid"].as_str().map(str::to_string).ok_or_else(|| self.unexpected("message/savedraft"))?;
        self.call("draft/send", None, json!({ "messageGuid": format!("Drafts/{uid}") }))?;
        Ok(SendResponse { thread_id, message: None, warning: None })
    }

    fn attachment_limit(&self) -> u64 {
        crate::attach::ICLOUD_LIMIT
    }

    fn download_attachment(&self, id: &str) -> Result<Download> {
        let p = decode::<PartRef>('a', self.local(id)?)
            .ok_or_else(|| self.fail(ErrorKind::NotFound, format!("{id} isn't an iCloud Mail attachment ID")))?;
        // A Mail Drop file (`mailDropStreamingUrl`, no partId) lives on Apple's servers for 30 days,
        // not in the message.
        if p.p.is_empty() {
            return Err(self.fail(
                ErrorKind::BadRequest,
                format!("{} is a Mail Drop link, not part of the message; open it in iCloud Mail on the web", p.n),
            ));
        }
        let base = self.session.webservice("mccgateway").map_err(|e| self.wrap(e))?;
        let mut url =
            url::Url::parse(&format!("{base}/mailws2/v1/message/part")).map_err(|_| self.unexpected("message/part"))?;
        url.query_pairs_mut()
            .append_pair("guid", &format!("messagepart:{}/{}-{}", p.f, p.u, p.p))
            .append_pair("type", &p.t)
            .append_pair("name", &p.n);
        let mut d = self.get_bytes(url.as_str())?;
        d.filename = Some(p.n).filter(|n| !n.is_empty());
        Ok(d)
    }

    fn raw_message(&self, id: &str) -> Result<Vec<u8>> {
        let (_, m) = self.message_ref(id)?;
        let base = self.session.webservice("mccgateway").map_err(|e| self.wrap(e))?;
        let dsid = self.session.dsid().map_err(|e| self.wrap(e))?;
        let mut url = url::Url::parse(&format!("{base}/mailws2/v1/message/download"))
            .map_err(|_| self.unexpected("message/download"))?;
        url.query_pairs_mut()
            .append_pair("guid", &format!("message:{}/{}", m.f, m.u))
            .append_pair("dsid", &dsid)
            .append_pair("filename", "message.eml");
        Ok(self.get_bytes(url.as_str())?.bytes)
    }

    fn message_ids(&self, thread_id: &str) -> Vec<String> {
        self.message_ids.lock().unwrap().get(thread_id).cloned().unwrap_or_default()
    }

    /// The senders in the Inbox and the recipients of sent mail (from each sent conversation).
    fn correspondents(&self, limit: u32) -> Result<Vec<Address>> {
        let own: Vec<String> = self.load_identities()?.into_iter().map(|a| a.email.to_ascii_lowercase()).collect();
        let q = ThreadQuery { limit, ..Default::default() };
        let mut out: Vec<Address> = self.search_mailbox(INBOX, &q, None)?.into_iter().filter_map(|t| t.from).collect();
        for t in self.search_mailbox(SENT, &q, None)? {
            let (_, r) = self.thread_ref(&t.id)?;
            for m in self.thread_messages(&r.f, &r.t)? {
                for list in [&m["to"], &m["cc"]] {
                    out.extend(strings(list).iter().flat_map(|s| addresses(s)));
                }
            }
        }
        out.retain(|a| !own.contains(&a.email.to_ascii_lowercase()));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_survive_any_mailbox_or_thread_id() {
        let ic = Icloud::new("icloud", &AccountConfig::default());
        let t = format!("icloud:{}", encode('t', &ThreadRef { f: "Sent Messages".into(), t: "a/b:c#d".into() }));
        let (_, r) = ic.thread_ref(&t).unwrap();
        assert_eq!((r.f.as_str(), r.t.as_str()), ("Sent Messages", "a/b:c#d"));
        let m = format!("{t}/{}", encode('m', &MessageRef { f: "INBOX".into(), u: "41".into() }));
        assert_eq!(ic.message_ref(&m).unwrap().1.u, "41");
        assert!(ic.message_ref(&t).is_err(), "a thread isn't a message");
    }

    #[test]
    fn send_from_addresses_follow_the_apps_rules() {
        let account = json!({
            "emailId": "ann", "fullName": "",
            "supportedDomains": [{ "domain": "icloud.com", "allowSendFrom": true }, { "domain": "me.com", "allowSendFrom": false }],
            "aliases": [{ "emailId": "ann.work", "isActive": true, "fullName": "Ann at Work", "supportedDomains": [{ "domain": "icloud.com", "allowSendFrom": true }] },
                        { "emailId": "old", "isActive": false, "supportedDomains": [{ "domain": "icloud.com", "allowSendFrom": true }] }],
            "customDomains": [{ "emailId": "hi", "domain": "ann.dev", "allowSendFrom": true }]
        });
        let own = own_addresses(&account, "Ann Example").unwrap();
        let shown: Vec<String> = own.iter().map(Address::formatted).collect();
        assert_eq!(
            shown,
            ["Ann Example <ann@icloud.com>", "Ann at Work <ann.work@icloud.com>", "Ann Example <hi@ann.dev>"]
        );
        let only_me_com =
            json!({ "emailId": "ann", "supportedDomains": [{ "domain": "icloud.com" }, { "domain": "me.com" }] });
        assert_eq!(
            own_addresses(&only_me_com, "").unwrap().iter().map(|a| a.email.as_str()).collect::<Vec<_>>(),
            ["ann@icloud.com"],
            "none may send: the primary icloud.com one"
        );
        assert!(own_addresses(&json!({}), "").is_none());
    }
}
