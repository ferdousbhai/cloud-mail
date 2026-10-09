//! iCloud Mail over IMAP (imap.mail.me.com:993, TLS) and SMTP (smtp.mail.me.com:587, STARTTLS),
//! signed in with an app-specific password from account.apple.com. Advanced Data Protection
//! doesn't cover iCloud Mail, so this works with it on.
//!
//! The password: kept in `~/.config/cloudmail/icloud/<account>`, readable only by you, as your
//! worker's API token is in config.toml. It can't sign in to your Apple Account itself, and you
//! can revoke it at account.apple.com at any time.
//!
//! IDs: IMAP has no thread IDs, so threads are grouped here by Message-ID, In-Reply-To and
//! References. A thread is `icloud:t<root>`, the root being its first message's Message-ID in
//! unpadded base64url, so the ID stays the same when the thread moves between mailboxes. A
//! message is `icloud:t<root>/<box>.<uidvalidity>.<uid>` and an attachment
//! `icloud:<box>.<uidvalidity>.<uid>#<n>`, `<box>` being `i` (Inbox), `a` (Archive) or `s` (Sent).
//!
//! Folders: the Inbox is INBOX, the Archive iCloud's Archive mailbox (created on the first archive
//! when there is none) and Sent its "Sent Messages". iCloud has no Screener, so its mail goes
//! straight to your Inbox, as Gmail's does.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use lettre::Transport;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{Certificate, Tls, TlsParameters};
use mail_parser::{MessageParser, MimeHeaders};
use rustls::ClientConfig;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::client::ThreadQuery;
use crate::config::AccountConfig;
use crate::error::{Error, ErrorKind, Result};
use crate::gmail::{Threading, build_message, normalize_message_id};
use crate::imap::{self, Arg, Fetched, Session, raw, string};
use crate::provider::{AccountStatus, Provider};
use crate::text::{bare_email, html_to_text, split_addresses};
use crate::types::*;

/// `host:port` of the IMAP server, instead of imap.mail.me.com:993 (tests, a local proxy).
pub const IMAP_ENV: &str = "CLOUDMAIL_ICLOUD_IMAP";
/// `host:port` of the SMTP server, instead of smtp.mail.me.com:587.
pub const SMTP_ENV: &str = "CLOUDMAIL_ICLOUD_SMTP";
/// A PEM file of one more certificate authority to trust, beside the usual ones (tests).
pub const CA_ENV: &str = "CLOUDMAIL_ICLOUD_CA";
/// Where app-specific passwords are made: Sign-In and Security → App-Specific Passwords.
pub const PASSWORD_URL: &str = "https://account.apple.com/account/manage";
/// The domains an iCloud Mail address signs in with.
pub const DOMAINS: &[&str] = &["icloud.com", "me.com", "mac.com"];

const IMAP_SERVER: (&str, u16) = ("imap.mail.me.com", 993);
const SMTP_SERVER: (&str, u16) = ("smtp.mail.me.com", 587);
const TIMEOUT: Duration = Duration::from_secs(60);
/// Messages fetched per round trip when listing.
const CHUNK: usize = 100;
/// The most messages one listing reads per mailbox before giving up on filling the page.
const MAX_SCAN: usize = 1000;
/// Idle signed-in connections kept for the next call.
const POOL: usize = 2;
/// How long to give iCloud to file a sent message in Sent Messages itself before saving a copy.
const SENT_COPY_WAIT: Duration = Duration::from_millis(1500);
const HEADER_FIELDS: &str = "FROM TO CC REPLY-TO SUBJECT DATE MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE CONTENT-TRANSFER-ENCODING";
const WHOLE: &str = "(UID FLAGS INTERNALDATE BODY.PEEK[])";

/// The mailboxes cloudmail reads, by the letter IDs use for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoxKind {
    Inbox,
    Archive,
    Sent,
}

impl BoxKind {
    fn letter(self) -> char {
        match self {
            BoxKind::Inbox => 'i',
            BoxKind::Archive => 'a',
            BoxKind::Sent => 's',
        }
    }

    fn from_letter(c: &str) -> Option<Self> {
        Some(match c {
            "i" => BoxKind::Inbox,
            "a" => BoxKind::Archive,
            "s" => BoxKind::Sent,
            _ => return None,
        })
    }

    fn folder(self) -> &'static str {
        match self {
            BoxKind::Inbox => "inbox",
            BoxKind::Archive => "archive",
            BoxKind::Sent => "sent",
        }
    }
}

const ALL_BOXES: [BoxKind; 3] = [BoxKind::Inbox, BoxKind::Archive, BoxKind::Sent];

/// The account's mailbox names, from LIST.
#[derive(Debug, Clone)]
struct Boxes {
    archive: Option<String>,
    sent: String,
}

impl Boxes {
    fn name(&self, kind: BoxKind) -> Option<&str> {
        match kind {
            BoxKind::Inbox => Some("INBOX"),
            BoxKind::Archive => self.archive.as_deref(),
            BoxKind::Sent => Some(&self.sent),
        }
    }

    fn from_list(list: &[imap::Mailbox]) -> Self {
        let named = |n: &str| list.iter().find(|m| m.selectable && m.name.eq_ignore_ascii_case(n)).map(|m| m.name.clone());
        let sent = list.iter().find(|m| m.sent && m.selectable).map(|m| m.name.clone()).or_else(|| named("Sent Messages")).or_else(|| named("Sent")).unwrap_or_else(|| "Sent Messages".into());
        let archive = list.iter().find(|m| m.archive && m.selectable).map(|m| m.name.clone()).or_else(|| named("Archive"));
        Self { archive, sent }
    }
}

/// How many messages a listing reads from a mailbox, and how far back.
#[derive(Debug, Clone, Copy)]
struct Page {
    want: usize,
    floor: Option<i64>,
}

/// Where a message is: its mailbox, that mailbox's UIDVALIDITY and its UID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Loc {
    kind: BoxKind,
    validity: u32,
    uid: u32,
}

impl Loc {
    fn encode(self) -> String {
        format!("{}.{}.{}", self.kind.letter(), self.validity, self.uid)
    }

    fn parse(s: &str) -> Option<Self> {
        let mut parts = s.split('.');
        let kind = BoxKind::from_letter(parts.next()?)?;
        let validity = parts.next()?.parse().ok()?;
        let uid = parts.next()?.parse().ok()?;
        parts.next().is_none().then_some(Self { kind, validity, uid })
    }
}

/// One message as read from IMAP: where it is and what listing and threading need.
#[derive(Debug, Clone)]
struct Meta {
    loc: Loc,
    seen: bool,
    date: i64,
    from: Option<Address>,
    to: Vec<Address>,
    cc: Vec<Address>,
    subject: String,
    /// Normalized Message-ID (no brackets, lowercase), or "" when there is none.
    message_id: String,
    in_reply_to: Vec<String>,
    references: Vec<String>,
    attachments: bool,
    snippet: String,
}

impl Meta {
    /// The key this message is known by in threading: its Message-ID, else its place.
    fn key(&self) -> String {
        if self.message_id.is_empty() { format!("uid:{}", self.loc.encode()) } else { self.message_id.clone() }
    }

    /// The first message of its conversation, as far as its own headers tell.
    fn root(&self) -> String {
        self.references.first().or(self.in_reply_to.first()).cloned().unwrap_or_else(|| self.key())
    }
}

fn ids_of(v: &mail_parser::HeaderValue<'_>) -> Vec<String> {
    let list: Vec<String> = match v {
        mail_parser::HeaderValue::Text(t) => vec![t.to_string()],
        mail_parser::HeaderValue::TextList(l) => l.iter().map(|t| t.to_string()).collect(),
        _ => Vec::new(),
    };
    list.iter().map(|i| normalize_message_id(i)).filter(|i| !i.is_empty()).collect()
}

fn addrs(a: Option<&mail_parser::Address<'_>>) -> Vec<Address> {
    a.map(|a| {
        a.iter()
            .filter_map(|x| {
                let email = x.address.as_deref()?.trim().to_string();
                email.contains('@').then(|| Address { name: x.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(str::to_string), email })
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Milliseconds from an INTERNALDATE ("02-Oct-2026 09:15:00 +0000").
fn internal_millis(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_str(s.trim(), "%d-%b-%Y %H:%M:%S %z").ok().map(|d| d.timestamp_millis())
}

fn meta(loc: Loc, f: &Fetched, bytes: &[u8]) -> Meta {
    let parsed = MessageParser::default().parse(bytes);
    let date_header = parsed.as_ref().and_then(|m| m.date()).map(|d| d.to_timestamp() * 1000);
    let date = f.internal_date.as_deref().and_then(internal_millis).or(date_header).unwrap_or(0);
    let mut out = Meta {
        loc,
        seen: f.seen(),
        date,
        from: None,
        to: Vec::new(),
        cc: Vec::new(),
        subject: String::new(),
        message_id: String::new(),
        in_reply_to: Vec::new(),
        references: Vec::new(),
        attachments: false,
        snippet: String::new(),
    };
    let Some(m) = parsed else { return out };
    out.attachments = m.content_type().is_some_and(|ct| ct.ctype().eq_ignore_ascii_case("multipart") && ct.subtype().is_some_and(|s| s.eq_ignore_ascii_case("mixed")));
    out.snippet = m.body_preview(200).map(|p| p.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default().chars().take(160).collect();
    out.from = addrs(m.from()).into_iter().next();
    out.to = addrs(m.to());
    out.cc = addrs(m.cc());
    out.subject = m.subject().unwrap_or_default().trim().to_string();
    out.message_id = m.message_id().map(normalize_message_id).unwrap_or_default();
    out.in_reply_to = ids_of(m.in_reply_to());
    out.references = ids_of(m.references());
    out
}

/// Groups messages into conversations: messages that name one another (Message-ID, In-Reply-To,
/// References) share one. Each group is oldest first and comes with its root, the key of its
/// thread ID; the newest conversation comes first.
fn group(metas: Vec<Meta>) -> Vec<(String, Vec<Meta>)> {
    fn find(parent: &mut HashMap<String, String>, k: &str) -> String {
        let mut k = k.to_string();
        let mut path = Vec::new();
        while let Some(p) = parent.get(&k).filter(|p| **p != k).cloned() {
            path.push(k);
            k = p;
        }
        for p in path {
            parent.insert(p, k.clone());
        }
        k
    }
    let mut parent: HashMap<String, String> = HashMap::new();
    for m in &metas {
        let key = m.key();
        parent.entry(key.clone()).or_insert_with(|| key.clone());
        for other in m.references.iter().chain(&m.in_reply_to) {
            parent.entry(other.clone()).or_insert_with(|| other.clone());
            let (a, b) = (find(&mut parent, &key), find(&mut parent, other));
            if a != b {
                // The smaller key wins, so the grouping doesn't depend on the order of messages.
                let (keep, merge) = if a < b { (a, b) } else { (b, a) };
                parent.insert(merge, keep);
            }
        }
    }
    let mut groups: HashMap<String, Vec<Meta>> = HashMap::new();
    for m in metas {
        let g = find(&mut parent, &m.key());
        groups.entry(g).or_default().push(m);
    }
    let mut out: Vec<(String, Vec<Meta>)> = groups
        .into_values()
        .map(|mut list| {
            list.sort_by_key(|m| (m.date, m.loc.uid));
            // A message in two mailboxes (one you sent yourself) counts once.
            let mut seen = HashSet::new();
            list.retain(|m| m.message_id.is_empty() || seen.insert(m.message_id.clone()));
            (list[0].root(), list)
        })
        .collect();
    out.sort_by_key(|(_, list)| std::cmp::Reverse(list.last().map(|m| m.date).unwrap_or(0)));
    out
}

/// A subject without its reply and forward prefixes, for a conversation whose first message isn't
/// at hand (only a reply was listed).
fn original_subject(subject: &str) -> String {
    let mut s = subject.trim();
    while let Some(p) = ["re:", "fwd:", "fw:", "aw:", "sv:"].iter().find(|p| s.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p))) {
        s = s[p.len()..].trim_start();
    }
    s.to_string()
}

fn thread_key(root: &str) -> String {
    format!("t{}", URL_SAFE_NO_PAD.encode(root.as_bytes()))
}

fn root_of(key: &str) -> Option<String> {
    let encoded = key.strip_prefix('t')?;
    String::from_utf8(URL_SAFE_NO_PAD.decode(encoded).ok()?).ok().filter(|r| !r.is_empty())
}

/// `OR a OR b c` for search keys.
fn any_of(terms: Vec<Vec<Arg>>) -> Vec<Arg> {
    let n = terms.len();
    let mut out = Vec::new();
    for (i, t) in terms.into_iter().enumerate() {
        if i + 1 < n {
            out.push(raw("OR"));
        }
        out.extend(t);
    }
    out
}

/// `TEXT <words>`, or nothing for no words.
fn text_search(words: &str) -> Vec<Arg> {
    let words = words.trim();
    if words.is_empty() { Vec::new() } else { vec![raw("TEXT"), string(words)] }
}

/// Search keys as UID SEARCH takes them: led by `CHARSET UTF-8` when any string isn't ASCII.
fn criteria(keys: Vec<Arg>) -> Vec<Arg> {
    if keys.iter().any(|k| matches!(k, Arg::Str(s) if !s.is_ascii())) {
        let mut out = vec![raw("CHARSET UTF-8")];
        out.extend(keys);
        out
    } else {
        keys
    }
}

fn header_term(field: &str, value: &str) -> Vec<Arg> {
    vec![raw(format!("HEADER {field}")), string(value)]
}

/// The TLS setup for IMAP: the usual web roots, plus `CLOUDMAIL_ICLOUD_CA` when set.
fn tls_config() -> std::result::Result<Arc<ClientConfig>, String> {
    static CONFIG: OnceLock<std::result::Result<Arc<ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            use rustls::pki_types::CertificateDer;
            use rustls::pki_types::pem::PemObject;
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            if let Some(path) = extra_ca() {
                let pem = std::fs::read(&path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
                for cert in CertificateDer::pem_slice_iter(&pem) {
                    roots.add(cert.map_err(|e| format!("{}: {e}", path.display()))?).map_err(|e| format!("{}: {e}", path.display()))?;
                }
            }
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().map_err(|e| e.to_string())?.with_root_certificates(roots).with_no_client_auth();
            Ok(Arc::new(config))
        })
        .clone()
}

fn extra_ca() -> Option<PathBuf> {
    std::env::var(CA_ENV).ok().filter(|v| !v.trim().is_empty()).map(PathBuf::from)
}

fn server(env: &str, default: (&str, u16)) -> (String, u16) {
    std::env::var(env)
        .ok()
        .and_then(|v| {
            let (h, p) = v.trim().rsplit_once(':')?;
            Some((h.to_string(), p.parse().ok()?))
        })
        .unwrap_or_else(|| (default.0.to_string(), default.1))
}

/// Where an account's app-specific password is kept.
pub fn password_path(name: &str) -> PathBuf {
    crate::config::path().parent().map(PathBuf::from).unwrap_or_default().join("icloud").join(name)
}

/// Keeps an account's app-specific password, readable only by you.
pub fn save_password(name: &str, password: &str) -> std::io::Result<PathBuf> {
    let path = password_path(name);
    crate::config::write_private(&path, &format!("{}\n", password.trim()))?;
    Ok(path)
}

/// Removes an account's saved password; whether there was one.
pub fn forget_password(name: &str) -> std::io::Result<bool> {
    match std::fs::remove_file(password_path(name)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Opens the page where app-specific passwords are made, in the browser.
pub fn open_password_page() {
    crate::gmail::open_browser(PASSWORD_URL);
}

/// Whether an address is one iCloud Mail signs in with.
pub fn is_icloud_address(email: &str) -> bool {
    email.rsplit_once('@').is_some_and(|(local, domain)| !local.is_empty() && DOMAINS.iter().any(|d| domain.eq_ignore_ascii_case(d)))
}

pub struct Icloud {
    name: String,
    email: String,
    aliases: Vec<String>,
    imap: (String, u16),
    smtp: (String, u16),
    /// A password given directly (while linking), instead of the saved one.
    password: Option<String>,
    pool: Mutex<Vec<Session>>,
    /// The user name iCloud's IMAP took: the address's name part, or the whole address.
    login_user: Mutex<Option<String>>,
    boxes: Mutex<Option<Boxes>>,
    /// Message-IDs of each thread as last listed, for spotting copies of worker mail.
    message_ids: Mutex<HashMap<String, Vec<String>>>,
}

impl Icloud {
    pub fn new(name: &str, cfg: &AccountConfig) -> Self {
        Self {
            name: name.to_string(),
            email: cfg.email.as_deref().map(bare_email).unwrap_or_default(),
            aliases: cfg.aliases.iter().map(|a| bare_email(a)).filter(|a| a.contains('@')).collect(),
            imap: server(IMAP_ENV, IMAP_SERVER),
            smtp: server(SMTP_ENV, SMTP_SERVER),
            password: None,
            pool: Default::default(),
            login_user: Default::default(),
            boxes: Default::default(),
            message_ids: Default::default(),
        }
    }

    /// The same account, signing in with this password instead of the saved one.
    pub fn with_password(mut self, password: &str) -> Self {
        self.password = Some(password.trim().to_string());
        self
    }

    pub fn email(&self) -> &str {
        &self.email
    }

    fn fail(&self, kind: ErrorKind, message: impl AsRef<str>) -> Error {
        Error::new(kind, format!("{}: {}", self.label(), message.as_ref()))
    }

    fn sign_in_again(&self) -> String {
        format!("run `cloudmail account login {}` with a new app-specific password from {PASSWORD_URL}", self.name)
    }

    fn imap_error(&self, f: imap::Fail) -> Error {
        match f {
            imap::Fail::Io(m) => self.fail(ErrorKind::AccountUnavailable, format!("can't reach {} ({m}); offline?", self.imap.0)),
            imap::Fail::Auth(m) => {
                let why = if m.is_empty() { "no reason given".to_string() } else { m };
                self.fail(ErrorKind::AccountAuth, format!("iCloud refused the app-specific password ({why}); {}", self.sign_in_again()))
            }
            imap::Fail::No(m) | imap::Fail::Protocol(m) => self.fail(ErrorKind::AccountUnavailable, m),
        }
    }

    fn password(&self) -> Result<String> {
        if let Some(p) = &self.password {
            return Ok(p.clone());
        }
        let missing = || self.fail(ErrorKind::AccountAuth, format!("no app-specific password is saved; {}", self.sign_in_again()));
        match std::fs::read_to_string(password_path(&self.name)) {
            Ok(p) if !p.trim().is_empty() => Ok(p.trim().to_string()),
            Ok(_) => Err(missing()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(missing()),
            Err(e) => Err(self.fail(ErrorKind::AccountUnavailable, format!("can't read {}: {e}", password_path(&self.name).display()))),
        }
    }

    /// Connects and signs in. iCloud's IMAP takes the address's name part as the user name (its
    /// documented form) and, on most accounts, the whole address; whichever works is remembered.
    fn connect(&self) -> Result<Session> {
        if self.email.is_empty() {
            return Err(self.fail(ErrorKind::Config, format!("no iCloud address is configured; run `cloudmail account add icloud --name {}`", self.name)));
        }
        let password = self.password()?;
        let tls = tls_config().map_err(|e| self.fail(ErrorKind::Config, e))?;
        let mut session = Session::connect(&self.imap.0, self.imap.1, tls, TIMEOUT).map_err(|e| self.imap_error(e))?;
        let known = self.login_user.lock().unwrap().clone();
        let users = match known {
            Some(u) => vec![u],
            None => vec![self.email.split('@').next().unwrap_or_default().to_string(), self.email.clone()],
        };
        let mut refused = imap::Fail::Auth(String::new());
        for user in users {
            match session.login(&user, &password) {
                Ok(()) => {
                    *self.login_user.lock().unwrap() = Some(user);
                    return Ok(session);
                }
                Err(e @ imap::Fail::Auth(_)) => refused = e,
                Err(e) => return Err(self.imap_error(e)),
            }
        }
        Err(self.imap_error(refused))
    }

    /// Runs `f` on a signed-in session: an idle one that still answers, else a new one. The
    /// session goes back to the pool only when `f` succeeded.
    fn with<T>(&self, f: impl FnOnce(&mut Session, &Boxes) -> Result<T>) -> Result<T> {
        let mut session = loop {
            let pooled = self.pool.lock().unwrap().pop();
            let Some(mut s) = pooled else { break self.connect()? };
            if s.noop().is_ok() {
                break s;
            }
        };
        let boxes = self.boxes(&mut session)?;
        let result = f(&mut session, &boxes);
        if result.is_ok() {
            let mut pool = self.pool.lock().unwrap();
            if pool.len() < POOL {
                pool.push(session);
            }
        }
        result
    }

    fn boxes(&self, s: &mut Session) -> Result<Boxes> {
        if let Some(b) = self.boxes.lock().unwrap().clone() {
            return Ok(b);
        }
        let b = Boxes::from_list(&s.list().map_err(|e| self.imap_error(e))?);
        *self.boxes.lock().unwrap() = Some(b.clone());
        Ok(b)
    }

    /// Opens the mailbox a stored ID points into, checking it is still the same one.
    fn open_at(&self, s: &mut Session, boxes: &Boxes, loc: Loc, read_only: bool) -> Result<()> {
        let name = boxes.name(loc.kind).ok_or_else(|| self.fail(ErrorKind::NotFound, "there's no Archive mailbox"))?;
        let validity = s.open(name, read_only).map_err(|e| self.imap_error(e))?;
        if validity != loc.validity {
            return Err(self.fail(ErrorKind::NotFound, "that message is gone (its mailbox changed since it was listed)"));
        }
        Ok(())
    }

    fn own(&self) -> Vec<String> {
        let mut own = vec![self.email.clone()];
        own.extend(self.aliases.iter().cloned());
        own
    }

    fn is_own(&self, a: &Address) -> bool {
        self.own().iter().any(|o| o.eq_ignore_ascii_case(&a.email))
    }

    /// A mailbox's messages matching `criteria` that `keep` keeps, newest first: read back from
    /// the newest a chunk at a time until the page is full or its messages are older than its floor.
    fn scan(&self, s: &mut Session, boxes: &Boxes, kind: BoxKind, criteria: Vec<Arg>, page: Page, keep: impl Fn(&Meta) -> bool) -> Result<Vec<Meta>> {
        let Some(name) = boxes.name(kind) else { return Ok(Vec::new()) };
        let validity = s.open(name, true).map_err(|e| self.imap_error(e))?;
        let uids = s.uid_search(criteria).map_err(|e| self.imap_error(e))?;
        let items = format!("(UID FLAGS INTERNALDATE BODY.PEEK[HEADER.FIELDS ({HEADER_FIELDS})] BODY.PEEK[TEXT]<0.2048>)");
        let mut out = Vec::new();
        let mut scanned = 0;
        for chunk in uids.rchunks(CHUNK) {
            let mut fetched = s.uid_fetch(chunk, &items).map_err(|e| self.imap_error(e))?;
            fetched.sort_by_key(|f| std::cmp::Reverse(f.uid));
            let mut oldest = i64::MAX;
            for f in &fetched {
                let mut bytes = f.header.clone().unwrap_or_default();
                bytes.extend_from_slice(f.text.as_deref().unwrap_or_default());
                let m = meta(Loc { kind, validity, uid: f.uid }, f, &bytes);
                oldest = oldest.min(m.date);
                if keep(&m) {
                    out.push(m);
                }
            }
            scanned += chunk.len();
            // UIDs grow with arrival, so older chunks only get older.
            if out.len() >= page.want || scanned >= MAX_SCAN || page.floor.is_some_and(|f| oldest <= f) {
                break;
            }
        }
        Ok(out)
    }

    fn summarize(&self, root: &str, list: &[Meta], folder: Option<&str>) -> ThreadSummary {
        let latest = list.last().expect("a conversation has a message");
        let latest_in = list.iter().rev().find(|m| m.loc.kind != BoxKind::Sent && !m.from.as_ref().is_some_and(|a| self.is_own(a))).unwrap_or(latest);
        let to_address = latest_in.to.iter().chain(&latest_in.cc).find(|a| self.is_own(a)).map(|a| a.email.to_ascii_lowercase()).unwrap_or_else(|| self.email.clone());
        let folder = folder.map(str::to_string).unwrap_or_else(|| {
            // Where the conversation stands: the Inbox if any of it is there, else the Archive, else Sent.
            let kinds: Vec<BoxKind> = list.iter().map(|m| m.loc.kind).collect();
            ALL_BOXES.into_iter().find(|k| kinds.contains(k)).unwrap_or(BoxKind::Inbox).folder().to_string()
        });
        ThreadSummary {
            id: format!("{}:{}", self.name, thread_key(root)),
            subject: list
                .iter()
                .find(|m| !m.subject.is_empty())
                .map(|m| if m.references.is_empty() && m.in_reply_to.is_empty() { m.subject.clone() } else { original_subject(&m.subject) })
                .unwrap_or_default(),
            folder,
            snippet: latest.snippet.clone(),
            from: latest_in.from.clone(),
            to_address: Some(to_address),
            message_count: list.len() as i64,
            unread: list.iter().any(|m| !m.seen && m.loc.kind != BoxKind::Sent),
            has_attachments: list.iter().any(|m| m.attachments),
            last_at: latest.date,
            account: Some(self.name.clone()),
        }
    }

    /// Threads from these messages, newest first, remembering their Message-IDs.
    fn threads_of(&self, metas: Vec<Meta>, folder: Option<&str>) -> Vec<ThreadSummary> {
        let mut cache = self.message_ids.lock().unwrap();
        group(metas)
            .iter()
            .map(|(root, list)| {
                let t = self.summarize(root, list, folder);
                cache.insert(t.id.clone(), list.iter().map(|m| m.message_id.clone()).filter(|m| !m.is_empty()).collect());
                t
            })
            .collect()
    }

    fn local<'a>(&self, id: &'a str) -> Result<&'a str> {
        id.strip_prefix(self.name.as_str())
            .and_then(|r| r.strip_prefix(':'))
            .ok_or_else(|| Error::new(ErrorKind::BadRequest, format!("{id} is not an {} ID", self.label())))
    }

    /// The thread key and root of a thread or message ID.
    fn thread_ref(&self, id: &str) -> Result<(String, String)> {
        let key = self.local(id)?.split('/').next().unwrap_or_default().to_string();
        let root = root_of(&key).ok_or_else(|| self.fail(ErrorKind::NotFound, format!("{id} isn't an iCloud Mail thread ID")))?;
        Ok((key, root))
    }

    fn message_ref(&self, id: &str) -> Result<(String, Loc)> {
        let local = self.local(id)?;
        let (key, loc) = local.split_once('/').ok_or_else(|| self.fail(ErrorKind::BadRequest, format!("{id} is a thread, not a message")))?;
        let loc = Loc::parse(loc).ok_or_else(|| self.fail(ErrorKind::NotFound, format!("{id} isn't an iCloud Mail message ID")))?;
        Ok((key.to_string(), loc))
    }

    /// Every message of a conversation in these mailboxes: those naming its root, then (twice
    /// more at most) those answering what was found, for replies that carry only In-Reply-To.
    fn find_thread(&self, s: &mut Session, boxes: &Boxes, root: &str, kinds: &[BoxKind]) -> Result<Vec<(Loc, Fetched)>> {
        if let Some(loc) = root.strip_prefix("uid:").and_then(Loc::parse) {
            // A message without a Message-ID is a conversation of its own.
            if !kinds.contains(&loc.kind) {
                return Ok(Vec::new());
            }
            self.open_at(s, boxes, loc, true)?;
            let f = s.uid_fetch(&[loc.uid], WHOLE).map_err(|e| self.imap_error(e))?;
            return Ok(f.into_iter().map(|f| (loc, f)).collect());
        }
        let mut found: Vec<(Loc, Fetched)> = Vec::new();
        let mut asked: Vec<String> = Vec::new();
        let mut next = vec![root.to_string()];
        for round in 0..3 {
            next.truncate(20);
            if next.is_empty() {
                break;
            }
            let terms: Vec<Vec<Arg>> = next
                .iter()
                .flat_map(|id| {
                    let mut t = vec![header_term("In-Reply-To", id)];
                    if round == 0 {
                        t.push(header_term("Message-ID", id));
                        t.push(header_term("References", id));
                    }
                    t
                })
                .collect();
            asked.append(&mut next);
            let criteria = any_of(terms);
            for &kind in kinds {
                let Some(name) = boxes.name(kind) else { continue };
                let validity = s.open(name, true).map_err(|e| self.imap_error(e))?;
                let uids: Vec<u32> = s.uid_search(criteria.clone()).map_err(|e| self.imap_error(e))?.into_iter().filter(|u| !found.iter().any(|(l, _)| l.kind == kind && l.uid == *u)).collect();
                for f in s.uid_fetch(&uids, WHOLE).map_err(|e| self.imap_error(e))? {
                    let id = MessageParser::default().parse_headers(f.full.as_deref().unwrap_or_default()).and_then(|m| m.message_id().map(normalize_message_id));
                    if let Some(id) = id
                        && !asked.contains(&id)
                        && !next.contains(&id)
                    {
                        next.push(id);
                    }
                    found.push((Loc { kind, validity, uid: f.uid }, f));
                }
            }
        }
        Ok(found)
    }

    fn to_message(&self, thread_id: &str, key: &str, loc: Loc, f: &Fetched) -> Option<(Meta, Message)> {
        let bytes = f.full.as_deref()?;
        let m = MessageParser::default().parse(bytes)?;
        let info = meta(loc, f, bytes);
        let html = m.html_bodies().find(|p| p.is_text_html()).and_then(|p| p.text_contents()).map(str::to_string).filter(|h| !h.trim().is_empty());
        let text = m
            .text_bodies()
            .find(|p| p.is_text() && !p.is_text_html())
            .and_then(|p| p.text_contents())
            .map(str::to_string)
            .filter(|t| !t.trim().is_empty())
            .or_else(|| html.as_deref().map(html_to_text));
        let attachments = m
            .attachments()
            .enumerate()
            .map(|(n, p)| {
                let disposition_attachment = p.content_disposition().is_some_and(|d| d.is_attachment());
                Attachment {
                    id: format!("{}:{}#{n}", self.name, loc.encode()),
                    filename: p.attachment_name().map(str::to_string).unwrap_or_else(|| format!("attachment-{}", n + 1)),
                    mime_type: p.content_type().map(|ct| format!("{}/{}", ct.ctype(), ct.subtype().unwrap_or("octet-stream")).to_ascii_lowercase()).unwrap_or_else(|| "application/octet-stream".into()),
                    size: p.contents().len() as i64,
                    inline: !disposition_attachment && (p.content_id().is_some() || p.content_disposition().is_some_and(|d| d.is_inline())),
                }
            })
            .collect();
        let message = Message {
            id: format!("{}:{key}/{}", self.name, loc.encode()),
            thread_id: thread_id.to_string(),
            outgoing: loc.kind == BoxKind::Sent || info.from.as_ref().is_some_and(|a| self.is_own(a)),
            from: info.from.clone().unwrap_or_default(),
            to: info.to.clone(),
            cc: info.cc.clone(),
            reply_to: addrs(m.reply_to()),
            subject: info.subject.clone(),
            date: info.date,
            text,
            html,
            message_id: m.message_id().map(|i| format!("<{i}>")),
            attachments,
            auth: None,
        };
        Some((info, message))
    }

    /// One message whole, by its place.
    fn fetch_one(&self, loc: Loc) -> Result<Vec<u8>> {
        self.with(|s, boxes| {
            self.open_at(s, boxes, loc, true)?;
            let f = s.uid_fetch(&[loc.uid], "(UID BODY.PEEK[])").map_err(|e| self.imap_error(e))?;
            f.into_iter().find_map(|f| f.full).ok_or_else(|| self.fail(ErrorKind::NotFound, "that message is gone"))
        })
    }

    /// Where a thread's messages are in these mailboxes, with what they say.
    fn thread_locs(&self, s: &mut Session, boxes: &Boxes, id: &str, kinds: &[BoxKind]) -> Result<Vec<(Loc, Meta)>> {
        let (_, root) = self.thread_ref(id)?;
        let found = self.find_thread(s, boxes, &root, kinds)?;
        Ok(found.into_iter().map(|(loc, f)| (loc, meta(loc, &f, f.full.as_deref().unwrap_or_default()))).collect())
    }

    /// Sends through iCloud's SMTP server, signed in with the whole address.
    fn smtp_send(&self, from: &str, recipients: &[String], message: &[u8]) -> Result<()> {
        let parse = |a: &str| a.parse::<lettre::Address>().map_err(|e| self.fail(ErrorKind::BadRequest, format!("{a} isn't an address iCloud can send to ({e})")));
        let to = recipients.iter().map(|r| parse(r)).collect::<Result<Vec<_>>>()?;
        let envelope = lettre::address::Envelope::new(Some(parse(from)?), to).map_err(|e| self.fail(ErrorKind::BadRequest, e.to_string()))?;
        let mut tls = TlsParameters::builder(self.smtp.0.clone());
        if let Some(path) = extra_ca() {
            let pem = std::fs::read(&path).map_err(|e| self.fail(ErrorKind::Config, format!("can't read {}: {e}", path.display())))?;
            tls = tls.add_root_certificate(Certificate::from_pem(&pem).map_err(|e| self.fail(ErrorKind::Config, format!("{}: {e}", path.display())))?);
        }
        let tls = tls.build_rustls().map_err(|e| self.fail(ErrorKind::Config, e.to_string()))?;
        let transport = lettre::SmtpTransport::builder_dangerous(self.smtp.0.clone())
            .port(self.smtp.1)
            .tls(Tls::Required(tls))
            .credentials(Credentials::new(self.email.clone(), self.password()?))
            .authentication(vec![Mechanism::Plain, Mechanism::Login])
            .timeout(Some(TIMEOUT))
            .build();
        transport.send_raw(&envelope, message).map(drop).map_err(|e| {
            let code = e.status().map(|c| c.to_string()).unwrap_or_default();
            if code == "535" || code == "534" {
                self.fail(ErrorKind::AccountAuth, format!("iCloud's mail server refused the app-specific password ({e}); {}", self.sign_in_again()))
            } else if e.is_permanent() {
                self.fail(ErrorKind::BadRequest, format!("iCloud wouldn't send the message: {e}"))
            } else {
                self.fail(ErrorKind::AccountUnavailable, format!("couldn't send through {} ({e}); offline?", self.smtp.0))
            }
        })
    }

    /// Makes sure a sent message is in Sent Messages: iCloud may file it there itself; if it
    /// hasn't after a moment, a copy is saved.
    fn file_sent(&self, message_id: &str, message: &[u8]) -> Result<()> {
        self.with(|s, boxes| {
            let present = |s: &mut Session| -> Result<bool> {
                s.open(&boxes.sent, true).map_err(|e| self.imap_error(e))?;
                Ok(!s.uid_search(header_term("Message-ID", message_id)).map_err(|e| self.imap_error(e))?.is_empty())
            };
            if present(s)? {
                return Ok(());
            }
            std::thread::sleep(SENT_COPY_WAIT);
            if present(s)? {
                return Ok(());
            }
            s.append(&boxes.sent, "\\Seen", message).map_err(|e| self.imap_error(e))
        })
    }

    /// Checks the password by signing in and reading the mailboxes; the account's addresses.
    pub fn verify(&self) -> Result<Vec<String>> {
        self.with(|_, _| Ok(()))?;
        Ok(self.own())
    }
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

    /// iCloud Mail has no browser sign-in: this opens the page that makes app-specific passwords
    /// and says how to save a new one.
    fn sign_in(&self) -> Result<()> {
        open_password_page();
        Err(self.fail(
            ErrorKind::AccountAuth,
            format!("make a new app-specific password in the page that opened (Sign-In and Security → App-Specific Passwords), then run `cloudmail account login {}` in a terminal", self.name),
        ))
    }

    fn status(&self) -> AccountStatus {
        let mut status = AccountStatus { name: self.name.clone(), provider: "icloud".into(), label: self.label().into(), ..Default::default() };
        match self.verify() {
            Ok(addresses) => {
                status.ok = true;
                status.addresses = addresses;
                status.detail = format!("signed in to {} with an app-specific password (kept in {})", self.imap.0, password_path(&self.name).display());
            }
            Err(e) => status.detail = e.message,
        }
        status
    }

    fn threads(&self, q: &ThreadQuery) -> Result<Vec<ThreadSummary>> {
        let folder = if q.folder.is_empty() { "inbox" } else { q.folder.as_str() };
        let kinds: &[BoxKind] = match folder {
            "inbox" => &[BoxKind::Inbox],
            "archive" => &[BoxKind::Archive],
            "sent" => &[BoxKind::Sent],
            "all" => &ALL_BOXES,
            _ => return Ok(Vec::new()),
        };
        let want = (q.limit.max(1) as usize * 2).min(400);
        let keep = |m: &Meta| q.before.is_none_or(|b| m.date < b) && q.since.is_none_or(|s| m.date > s);
        let metas = self.with(|s, boxes| {
            let mut all = Vec::new();
            for &kind in kinds {
                let mut keys = vec![raw(if q.unread && kind != BoxKind::Sent { "UNSEEN" } else { "ALL" })];
                keys.extend(text_search(q.q.as_deref().unwrap_or_default()));
                all.extend(self.scan(s, boxes, kind, criteria(keys), Page { want, floor: q.since }, keep)?);
            }
            Ok(all)
        })?;
        let mut list = self.threads_of(metas, (kinds.len() == 1).then_some(folder));
        list.retain(|t| !q.unread || t.unread);
        list.truncate(q.limit.max(1) as usize);
        Ok(list)
    }

    fn search(&self, query: &str, limit: u32) -> Result<Vec<ThreadSummary>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let want = (limit.max(1) as usize * 2).min(400);
        let metas = self.with(|s, boxes| {
            let mut all = Vec::new();
            for kind in ALL_BOXES {
                all.extend(self.scan(s, boxes, kind, criteria(text_search(query)), Page { want, floor: None }, |_| true)?);
            }
            Ok(all)
        })?;
        let mut list = self.threads_of(metas, None);
        list.truncate(limit.max(1) as usize);
        Ok(list)
    }

    fn thread(&self, id: &str, _html: bool) -> Result<ThreadDetail> {
        let (key, root) = self.thread_ref(id)?;
        let thread_id = format!("{}:{key}", self.name);
        let found = self.with(|s, boxes| self.find_thread(s, boxes, &root, &ALL_BOXES))?;
        let mut pairs: Vec<(Meta, Message)> = found.iter().filter_map(|(loc, f)| self.to_message(&thread_id, &key, *loc, f)).collect();
        if pairs.is_empty() {
            return Err(self.fail(ErrorKind::NotFound, format!("no iCloud Mail thread {id} (moved or deleted?)")));
        }
        pairs.sort_by_key(|(m, _)| (m.date, m.loc.uid));
        let mut seen = HashSet::new();
        pairs.retain(|(m, _)| m.message_id.is_empty() || seen.insert(m.message_id.clone()));
        let metas: Vec<Meta> = pairs.iter().map(|(m, _)| m.clone()).collect();
        let mut summary = self.summarize(&root, &metas, None);
        summary.id = thread_id.clone();
        self.message_ids.lock().unwrap().insert(thread_id, metas.iter().map(|m| m.message_id.clone()).filter(|m| !m.is_empty()).collect());
        Ok(ThreadDetail { thread: summary, messages: pairs.into_iter().map(|(_, m)| m).collect() })
    }

    fn move_thread(&self, id: &str, folder: &str) -> Result<()> {
        let (from, to) = match folder {
            "archive" => (BoxKind::Inbox, BoxKind::Archive),
            "inbox" => (BoxKind::Archive, BoxKind::Inbox),
            other => return Err(self.fail(ErrorKind::BadRequest, format!("iCloud Mail threads move between the Inbox and the Archive, not \"{other}\""))),
        };
        self.with(|s, boxes| {
            let mut boxes = boxes.clone();
            if boxes.archive.is_none() {
                if from == BoxKind::Archive {
                    return Ok(());
                }
                // Mail makes an Archive mailbox the first time it archives; so does this.
                s.create("Archive").map_err(|e| self.imap_error(e))?;
                boxes.archive = Some("Archive".into());
                *self.boxes.lock().unwrap() = Some(boxes.clone());
            }
            let uids: Vec<u32> = self.thread_locs(s, &boxes, id, &[from])?.iter().map(|(l, _)| l.uid).collect();
            if uids.is_empty() {
                return Ok(());
            }
            s.open(boxes.name(from).unwrap_or("INBOX"), false).map_err(|e| self.imap_error(e))?;
            s.uid_move(&uids, boxes.name(to).unwrap_or("INBOX")).map_err(|e| self.imap_error(e))
        })
    }

    fn set_unread(&self, id: &str, unread: bool) -> Result<()> {
        self.with(|s, boxes| {
            let locs = self.thread_locs(s, boxes, id, &[BoxKind::Inbox, BoxKind::Archive])?;
            let targets: Vec<Loc> = if unread {
                // Unread is the latest message you received, as in Mail.
                locs.iter().filter(|(_, m)| !m.from.as_ref().is_some_and(|a| self.is_own(a))).max_by_key(|(_, m)| m.date).map(|(l, _)| *l).into_iter().collect()
            } else {
                locs.iter().map(|(l, _)| *l).collect()
            };
            for kind in [BoxKind::Inbox, BoxKind::Archive] {
                let uids: Vec<u32> = targets.iter().filter(|l| l.kind == kind).map(|l| l.uid).collect();
                if uids.is_empty() {
                    continue;
                }
                s.open(boxes.name(kind).unwrap_or("INBOX"), false).map_err(|e| self.imap_error(e))?;
                s.uid_store(&uids, if unread { "-FLAGS.SILENT" } else { "+FLAGS.SILENT" }, "\\Seen").map_err(|e| self.imap_error(e))?;
            }
            Ok(())
        })
    }

    fn screener(&self) -> Result<Vec<PendingSender>> {
        Ok(Vec::new())
    }

    fn decide_sender(&self, id: &str, _status: &str) -> Result<i64> {
        Err(self.fail(ErrorKind::BadRequest, format!("iCloud Mail has no Screener, so there's no sender {id} to decide on")))
    }

    fn identities(&self) -> Result<Vec<Address>> {
        if self.email.is_empty() {
            return Err(self.fail(ErrorKind::Config, "no iCloud address is configured"));
        }
        Ok(self.own().into_iter().map(|email| Address { name: None, email }).collect())
    }

    fn send(&self, req: &SendRequest) -> Result<SendResponse> {
        let own = self.identities()?;
        let from = match req.from.as_deref().map(bare_email).filter(|f| !f.is_empty()) {
            Some(f) => own.iter().find(|a| a.email.eq_ignore_ascii_case(&f)).cloned().ok_or_else(|| {
                self.fail(ErrorKind::BadRequest, format!("{f} isn't one of your iCloud addresses (add it with `cloudmail account add icloud --alias {f}`)"))
            })?,
            None => own[0].clone(),
        };
        let recipients: Vec<String> = req.to.iter().chain(&req.cc).chain(&req.bcc).flat_map(|r| split_addresses(r)).map(|a| bare_email(&a)).filter(|a| !a.is_empty()).collect();
        if recipients.is_empty() {
            return Err(self.fail(ErrorKind::BadRequest, "the message has no recipients"));
        }
        let (key, threading) = match req.reply_to_message_id.as_deref() {
            Some(target) => {
                let (key, loc) = self.message_ref(target)?;
                let head = self.with(|s, boxes| {
                    self.open_at(s, boxes, loc, true)?;
                    let f = s.uid_fetch(&[loc.uid], "(UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID REFERENCES)])").map_err(|e| self.imap_error(e))?;
                    Ok(f.into_iter().find_map(|f| f.header).unwrap_or_default())
                })?;
                let parsed = MessageParser::default().parse_headers(&head);
                let in_reply_to = parsed.as_ref().and_then(|m| m.message_id()).map(|i| format!("<{i}>")).unwrap_or_default();
                let references = parsed.as_ref().map(|m| ids_of(m.references()).iter().map(|r| format!("<{r}>")).collect::<Vec<_>>().join(" ")).unwrap_or_default();
                (Some(key), Some(Threading { in_reply_to, references }))
            }
            None => (None, None),
        };
        let domain = from.email.rsplit_once('@').map(|(_, d)| d.to_string()).unwrap_or_else(|| "icloud.com".into());
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let message_id = format!("{nanos:x}.{:x}.cloudmail@{domain}", std::process::id());
        let date = chrono::Utc::now().to_rfc2822();
        // The copy you keep names its Bcc recipients; the one that goes out mustn't.
        let kept = format!("Message-ID: <{message_id}>\r\n{}", build_message(&from, req, threading.as_ref(), &date));
        let outgoing = format!("Message-ID: <{message_id}>\r\n{}", build_message(&from, &SendRequest { bcc: Vec::new(), ..req.clone() }, threading.as_ref(), &date));
        self.smtp_send(&from.email, &recipients, outgoing.as_bytes())?;
        let warning = self.file_sent(&message_id, kept.as_bytes()).err().map(|e| format!("sent, but no copy could be saved in Sent Messages: {}", e.message));
        let key = key.unwrap_or_else(|| thread_key(&normalize_message_id(&message_id)));
        Ok(SendResponse { thread_id: Some(format!("{}:{key}", self.name)), message: None, warning })
    }

    fn attachment_limit(&self) -> u64 {
        crate::attach::ICLOUD_LIMIT
    }

    fn download_attachment(&self, id: &str) -> Result<Download> {
        let local = self.local(id)?;
        let (loc, n) = local
            .split_once('#')
            .and_then(|(l, n)| Some((Loc::parse(l)?, n.parse::<usize>().ok()?)))
            .ok_or_else(|| self.fail(ErrorKind::NotFound, format!("{id} isn't an iCloud Mail attachment ID")))?;
        let bytes = self.fetch_one(loc)?;
        let m = MessageParser::default().parse(&bytes).ok_or_else(|| self.fail(ErrorKind::AccountUnavailable, "that message can't be read"))?;
        let part = m.attachments().nth(n).ok_or_else(|| self.fail(ErrorKind::NotFound, format!("no attachment {id}")))?;
        Ok(Download {
            bytes: part.contents().to_vec(),
            content_type: part.content_type().map(|ct| format!("{}/{}", ct.ctype(), ct.subtype().unwrap_or("octet-stream"))),
            filename: part.attachment_name().map(str::to_string),
        })
    }

    fn raw_message(&self, id: &str) -> Result<Vec<u8>> {
        let (_, loc) = self.message_ref(id)?;
        self.fetch_one(loc)
    }

    fn message_ids(&self, thread_id: &str) -> Vec<String> {
        self.message_ids.lock().unwrap().get(thread_id).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(uid: u32, date: i64, id: &str, irt: &[&str], refs: &[&str]) -> Meta {
        Meta {
            loc: Loc { kind: BoxKind::Inbox, validity: 1, uid },
            seen: true,
            date,
            from: None,
            to: Vec::new(),
            cc: Vec::new(),
            subject: String::new(),
            message_id: id.into(),
            in_reply_to: irt.iter().map(|s| s.to_string()).collect(),
            references: refs.iter().map(|s| s.to_string()).collect(),
            attachments: false,
            snippet: String::new(),
        }
    }

    #[test]
    fn replies_join_their_conversation() {
        let groups = group(vec![
            m(1, 10, "a@x", &[], &[]),
            m(2, 20, "b@x", &["a@x"], &["a@x"]),
            // Only In-Reply-To, naming the middle message.
            m(3, 30, "c@x", &["b@x"], &[]),
            m(4, 15, "other@x", &[], &[]),
            // Its first message isn't here, but it names it.
            m(5, 40, "e@x", &["d@x"], &["d@x"]),
        ]);
        let shape: Vec<(String, Vec<u32>)> = groups.iter().map(|(k, l)| (k.clone(), l.iter().map(|m| m.loc.uid).collect())).collect();
        assert_eq!(shape, vec![("d@x".into(), vec![5]), ("a@x".into(), vec![1, 2, 3]), ("other@x".into(), vec![4])]);
    }

    #[test]
    fn a_message_without_an_id_is_its_own_thread() {
        let groups = group(vec![m(7, 1, "", &[], &[]), m(8, 2, "", &[], &[])]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "uid:i.1.8");
    }

    #[test]
    fn ids_round_trip() {
        let key = thread_key("CAF+x/y=z@mail.example.com");
        assert!(key.starts_with('t') && !key.contains(['/', '+', '=', ':', '#']), "{key}");
        assert_eq!(root_of(&key).as_deref(), Some("CAF+x/y=z@mail.example.com"));
        let loc = Loc { kind: BoxKind::Sent, validity: 77, uid: 9 };
        assert_eq!(Loc::parse(&loc.encode()), Some(loc));
        assert_eq!(Loc::parse("x.1.2"), None);
        let ic = Icloud::new("icloud", &AccountConfig { email: Some("me@icloud.com".into()), ..Default::default() });
        let (k, l) = ic.message_ref(&format!("icloud:{key}/s.77.9")).unwrap();
        assert_eq!((k.as_str(), l), (key.as_str(), loc));
        assert!(ic.owns(&format!("icloud:{key}")) && !ic.owns("gmail:abc"));
        assert_eq!(ic.thread_ref(&format!("icloud:{key}/i.1.2")).unwrap().1, "CAF+x/y=z@mail.example.com");
    }

    #[test]
    fn mailboxes_come_from_special_use_or_names() {
        let mb = |name: &str, sent: bool, archive: bool| imap::Mailbox { name: name.into(), sent, archive, selectable: true };
        let b = Boxes::from_list(&[mb("INBOX", false, false), mb("Sent Messages", false, false), mb("Archive", false, false)]);
        assert_eq!((b.sent.as_str(), b.archive.as_deref()), ("Sent Messages", Some("Archive")));
        let b = Boxes::from_list(&[mb("INBOX", false, false), mb("Gesendet", true, false), mb("Alt", false, true)]);
        assert_eq!((b.sent.as_str(), b.archive.as_deref()), ("Gesendet", Some("Alt")));
        let b = Boxes::from_list(&[mb("INBOX", false, false)]);
        assert_eq!((b.sent.as_str(), b.archive), ("Sent Messages", None));
    }

    #[test]
    fn headers_become_metadata() {
        let raw = b"From: \"Ann, Example\" <ann@example.com>\r\nTo: me@icloud.com\r\nSubject: =?UTF-8?B?Q2Fmw6k=?=\r\nMessage-ID: <A1@Example.com>\r\nIn-Reply-To: <r0@x>\r\nReferences: <r0@x> <r1@x>\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nHello there\r\n--b--\r\n";
        let f = Fetched { uid: 3, flags: vec!["\\Seen".into()], internal_date: Some("02-Oct-2026 09:15:00 +0000".into()), ..Default::default() };
        let x = meta(Loc { kind: BoxKind::Inbox, validity: 1, uid: 3 }, &f, raw);
        assert_eq!(x.from.as_ref().unwrap().name.as_deref(), Some("Ann, Example"));
        assert_eq!(x.subject, "Café");
        assert_eq!(x.message_id, "a1@example.com");
        assert_eq!(x.references, ["r0@x", "r1@x"]);
        assert_eq!(x.root(), "r0@x");
        assert!(x.attachments && x.seen);
        assert_eq!(x.date, 1790932500000);
        assert_eq!(x.snippet, "Hello there");
    }

    #[test]
    fn searches_name_their_charset_first() {
        let words = |args: Vec<Arg>| -> String {
            args.iter()
                .map(|a| match a {
                    Arg::Raw(r) => r.clone(),
                    Arg::Str(s) => format!("\"{}\"", String::from_utf8_lossy(s)),
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        let mut keys = vec![raw("UNSEEN")];
        keys.extend(text_search("café"));
        assert_eq!(words(criteria(keys)), "CHARSET UTF-8 UNSEEN TEXT \"café\"");
        assert_eq!(words(criteria(text_search(" lunch "))), "TEXT \"lunch\"");
        assert!(text_search("  ").is_empty());
    }

    #[test]
    fn searches_chain_with_or() {
        let args = any_of(vec![header_term("Message-ID", "a"), header_term("References", "a"), header_term("In-Reply-To", "a")]);
        let words: Vec<String> = args
            .iter()
            .map(|a| match a {
                Arg::Raw(r) => r.clone(),
                Arg::Str(s) => format!("\"{}\"", String::from_utf8_lossy(s)),
            })
            .collect();
        assert_eq!(words.join(" "), "OR HEADER Message-ID \"a\" OR HEADER References \"a\" HEADER In-Reply-To \"a\"");
    }

    #[test]
    fn replies_lose_their_prefixes_for_the_thread_subject() {
        assert_eq!(original_subject("RE: Fwd:  Lunch?"), "Lunch?");
        assert_eq!(original_subject("Rebate"), "Rebate");
        assert_eq!(original_subject("Re: Café"), "Café");
    }

    #[test]
    fn addresses_that_sign_in() {
        assert!(is_icloud_address("me@icloud.com") && is_icloud_address("Me@Mac.com"));
        assert!(!is_icloud_address("me@example.com") && !is_icloud_address("@icloud.com"));
    }
}
