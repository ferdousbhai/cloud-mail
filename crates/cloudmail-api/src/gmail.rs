//! Gmail through Google's Workspace CLI `gws` (github.com/googleworkspace/cli): every call is a
//! raw Gmail API request, `gws gmail users … --params '<json>'`, whose JSON answer is mapped into
//! cloudmail's types. gws owns the tokens; cloudmail never sees one.
//!
//! Isolation: gws runs with its config directory set to cloudmail's own
//! (`~/.config/cloudmail/gws/<account>`), with its encryption key in a file there rather than in
//! the OS keyring (whose one "gws-cli" entry a gws of your own also uses), and with no way to fall
//! back to Application Default Credentials. So signing Cloudmail in to Gmail never touches a gws
//! setup of your own, and yours never answers for Cloudmail.
//!
//! Sign-in: `gws auth login --scopes <gmail.modify>` with Cloudmail's built-in OAuth client (see
//! `GOOGLE_CLIENT_ID`), so there is nothing to set up in Google Cloud: one browser sign-in.
//!
//! IDs: a thread is `gmail:<threadId>`, a message `gmail:<threadId>/<messageId>`, an attachment
//! `gmail:<messageId>:<partId>` (Gmail's own attachment IDs change on every read).
//!
//! Folders: the Inbox is Gmail's INBOX label; archiving removes it (Gmail's own archive) and
//! "move to Inbox" adds it back; the Archive lists threads with mail outside the Inbox; Sent is the
//! SENT label; unread is the UNREAD label. Gmail has no Screener, so your worker's decides (see
//! `unified.rs`).

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_PAD_INDIFFERENT};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::client::ThreadQuery;
use crate::config::AccountConfig;
use crate::error::{Error, ErrorKind, Result};
use crate::provider::{AccountStatus, PrivateDir, Provider, Run, run_command};
use crate::text::{bare_email, html_to_text, split_addresses};
use crate::types::*;

/// Cloudmail's own Google OAuth client, a "Desktop app" client in Cloudmail's Google Cloud project
/// with the Gmail API enabled, used for the one browser sign-in. Google treats a desktop client's
/// secret as public, so it ships in the binary. CLOUDMAIL_GOOGLE_CLIENT_ID / _SECRET, or `client_id` /
/// `client_secret` under `[accounts.gmail]` in config.toml, use another client instead.
pub const GOOGLE_CLIENT_ID: &str = "858534886670-37fmq8bl312beufkc420h9nvtjrb4l0d.apps.googleusercontent.com";
pub const GOOGLE_CLIENT_SECRET: &str = "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";

pub const COMMAND_ENV: &str = "CLOUDMAIL_GWS_COMMAND";
pub const CLIENT_ID_ENV: &str = "CLOUDMAIL_GOOGLE_CLIENT_ID";
pub const CLIENT_SECRET_ENV: &str = "CLOUDMAIL_GOOGLE_CLIENT_SECRET";
/// The program that opens the sign-in link (default: xdg-open, or open on macOS).
pub const BROWSER_ENV: &str = "CLOUDMAIL_BROWSER";
/// Read, label, archive and send; nothing else in your Google account.
pub const SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";
/// Google's Workspace CLI on npm; `cloudmail account add gmail` installs it when it's missing.
pub const PACKAGE: &str = "@googleworkspace/cli";
pub const INSTALL_HINT: &str = "install Google's Workspace CLI: `npm install -g @googleworkspace/cli` (or a release binary from https://github.com/googleworkspace/cli/releases), or pass --command <path>";

const TIMEOUT: Duration = Duration::from_secs(90);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);
/// The most threads one listing asks Gmail for; each is one more `gws` run (cached by historyId).
const LIST_LIMIT: u32 = 100;
/// `gws` runs at a time when fetching a listing's threads.
const PARALLEL: usize = 8;
const META_HEADERS: &[&str] = &["From", "To", "Cc", "Subject", "Date", "Message-ID", "Delivered-To", "Content-Type"];

/// Threads kept on disk, so a CLI run fetches only what changed (each `threads.get` costs 10
/// of Gmail's 15,000 quota units a minute; the Screener lists up to 200 threads).
const CACHE_FILE: &str = "threads.json";
const CACHE_KEEP: usize = 2000;

/// A thread as last listed, kept while its historyId stays the same. Any change to a thread
/// (labels, read state, a new message) gives it a new historyId, so a kept entry is never stale.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Cached {
    history: String,
    summary: ThreadSummary,
    message_ids: Vec<String>,
    /// Who your sent messages in it went to.
    recipients: Vec<Address>,
}

pub struct Gmail {
    name: String,
    command: String,
    dir: PathBuf,
    client: Option<(String, String)>,
    cache: Mutex<HashMap<String, Cached>>,
    own: Mutex<Option<Vec<Address>>>,
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().trim().to_string()
}

fn nonempty(s: String) -> Option<String> {
    Some(s).filter(|s| !s.is_empty())
}

fn nonblank(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// A header's value from a message or part (`payload.headers`, or a part's own `headers`).
fn header(part: &Value, name: &str) -> String {
    part["headers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|h| h["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name)))
        .map(|h| text(&h["value"]))
        .unwrap_or_default()
}

fn labels(m: &Value) -> Vec<&str> {
    m["labelIds"].as_array().into_iter().flatten().filter_map(Value::as_str).collect()
}

fn has_label(m: &Value, label: &str) -> bool {
    labels(m).contains(&label)
}

fn internal_date(m: &Value) -> i64 {
    m["internalDate"].as_str().and_then(|s| s.parse().ok()).or_else(|| m["internalDate"].as_i64()).unwrap_or(0)
}

/// `<Id@Host>` → `id@host`, for comparing Message-IDs.
pub fn normalize_message_id(id: &str) -> String {
    id.trim().trim_start_matches('<').trim_end_matches('>').trim().to_ascii_lowercase()
}

/// One address from `Name <a@b>`, `"Last, First" <a@b>` or `a@b`.
pub fn address_from(s: &str) -> Address {
    let s = s.trim();
    match (s.rfind('<'), s.rfind('>')) {
        (Some(l), Some(r)) if l < r => {
            let name = s[..l].trim().trim_matches('"').replace("\\\"", "\"").trim().to_string();
            Address { name: nonempty(name), email: s[l + 1..r].trim().to_string() }
        }
        _ => Address { name: None, email: s.trim_matches(|c| c == '<' || c == '>' || c == '"').to_string() },
    }
}

/// Every address in a header such as To or Cc.
pub fn addresses(header: &str) -> Vec<Address> {
    split_addresses(header).iter().map(|a| address_from(a)).filter(|a| a.email.contains('@')).collect()
}

fn decode_b64(data: &str) -> Option<Vec<u8>> {
    URL_SAFE_PAD_INDIFFERENT.decode(data.trim()).ok()
}

/// Decoded body text in the part's charset (UTF-8 unless it says Latin-1 or similar).
fn decode_text(bytes: &[u8], content_type: &str) -> String {
    let ct = content_type.to_ascii_lowercase();
    let charset = ct
        .split(';')
        .filter_map(|p| p.trim().strip_prefix("charset="))
        .next()
        .unwrap_or("utf-8")
        .trim_matches('"')
        .to_string();
    if matches!(charset.as_str(), "iso-8859-1" | "latin1" | "latin-1" | "windows-1252" | "cp1252" | "iso-8859-15") {
        bytes.iter().map(|&b| b as char).collect()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// Header text as RFC 2047 when it isn't plain ASCII; line breaks are dropped either way.
fn encode_words(s: &str) -> String {
    let s: String = s.chars().filter(|c| *c != '\r' && *c != '\n').collect();
    if s.is_ascii() {
        return s;
    }
    let mut words = Vec::new();
    let mut chunk = String::new();
    for c in s.chars() {
        if chunk.len() + c.len_utf8() > 45 {
            words.push(format!("=?UTF-8?B?{}?=", STANDARD.encode(chunk.as_bytes())));
            chunk.clear();
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        words.push(format!("=?UTF-8?B?{}?=", STANDARD.encode(chunk.as_bytes())));
    }
    words.join("\r\n ")
}

/// `Name <a@b>` for a header, the name quoted or encoded as needed.
pub(crate) fn format_address(a: &Address) -> String {
    let email: String = a.email.chars().filter(|c| !matches!(c, '\r' | '\n' | '<' | '>' | ',' | '"')).collect();
    match a.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        None => email,
        Some(n) if !n.is_ascii() => format!("{} <{email}>", encode_words(n)),
        Some(n) if n.chars().any(|c| "()<>[]:;@\\,.\"".contains(c)) => {
            format!("\"{}\" <{email}>", n.replace(['\r', '\n'], "").replace('\\', "\\\\").replace('"', "\\\""))
        }
        Some(n) => format!("{} <{email}>", n.replace(['\r', '\n'], "")),
    }
}

/// What a sent message threads onto: the original's Message-ID and References.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Threading {
    pub in_reply_to: String,
    pub references: String,
}

/// Base64 in 76-character lines, as a MIME part carries it.
fn base64_lines(bytes: &[u8]) -> String {
    let encoded = STANDARD.encode(bytes);
    encoded.as_bytes().chunks(76).map(|c| std::str::from_utf8(c).unwrap_or("")).collect::<Vec<_>>().join("\r\n")
}

/// A file name as Content-Type `name` and Content-Disposition `filename` parameters: quoted when
/// ASCII, else RFC 2231 (`filename*=UTF-8''…`) with an RFC 2047 `name` for older readers.
fn filename_params(filename: &str) -> (String, String) {
    let name = crate::attach::safe_filename(filename);
    if name.is_ascii() {
        let quoted = format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""));
        return (format!("name={quoted}"), format!("filename={quoted}"));
    }
    let encoded: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    (format!("name=\"=?UTF-8?B?{}?=\"", STANDARD.encode(name.as_bytes())), format!("filename*=UTF-8''{encoded}"))
}

/// A MIME type fit for a header, else `application/octet-stream`.
fn clean_mime_type(t: &str) -> String {
    let t = t.trim().to_ascii_lowercase();
    let ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&b));
    match t.split_once('/') {
        Some((a, b)) if ok(a) && ok(b) => t,
        _ => "application/octet-stream".into(),
    }
}

/// An RFC 5322 message for `users.messages.send`: plain text, or multipart/mixed with the text
/// first and each attachment after it.
pub fn build_message(from: &Address, req: &SendRequest, threading: Option<&Threading>, date: &str) -> String {
    let list = |items: &[String]| {
        items
            .iter()
            .flat_map(|i| split_addresses(i))
            .map(|a| format_address(&address_from(&a)))
            .filter(|a| a.contains('@'))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut head = vec![format!("From: {}", format_address(from))];
    for (name, items) in [("To", &req.to), ("Cc", &req.cc), ("Bcc", &req.bcc)] {
        let v = list(items);
        if !v.is_empty() {
            head.push(format!("{name}: {v}"));
        }
    }
    head.push(format!("Subject: {}", encode_words(&req.subject)));
    head.push(format!("Date: {date}"));
    if let Some(t) = threading.filter(|t| !t.in_reply_to.is_empty()) {
        let clean = |s: &str| s.replace(['\r', '\n'], " ");
        head.push(format!("In-Reply-To: {}", clean(&t.in_reply_to)));
        let mut refs: Vec<&str> = t.references.split_whitespace().collect();
        refs.push(t.in_reply_to.trim());
        let refs = refs[refs.len().saturating_sub(20)..].join(" ");
        head.push(format!("References: {}", clean(&refs)));
    }
    head.push("MIME-Version: 1.0".into());
    let text = "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64";
    let body = base64_lines(req.text.replace("\r\n", "\n").replace('\n', "\r\n").as_bytes());
    if req.attachments.is_empty() {
        return format!("{}\r\n{text}\r\n\r\n{body}\r\n", head.join("\r\n"));
    }
    // Every part is base64, which never contains "=_", so the boundary can't occur inside one.
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let boundary = format!("=_cloudmail_{nanos:x}");
    head.push(format!("Content-Type: multipart/mixed; boundary=\"{boundary}\""));
    let mut out = format!("{}\r\n\r\n--{boundary}\r\n{text}\r\n\r\n{body}\r\n", head.join("\r\n"));
    for a in &req.attachments {
        let (name, filename) = filename_params(&a.filename);
        out.push_str(&format!(
            "--{boundary}\r\nContent-Type: {}; {name}\r\nContent-Disposition: attachment; {filename}\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n",
            clean_mime_type(&a.mime_type),
            base64_lines(&a.content)
        ));
    }
    out.push_str(&format!("--{boundary}--\r\n"));
    out
}

/// The client to sign in with: the environment, then the config, then cloudmail's built-in one.
fn client_credentials(cfg: &AccountConfig) -> Option<(String, String)> {
    let env = |k| nonblank(std::env::var(k).ok());
    let builtin = (nonblank(Some(GOOGLE_CLIENT_ID.to_string())), nonblank(Some(GOOGLE_CLIENT_SECRET.to_string())));
    [
        (env(CLIENT_ID_ENV), env(CLIENT_SECRET_ENV)),
        (nonblank(cfg.client_id.clone()), nonblank(cfg.client_secret.clone())),
        builtin,
    ]
    .into_iter()
    .find_map(|(id, secret)| Some((id?, secret?)))
}

/// cloudmail's own gws directory for an account, beside its config file.
/// Where `cloudmail account add gmail` installs gws when there's none: `<data dir>/cloudmail/gws`
/// (`~/.local/share/cloudmail/gws`), an npm prefix, so no root and no PATH change.
pub fn bundled_prefix() -> PathBuf {
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("cloudmail").join("gws")
}

/// The gws in [`bundled_prefix`], whether or not it's there.
pub fn bundled_command() -> PathBuf {
    bundled_prefix().join("bin").join("gws")
}

/// The thread cache a previous run left (empty when there's none or it's unreadable).
fn load_cache(dir: &Path) -> HashMap<String, Cached> {
    std::fs::read(dir.join(CACHE_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn gws_dir(name: &str) -> PathBuf {
    crate::config::path().parent().map(Path::to_path_buf).unwrap_or_default().join("gws").join(name)
}

/// The sign-in link in a line gws printed, if any.
fn sign_in_url(line: &str) -> Option<String> {
    line.split_whitespace().find(|w| w.starts_with("https://accounts.google.com/")).map(str::to_string)
}

pub(crate) fn open_browser(url: &str) {
    let program = nonblank(std::env::var(BROWSER_ENV).ok())
        .unwrap_or_else(|| if cfg!(target_os = "macos") { "open".into() } else { "xdg-open".into() });
    let _ = Command::new(program).arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

impl Gmail {
    pub fn new(name: &str, cfg: &AccountConfig) -> Self {
        let command = nonblank(std::env::var(COMMAND_ENV).ok())
            .or_else(|| nonblank(cfg.command.clone()))
            .or_else(|| Some(bundled_command()).filter(|b| b.is_file()).map(|b| b.display().to_string()))
            .unwrap_or_else(|| "gws".into());
        Self {
            name: name.to_string(),
            command,
            dir: gws_dir(name),
            client: client_credentials(cfg),
            cache: Mutex::new(load_cache(&gws_dir(name))),
            own: Default::default(),
        }
    }

    /// Writes the cache for the next run: the most recent threads, privately, atomically.
    fn save_cache(&self) {
        if !self.dir.is_dir() {
            return;
        }
        let mut entries: Vec<(String, Cached)> =
            self.cache.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        entries.sort_by_key(|(_, c)| std::cmp::Reverse(c.summary.last_at));
        entries.truncate(CACHE_KEEP);
        let map: HashMap<String, Cached> = entries.into_iter().collect();
        let Ok(body) = serde_json::to_vec(&map) else { return };
        let tmp = self.dir.join(format!(".{CACHE_FILE}.{}", std::process::id()));
        let written = (|| -> std::io::Result<()> {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            f.write_all(&body)?;
            std::fs::rename(&tmp, self.dir.join(CACHE_FILE))
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    /// Where gws keeps this account's sign-in (cloudmail's own, not ~/.config/gws).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether a Google OAuth client is available to sign in with.
    pub fn client_configured(&self) -> bool {
        self.client.is_some()
    }

    /// Signed in on this computer: gws saved credentials in cloudmail's directory.
    pub fn signed_in(&self) -> bool {
        self.dir.join("credentials.enc").is_file()
    }

    fn fail(&self, kind: ErrorKind, message: impl AsRef<str>) -> Error {
        Error::new(kind, format!("Gmail: {}", message.as_ref()))
    }

    fn not_signed_in(&self) -> Error {
        self.fail(ErrorKind::AccountAuth, format!("not signed in; run `cloudmail account login {}`", self.name))
    }

    fn missing(&self) -> Error {
        self.fail(
            ErrorKind::AccountUnavailable,
            format!("Google's Workspace CLI isn't installed (no `{}` on PATH); {INSTALL_HINT}", self.command),
        )
    }

    /// `gws`, confined to cloudmail's own directory and sign-in.
    fn gws(&self) -> Command {
        let mut cmd = Command::new(&self.command);
        // gws also reads a .env from its working directory.
        cmd.current_dir(if self.dir.is_dir() { self.dir.clone() } else { std::env::temp_dir() })
            .env("GOOGLE_WORKSPACE_CLI_CONFIG_DIR", &self.dir)
            .env("GOOGLE_WORKSPACE_CLI_KEYRING_BACKEND", "file")
            .env("GOOGLE_APPLICATION_CREDENTIALS", self.dir.join("no-application-default-credentials.json"))
            .env_remove("GOOGLE_WORKSPACE_CLI_TOKEN")
            .env_remove("GOOGLE_WORKSPACE_CLI_CREDENTIALS_FILE");
        match &self.client {
            Some((id, secret)) => {
                cmd.env("GOOGLE_WORKSPACE_CLI_CLIENT_ID", id).env("GOOGLE_WORKSPACE_CLI_CLIENT_SECRET", secret)
            }
            None => cmd.env_remove("GOOGLE_WORKSPACE_CLI_CLIENT_ID").env_remove("GOOGLE_WORKSPACE_CLI_CLIENT_SECRET"),
        };
        cmd
    }

    /// Whether the gws this account would run isn't there at all.
    pub fn is_missing(&self) -> bool {
        matches!(run_command(self.gws().arg("--version"), None, TIMEOUT), Run::Missing)
    }

    /// The installed gws's version line, or an error when it isn't installed.
    pub fn version(&self) -> Result<String> {
        match run_command(self.gws().arg("--version"), None, TIMEOUT) {
            Run::Done { status, stdout, .. } if status.success() => {
                Ok(String::from_utf8_lossy(&stdout).lines().next().unwrap_or("").trim().to_string())
            }
            Run::Done { status, stderr, .. } => Err(self.fail(
                ErrorKind::AccountUnavailable,
                format!("`{} --version` failed ({status}): {stderr}", self.command),
            )),
            Run::Missing => Err(self.missing()),
            Run::TimedOut => Err(self.fail(ErrorKind::AccountUnavailable, "`gws --version` didn't answer")),
            Run::Failed(e) => {
                Err(self.fail(ErrorKind::AccountUnavailable, format!("could not run {}: {e}", self.command)))
            }
        }
    }

    /// The error for "this build has no Google client to sign in with".
    pub fn no_client_error() -> Error {
        Error::new(
            ErrorKind::Config,
            format!(
                "Cloudmail's Google sign-in isn't configured in this build; set {CLIENT_ID_ENV} and {CLIENT_SECRET_ENV} (a Desktop-app OAuth client from Google Cloud Console with the Gmail API enabled), or client_id and client_secret under [accounts.gmail] in config.toml"
            ),
        )
    }

    /// Google's browser sign-in through `gws auth login`, for Gmail only: gws prints a sign-in link
    /// (opened here in the browser) and waits for Google to send the browser back to it. Returns
    /// the signed-in address when gws reports it.
    pub fn login(&self) -> Result<Option<String>> {
        if self.client.is_none() {
            return Err(Self::no_client_error());
        }
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&self.dir).map_err(|e| {
            self.fail(ErrorKind::AccountUnavailable, format!("could not create {}: {e}", self.dir.display()))
        })?;
        let mut cmd = self.gws();
        cmd.current_dir(&self.dir)
            .args(["auth", "login", "--scopes", SCOPE])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                self.missing()
            } else {
                self.fail(ErrorKind::AccountUnavailable, format!("could not run {}: {e}", self.command))
            }
        })?;
        let opened = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // gws prints the link on stderr (or, behind a proxy, on stdout); both are shown as they come.
        let watch = |pipe: Box<dyn Read + Send>, echo: bool| {
            let opened = opened.clone();
            std::thread::spawn(move || {
                let mut all = String::new();
                for line in BufReader::new(pipe).lines().map_while(std::result::Result::ok) {
                    if let Some(url) = sign_in_url(&line)
                        && !opened.swap(true, std::sync::atomic::Ordering::SeqCst)
                    {
                        open_browser(&url);
                    }
                    if echo || sign_in_url(&line).is_some() || line.starts_with("Open this URL") {
                        eprintln!("{line}");
                    }
                    all.push_str(&line);
                    all.push('\n');
                }
                all
            })
        };
        let out = child.stdout.take().map(|p| watch(Box::new(p), false));
        let err = child.stderr.take().map(|p| watch(Box::new(p), true));
        let deadline = Instant::now() + LOGIN_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(
                        self.fail(ErrorKind::AccountAuth, "the Google sign-in wasn't finished within 10 minutes")
                    );
                }
                Err(e) => return Err(self.fail(ErrorKind::AccountUnavailable, e.to_string())),
            }
        };
        let stdout = out.and_then(|h| h.join().ok()).unwrap_or_default();
        let stderr = err.and_then(|h| h.join().ok()).unwrap_or_default();
        let json: Value = stdout.find('{').and_then(|i| serde_json::from_str(&stdout[i..]).ok()).unwrap_or(Value::Null);
        if !status.success() || !self.signed_in() {
            let message = nonempty(text(&json["error"]["message"]))
                .or_else(|| stderr.lines().rev().find(|l| !l.trim().is_empty()).map(str::to_string))
                .unwrap_or_else(|| format!("`gws auth login` failed ({status})"));
            return Err(self.fail(
                ErrorKind::AccountAuth,
                format!("the Google sign-in didn't finish: {}", message.lines().next().unwrap_or("")),
            ));
        }
        // Maybe another Google account now: nothing listed before it carries over.
        self.cache.lock().unwrap().clear();
        let _ = std::fs::remove_file(self.dir.join(CACHE_FILE));
        *self.own.lock().unwrap() = None;
        Ok(nonempty(text(&json["account"])).filter(|a| a.contains('@')))
    }

    /// Signs cloudmail out of Gmail on this computer: removes cloudmail's gws directory (its
    /// saved sign-in). Google keeps the grant until you remove it from your Google Account.
    pub fn forget(&self) -> std::io::Result<bool> {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// `gws gmail users <method…> --params <params> [--json <body>]`, returning the API's JSON.
    fn call(&self, method: &[&str], params: Value, body: Option<&Value>) -> Result<Value> {
        self.call_with(method, params, body, None)
    }

    /// `call`, uploading a message (`--upload <path> --upload-content-type message/rfc822`): gws
    /// only uploads files under its working directory, `self.dir`, so `upload` is relative to it.
    fn call_with(&self, method: &[&str], params: Value, body: Option<&Value>, upload: Option<&Path>) -> Result<Value> {
        if !self.signed_in() {
            return Err(self.not_signed_in());
        }
        let what = method.join(" ");
        let mut cmd = self.gws();
        cmd.args(["gmail", "users"]).args(method).arg("--params").arg(params.to_string());
        if let Some(b) = body {
            cmd.arg("--json").arg(b.to_string());
        }
        if let Some(path) = upload {
            cmd.arg("--upload").arg(path).arg("--upload-content-type").arg("message/rfc822");
        }
        let (status, stdout, stderr) = match run_command(&mut cmd, None, TIMEOUT) {
            Run::Done { status, stdout, stderr } => (status, stdout, stderr),
            Run::Missing => return Err(self.missing()),
            Run::TimedOut => {
                return Err(self.fail(
                    ErrorKind::AccountUnavailable,
                    format!("`gws gmail users {what}` took longer than {}s", TIMEOUT.as_secs()),
                ));
            }
            Run::Failed(e) => {
                return Err(self.fail(ErrorKind::AccountUnavailable, format!("could not run {}: {e}", self.command)));
            }
        };
        let parsed: std::result::Result<Value, _> = serde_json::from_slice(&stdout);
        if status.success() {
            if String::from_utf8_lossy(&stdout).trim().is_empty() {
                return Ok(Value::Null);
            }
            return parsed.map_err(|e| {
                self.fail(
                    ErrorKind::AccountUnavailable,
                    format!("`gws gmail users {what}` answered something that isn't JSON ({e})"),
                )
            });
        }
        Err(self.gws_error(status.code(), &parsed.unwrap_or(Value::Null), &stderr, &what))
    }

    /// gws's exit statuses: 1 API error (Google's HTTP status in the JSON), 2 auth, 3 validation,
    /// 4 discovery (Google unreachable), 5 other; the error JSON goes to stdout.
    fn gws_error(&self, code: Option<i32>, out: &Value, stderr: &str, what: &str) -> Error {
        let err = &out["error"];
        let mut message = text(&err["message"]);
        if message.is_empty() {
            message = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        }
        if message.is_empty() {
            message = format!("`gws gmail users {what}` failed");
        }
        let mut message = message.lines().next().unwrap_or("").to_string();
        if message.chars().count() > 300 {
            message = format!("{}…", message.chars().take(300).collect::<String>());
        }
        let http = err["code"].as_i64().unwrap_or(0);
        let reason = text(&err["reason"]);
        let again = format!("run `cloudmail account login {}` to sign in again", self.name);
        match (code, http) {
            (Some(2), _) | (Some(1), 401) => self.fail(
                ErrorKind::AccountAuth,
                format!("the Google sign-in expired or was revoked ({message}); {again}"),
            ),
            (Some(1), 403)
                if reason.contains("insufficient") || message.to_ascii_lowercase().contains("insufficient") =>
            {
                self.fail(
                    ErrorKind::AccountAuth,
                    format!("Cloudmail isn't allowed to do that in Gmail ({message}); {again}"),
                )
            }
            (Some(1), 404) => self.fail(ErrorKind::NotFound, message),
            (Some(1), 400) => self.fail(ErrorKind::BadRequest, message),
            (Some(1), 429) => self
                .fail(ErrorKind::AccountUnavailable, format!("Gmail is rate limiting ({message}); try again shortly")),
            (Some(4), _) => self.fail(ErrorKind::AccountUnavailable, format!("couldn't reach Google ({message})")),
            (Some(3), _) => self
                .fail(ErrorKind::AccountUnavailable, format!("gws rejected `{what}` ({message}); is gws up to date?")),
            _ => self.fail(ErrorKind::AccountUnavailable, message),
        }
    }

    fn shape_error(&self, what: &str) -> Error {
        self.fail(
            ErrorKind::AccountUnavailable,
            format!("unexpected answer to `gws gmail users {what}` (a newer gws?)"),
        )
    }

    fn local<'a>(&self, id: &'a str) -> Result<&'a str> {
        id.strip_prefix(self.name.as_str())
            .and_then(|r| r.strip_prefix(':'))
            .filter(|r| !r.is_empty())
            .ok_or_else(|| Error::new(ErrorKind::BadRequest, format!("{id} is not a Gmail ID")))
    }

    /// The Gmail thread ID in `gmail:<thread>[/<message>]`.
    fn thread_id<'a>(&self, id: &'a str) -> Result<&'a str> {
        let local = self.local(id)?;
        let thread = local.split('/').next().unwrap_or("");
        if thread.is_empty() || thread.contains(':') {
            return Err(self.fail(ErrorKind::NotFound, format!("no Gmail thread {id}")));
        }
        Ok(thread)
    }

    /// `(thread, message)` from `gmail:<thread>/<message>`.
    fn message_ref<'a>(&self, id: &'a str) -> Result<(&'a str, &'a str)> {
        self.local(id)?.split_once('/').filter(|(t, m)| !t.is_empty() && !m.is_empty()).ok_or_else(|| {
            self.fail(ErrorKind::NotFound, format!("{id} isn't a Gmail message ID (gmail:<thread>/<message>)"))
        })
    }

    /// A thread's summary and Message-IDs from `threads.get` (metadata or full).
    fn parse_thread(&self, t: &Value) -> Option<Cached> {
        let id = t["id"].as_str()?;
        let msgs: Vec<&Value> = t["messages"].as_array()?.iter().filter(|m| !has_label(m, "DRAFT")).collect();
        if msgs.is_empty() {
            return None;
        }
        let incoming: Vec<&&Value> = msgs.iter().filter(|m| !has_label(m, "SENT")).collect();
        let latest = msgs.iter().max_by_key(|m| internal_date(m)).copied()?;
        let latest_in = incoming.iter().max_by_key(|m| internal_date(m)).map(|m| **m).unwrap_or(latest);
        let head = |m: &Value, name: &str| header(&m["payload"], name);
        let folder = if msgs.iter().any(|m| has_label(m, "INBOX")) {
            "inbox"
        } else if incoming.is_empty() {
            "sent"
        } else {
            "archive"
        };
        let summary = ThreadSummary {
            id: format!("{}:{id}", self.name),
            subject: msgs.iter().map(|m| head(m, "Subject")).find(|s| !s.is_empty()).unwrap_or_default(),
            folder: folder.into(),
            snippet: html_to_text(&text(&latest["snippet"])),
            from: addresses(&head(latest_in, "From")).into_iter().next(),
            to_address: nonempty(bare_email(&head(latest_in, "Delivered-To"))),
            message_count: msgs.len() as i64,
            unread: msgs.iter().any(|m| has_label(m, "UNREAD")),
            has_attachments: msgs.iter().any(|m| {
                let ct = head(m, "Content-Type").to_ascii_lowercase();
                ct.starts_with("multipart/mixed")
                    || m["payload"]["mimeType"].as_str().is_some_and(|t| t.eq_ignore_ascii_case("multipart/mixed"))
            }),
            last_at: internal_date(latest),
            account: Some(self.name.clone()),
        };
        let message_ids =
            msgs.iter().map(|m| normalize_message_id(&head(m, "Message-ID"))).filter(|m| !m.is_empty()).collect();
        let recipients = msgs
            .iter()
            .filter(|m| has_label(m, "SENT"))
            .flat_map(|m| [addresses(&head(m, "To")), addresses(&head(m, "Cc"))].concat())
            .collect();
        Some(Cached { history: text(&t["historyId"]), summary, message_ids, recipients })
    }

    fn get_thread(&self, id: &str, format: &str) -> Result<Value> {
        let mut params = json!({ "userId": "me", "id": id, "format": format });
        if format == "metadata" {
            params["metadataHeaders"] = json!(META_HEADERS);
        }
        self.call(&["threads", "get"], params, None)
    }

    /// `threads.list`, then each thread's headers (from the cache while its historyId holds),
    /// a few `gws` runs at a time, in Gmail's order.
    fn listing(&self, params: Value) -> Result<Vec<ThreadSummary>> {
        let data = self.call(&["threads", "list"], params, None)?;
        let entries: Vec<(String, String)> = match data.get("threads") {
            None | Some(Value::Null) => Vec::new(),
            Some(list) => list
                .as_array()
                .ok_or_else(|| self.shape_error("threads list"))?
                .iter()
                .filter_map(|t| Some((t["id"].as_str()?.to_string(), text(&t["historyId"]))))
                .collect(),
        };
        let mut found: HashMap<String, Cached> = HashMap::new();
        let mut stale = Vec::new();
        {
            let cache = self.cache.lock().unwrap();
            for (id, history) in &entries {
                match cache.get(id).filter(|c| !history.is_empty() && c.history == *history) {
                    Some(c) => {
                        found.insert(id.clone(), c.clone());
                    }
                    None => stale.push(id.clone()),
                }
            }
        }
        let mut first_err = None;
        for chunk in stale.chunks(PARALLEL) {
            let results: Vec<(String, Result<Value>)> = std::thread::scope(|s| {
                let handles: Vec<_> =
                    chunk.iter().map(|id| (id.clone(), s.spawn(move || self.get_thread(id, "metadata")))).collect();
                handles
                    .into_iter()
                    .map(|(id, h)| (id, h.join().unwrap_or_else(|_| Err(self.shape_error("threads get")))))
                    .collect()
            });
            for (id, r) in results {
                match r.map(|v| self.parse_thread(&v)) {
                    Ok(Some(c)) => {
                        self.cache.lock().unwrap().insert(id.clone(), c.clone());
                        found.insert(id, c);
                    }
                    Ok(None) => {}
                    // A thread deleted since the listing is just left out; a lost sign-in stops everything.
                    Err(e) if e.kind == ErrorKind::AccountAuth => return Err(e),
                    Err(e) if e.kind == ErrorKind::NotFound => {}
                    Err(e) => {
                        first_err.get_or_insert(e);
                    }
                }
            }
        }
        if !stale.is_empty() {
            self.save_cache();
        }
        if found.is_empty()
            && let Some(e) = first_err
        {
            return Err(e);
        }
        Ok(entries.iter().filter_map(|(id, _)| found.remove(id)).map(|c| c.summary).collect())
    }

    fn own_addresses(&self) -> Result<Vec<Address>> {
        if let Some(own) = self.own.lock().unwrap().clone() {
            return Ok(own);
        }
        let own = match self.call(&["settings", "sendAs", "list"], json!({ "userId": "me" }), None) {
            Ok(data) => {
                let mut list: Vec<(bool, Address)> = data["sendAs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|s| s["isPrimary"] == json!(true) || s["verificationStatus"] == "accepted")
                    .map(|s| {
                        (
                            s["isDefault"] == json!(true),
                            Address { name: nonempty(text(&s["displayName"])), email: text(&s["sendAsEmail"]) },
                        )
                    })
                    .filter(|(_, a)| !a.email.is_empty())
                    .collect();
                list.sort_by_key(|(default, _)| !*default);
                list.into_iter().map(|(_, a)| a).collect::<Vec<_>>()
            }
            Err(e) if e.kind == ErrorKind::AccountAuth => return Err(e),
            // Without send-as settings, the account's own address still is one.
            Err(_) => {
                let p = self.call(&["getProfile"], json!({ "userId": "me" }), None)?;
                nonempty(text(&p["emailAddress"])).map(|email| vec![Address { name: None, email }]).unwrap_or_default()
            }
        };
        if !own.is_empty() {
            *self.own.lock().unwrap() = Some(own.clone());
        }
        Ok(own)
    }

    fn forget_thread(&self, thread: &str) {
        self.cache.lock().unwrap().remove(thread);
    }

    fn modify(&self, id: &str, add: &[&str], remove: &[&str]) -> Result<()> {
        let thread = self.thread_id(id)?.to_string();
        self.call(
            &["threads", "modify"],
            json!({ "userId": "me", "id": thread }),
            Some(&json!({ "addLabelIds": add, "removeLabelIds": remove })),
        )?;
        self.forget_thread(&thread);
        Ok(())
    }

    /// The Message-ID and References a reply to this message carries.
    fn threading(&self, message: &str) -> Result<Threading> {
        let m = self.call(&["messages", "get"], json!({ "userId": "me", "id": message, "format": "metadata", "metadataHeaders": ["Message-ID", "References"] }), None)?;
        Ok(Threading {
            in_reply_to: header(&m["payload"], "Message-ID"),
            references: header(&m["payload"], "References"),
        })
    }

    fn query(&self, q: &ThreadQuery, folder: &str) -> Value {
        let mut words: Vec<String> = Vec::new();
        let mut label: Option<&str> = None;
        match folder {
            "inbox" => label = Some("INBOX"),
            "sent" => label = Some("SENT"),
            // Threads with mail that isn't in the Inbox; ones still in it are dropped after.
            "archive" => words.push("-in:inbox -in:sent -in:draft -in:chats".into()),
            _ => {}
        }
        if let Some(extra) = q.q.as_deref().filter(|s| !s.trim().is_empty()) {
            words.push(extra.trim().to_string());
        }
        if let Some(before) = q.before {
            words.push(format!("before:{}", before.div_euclid(1000) + 1));
        }
        if let Some(since) = q.since {
            words.push(format!("after:{}", since.div_euclid(1000)));
        }
        if q.unread {
            words.push("is:unread".into());
        }
        let mut params = json!({ "userId": "me", "maxResults": q.limit.clamp(1, LIST_LIMIT) });
        if let Some(l) = label {
            params["labelIds"] = json!([l]);
        }
        if !words.is_empty() {
            params["q"] = json!(words.join(" "));
        }
        params
    }

    /// A message part tree's text, HTML and attachments.
    fn walk(
        &self,
        message: &str,
        part: &Value,
        text_body: &mut Option<String>,
        html: &mut Option<String>,
        atts: &mut Vec<Attachment>,
    ) {
        let mime = part["mimeType"].as_str().unwrap_or("").to_ascii_lowercase();
        let filename = text(&part["filename"]);
        let disposition = header(part, "Content-Disposition").to_ascii_lowercase();
        let is_file = !mime.starts_with("multipart/")
            && (!filename.is_empty()
                || part["body"]["attachmentId"].is_string()
                || disposition.starts_with("attachment"));
        if is_file {
            let part_id = nonempty(text(&part["partId"])).unwrap_or_else(|| "root".into());
            let cid = header(part, "Content-ID");
            atts.push(Attachment {
                id: format!("{}:{message}:{part_id}", self.name),
                filename: if filename.is_empty() { "attachment".into() } else { filename },
                mime_type: mime.clone(),
                size: part["body"]["size"].as_i64().unwrap_or(0),
                inline: !cid.is_empty() && !disposition.starts_with("attachment"),
            });
        } else if mime == "text/plain" || mime == "text/html" {
            let slot = if mime == "text/plain" { &mut *text_body } else { &mut *html };
            if slot.is_none()
                && let Some(bytes) = part["body"]["data"].as_str().and_then(decode_b64)
            {
                *slot = Some(decode_text(&bytes, &header(part, "Content-Type")));
            }
        }
        for p in part["parts"].as_array().into_iter().flatten() {
            self.walk(message, p, text_body, html, atts);
        }
    }

    fn find_part<'a>(part: &'a Value, part_id: &str) -> Option<&'a Value> {
        let own = part["partId"].as_str().unwrap_or("");
        if own == part_id || (part_id == "root" && own.is_empty()) {
            return Some(part);
        }
        part["parts"].as_array().into_iter().flatten().find_map(|p| Self::find_part(p, part_id))
    }
}

impl Provider for Gmail {
    fn name(&self) -> &str {
        &self.name
    }

    fn label(&self) -> &str {
        "Gmail"
    }

    fn has_folder(&self, folder: &str) -> bool {
        matches!(folder, "inbox" | "archive" | "sent" | "all")
    }

    fn sign_in(&self) -> Result<()> {
        self.login().map(|_| ())
    }

    fn status(&self) -> AccountStatus {
        let mut status = AccountStatus {
            name: self.name.clone(),
            provider: "gmail".into(),
            label: "Gmail".into(),
            ..Default::default()
        };
        if !self.signed_in() {
            status.detail = format!("not signed in: run `cloudmail account login {}`", self.name);
            return status;
        }
        match self.own_addresses() {
            Ok(own) => {
                status.ok = true;
                status.addresses = own.into_iter().map(|a| a.email).collect();
                status.detail = format!("signed in via {} (sign-in kept in {})", self.command, self.dir.display());
            }
            Err(e) => status.detail = e.message,
        }
        status
    }

    fn threads(&self, q: &ThreadQuery) -> Result<Vec<ThreadSummary>> {
        let folder = if q.folder.is_empty() { "inbox" } else { q.folder.as_str() };
        if !self.has_folder(folder) {
            return Ok(Vec::new());
        }
        let mut list = self.listing(self.query(q, folder))?;
        list.retain(|t| {
            (folder != "archive" || t.folder != "inbox")
                && q.before.is_none_or(|b| t.last_at < b)
                && q.since.is_none_or(|s| t.last_at > s)
                && (!q.unread || t.unread)
        });
        list.sort_by_key(|t| std::cmp::Reverse(t.last_at));
        list.truncate(q.limit.max(1) as usize);
        Ok(list)
    }

    fn search(&self, query: &str, limit: u32) -> Result<Vec<ThreadSummary>> {
        let mut list = self.listing(json!({ "userId": "me", "q": query, "maxResults": limit.clamp(1, LIST_LIMIT) }))?;
        list.truncate(limit.max(1) as usize);
        Ok(list)
    }

    fn thread(&self, id: &str, _html: bool) -> Result<ThreadDetail> {
        let thread = self.thread_id(id)?;
        let data = self.get_thread(thread, "full")?;
        let cached = self
            .parse_thread(&data)
            .ok_or_else(|| self.fail(ErrorKind::NotFound, format!("no Gmail thread {id} (or only drafts)")))?;
        let summary = cached.summary.clone();
        let mut messages: Vec<Message> = data["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| !has_label(m, "DRAFT"))
            .map(|m| {
                let mid = text(&m["id"]);
                let p = &m["payload"];
                let (mut body, mut html, mut atts) = (None, None, Vec::new());
                self.walk(&mid, p, &mut body, &mut html, &mut atts);
                Message {
                    id: format!("{}:{thread}/{mid}", self.name),
                    thread_id: summary.id.clone(),
                    outgoing: has_label(m, "SENT"),
                    from: addresses(&header(p, "From")).into_iter().next().unwrap_or_default(),
                    to: addresses(&header(p, "To")),
                    cc: addresses(&header(p, "Cc")),
                    reply_to: addresses(&header(p, "Reply-To")),
                    subject: header(p, "Subject"),
                    date: internal_date(m),
                    text: body.filter(|b| !b.trim().is_empty()).or_else(|| html.as_deref().map(html_to_text)),
                    html,
                    message_id: nonempty(header(p, "Message-ID")),
                    attachments: atts,
                    auth: None,
                }
            })
            .collect();
        messages.sort_by_key(|m| m.date);
        self.cache.lock().unwrap().insert(thread.to_string(), cached);
        Ok(ThreadDetail { thread: summary, messages })
    }

    fn move_thread(&self, id: &str, folder: &str) -> Result<()> {
        match folder {
            "inbox" => self.modify(id, &["INBOX"], &[]),
            "archive" => self.modify(id, &[], &["INBOX"]),
            other => Err(self.fail(
                ErrorKind::BadRequest,
                format!("Gmail threads move between the Inbox and the Archive, not \"{other}\""),
            )),
        }
    }

    fn set_unread(&self, id: &str, unread: bool) -> Result<()> {
        if unread { self.modify(id, &["UNREAD"], &[]) } else { self.modify(id, &[], &["UNREAD"]) }
    }

    fn screener(&self) -> Result<Vec<PendingSender>> {
        Ok(Vec::new())
    }

    fn decide_sender(&self, id: &str, _status: &str) -> Result<i64> {
        Err(self.fail(ErrorKind::BadRequest, format!("Gmail has no Screener, so there's no sender {id} to decide on")))
    }

    fn identities(&self) -> Result<Vec<Address>> {
        let own = self.own_addresses()?;
        if own.is_empty() {
            return Err(self.fail(ErrorKind::AccountUnavailable, "Gmail reported no address to send from"));
        }
        Ok(own)
    }

    fn send(&self, req: &SendRequest) -> Result<SendResponse> {
        let own = self.identities()?;
        let from = match req.from.as_deref().map(bare_email).filter(|f| !f.is_empty()) {
            Some(f) => own.iter().find(|a| a.email.eq_ignore_ascii_case(&f)).cloned().ok_or_else(|| {
                self.fail(ErrorKind::BadRequest, format!("{f} isn't one of your Gmail send-as addresses"))
            })?,
            None => own[0].clone(),
        };
        let (thread, threading) = match req.reply_to_message_id.as_deref() {
            Some(target) => {
                let (thread, message) = self.message_ref(target)?;
                (Some(thread.to_string()), Some(self.threading(message)?))
            }
            None => (None, None),
        };
        let message = build_message(&from, req, threading.as_ref(), &chrono::Utc::now().to_rfc2822());
        if !self.signed_in() {
            return Err(self.not_signed_in());
        }
        // The message goes as a media upload from a file (an argument couldn't hold more than about
        // 128 KiB), in a private directory inside gws's working directory, removed after.
        let dir = PrivateDir::new(&self.dir, ".outgoing").map_err(|e| {
            self.fail(
                ErrorKind::AccountUnavailable,
                format!("could not create a temporary directory in {}: {e}", self.dir.display()),
            )
        })?;
        let path = dir.write("0", "message.eml", message.as_bytes()).map_err(|e| {
            self.fail(ErrorKind::AccountUnavailable, format!("could not write the message for gws: {e}"))
        })?;
        let relative = path.strip_prefix(&self.dir).unwrap_or(&path).to_path_buf();
        let metadata = thread.as_ref().map(|t| json!({ "threadId": t }));
        let sent = self.call_with(&["messages", "send"], json!({ "userId": "me" }), metadata.as_ref(), Some(&relative));
        drop(dir);
        let sent = sent?;
        let thread_id = nonempty(text(&sent["threadId"])).or(thread);
        if let Some(t) = &thread_id {
            self.forget_thread(t);
        }
        Ok(SendResponse { thread_id: thread_id.map(|t| format!("{}:{t}", self.name)), message: None, warning: None })
    }

    fn attachment_limit(&self) -> u64 {
        crate::attach::GMAIL_LIMIT
    }

    fn download_attachment(&self, id: &str) -> Result<Download> {
        let (message, part_id) = self
            .local(id)?
            .split_once(':')
            .ok_or_else(|| self.fail(ErrorKind::NotFound, format!("{id} isn't a Gmail attachment ID")))?;
        let m = self.call(&["messages", "get"], json!({ "userId": "me", "id": message, "format": "full" }), None)?;
        let part = Self::find_part(&m["payload"], part_id)
            .ok_or_else(|| self.fail(ErrorKind::NotFound, format!("no attachment {id}")))?;
        let data = match part["body"]["attachmentId"].as_str() {
            Some(att) => text(
                &self.call(
                    &["messages", "attachments", "get"],
                    json!({ "userId": "me", "messageId": message, "id": att }),
                    None,
                )?["data"],
            ),
            None => text(&part["body"]["data"]),
        };
        let bytes = decode_b64(&data).ok_or_else(|| self.shape_error("messages attachments get"))?;
        Ok(Download {
            bytes,
            content_type: nonempty(text(&part["mimeType"])),
            filename: nonempty(text(&part["filename"])),
        })
    }

    fn raw_message(&self, id: &str) -> Result<Vec<u8>> {
        let (_, message) = self.message_ref(id)?;
        let m = self.call(&["messages", "get"], json!({ "userId": "me", "id": message, "format": "raw" }), None)?;
        m["raw"].as_str().and_then(decode_b64).ok_or_else(|| self.shape_error("messages get"))
    }

    fn message_ids(&self, thread_id: &str) -> Vec<String> {
        let Ok(thread) = self.thread_id(thread_id) else { return Vec::new() };
        self.cache.lock().unwrap().get(thread).map(|c| c.message_ids.clone()).unwrap_or_default()
    }

    fn screened_by_worker(&self) -> bool {
        true
    }

    fn correspondents(&self, limit: u32) -> Result<Vec<Address>> {
        let own: Vec<String> = self.own_addresses()?.into_iter().map(|a| a.email.to_ascii_lowercase()).collect();
        let max = limit.clamp(1, LIST_LIMIT);
        let mut out: Vec<Address> = self
            .listing(json!({ "userId": "me", "labelIds": ["INBOX"], "maxResults": max }))?
            .into_iter()
            .filter_map(|t| t.from)
            .collect();
        let sent = self.listing(json!({ "userId": "me", "labelIds": ["SENT"], "maxResults": max }))?;
        let cache = self.cache.lock().unwrap();
        for t in sent {
            if let Some(c) = self.thread_id(&t.id).ok().and_then(|id| cache.get(id)) {
                out.extend(c.recipients.iter().cloned());
            }
        }
        out.retain(|a| !own.contains(&a.email.to_ascii_lowercase()));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE;

    fn gmail() -> Gmail {
        Gmail::new("gmail", &AccountConfig::default())
    }

    fn msg(id: &str, labels: &[&str], at: i64, headers: &[(&str, &str)]) -> Value {
        json!({
            "id": id, "threadId": "t1", "labelIds": labels, "snippet": "It&#39;s here &amp; ready", "internalDate": at.to_string(),
            "payload": { "mimeType": "text/plain", "headers": headers.iter().map(|(n, v)| json!({ "name": n, "value": v })).collect::<Vec<_>>() }
        })
    }

    #[test]
    fn threads_map_to_summaries() {
        let t = json!({ "id": "t1", "historyId": "77", "messages": [
            msg("m1", &["INBOX", "UNREAD"], 1_000, &[("From", "\"Ana, A.\" <ana@example.net>"), ("Subject", "Trip"), ("Message-Id", "<One@Example.net>"), ("Delivered-To", "me@gmail.example"), ("Content-Type", "multipart/mixed; boundary=x")]),
            msg("m2", &["SENT"], 2_000, &[("From", "Me <me@gmail.example>"), ("Subject", "Re: Trip"), ("Message-ID", "<two@gmail.example>")]),
            msg("m3", &["DRAFT"], 3_000, &[("From", "Me <me@gmail.example>")]),
        ]});
        let c = gmail().parse_thread(&t).unwrap();
        let s = &c.summary;
        assert_eq!(s.id, "gmail:t1");
        assert_eq!((s.subject.as_str(), s.folder.as_str(), s.message_count, s.last_at), ("Trip", "inbox", 2, 2_000));
        assert!(s.unread && s.has_attachments);
        assert_eq!(
            s.from,
            Some(Address { name: Some("Ana, A.".into()), email: "ana@example.net".into() }),
            "the latest mail you received"
        );
        assert_eq!(s.to_address.as_deref(), Some("me@gmail.example"));
        assert_eq!(s.snippet, "It's here & ready");
        assert_eq!(c.message_ids, ["one@example.net", "two@gmail.example"]);
        assert_eq!(c.history, "77");

        let archived = json!({ "id": "t2", "messages": [msg("m1", &["CATEGORY_UPDATES"], 5, &[("From", "a@b.c")])] });
        assert_eq!(gmail().parse_thread(&archived).unwrap().summary.folder, "archive");
        let sent = json!({ "id": "t3", "messages": [msg("m1", &["SENT"], 5, &[("From", "me@gmail.example")])] });
        assert_eq!(gmail().parse_thread(&sent).unwrap().summary.folder, "sent");
        let drafts = json!({ "id": "t4", "messages": [msg("m1", &["DRAFT"], 5, &[])] });
        assert!(gmail().parse_thread(&drafts).is_none());
    }

    #[test]
    fn folders_become_labels_and_queries() {
        let g = gmail();
        let p = g.query(
            &ThreadQuery {
                folder: "inbox".into(),
                limit: 25,
                before: Some(10_500),
                unread: true,
                ..Default::default()
            },
            "inbox",
        );
        assert_eq!(p, json!({ "userId": "me", "maxResults": 25, "labelIds": ["INBOX"], "q": "before:11 is:unread" }));
        let p = g.query(&ThreadQuery { folder: "archive".into(), limit: 500, ..Default::default() }, "archive");
        assert_eq!(p["maxResults"], 100);
        assert!(p["q"].as_str().unwrap().starts_with("-in:inbox"));
        assert!(p.get("labelIds").is_none());
    }

    #[test]
    fn ids_route_back() {
        let g = gmail();
        assert!(g.owns("gmail:abc") && !g.owns("hey:1") && !g.owns("t_1"));
        assert_eq!(g.thread_id("gmail:abc/def").unwrap(), "abc");
        assert_eq!(g.message_ref("gmail:abc/def").unwrap(), ("abc", "def"));
        assert!(g.message_ref("gmail:abc").is_err());
        assert!(g.thread_id("gmail:m1:2").is_err(), "an attachment isn't a thread");
    }

    #[test]
    fn replies_carry_threading_headers() {
        let req = SendRequest {
            to: vec!["Ana Álvarez <ana@example.net>".into(), "b@example.net, \"Last, First\" <c@example.net>".into()],
            bcc: vec!["hidden@example.net".into()],
            subject: "Re: Trip ✈".into(),
            text: "See you\nthere".into(),
            ..Default::default()
        };
        let t = Threading { in_reply_to: "<m1@example.net>".into(), references: "<m0@example.net>".into() };
        let raw = build_message(
            &Address { name: Some("Sam Sample".into()), email: "me@gmail.example".into() },
            &req,
            Some(&t),
            "Wed, 30 Sep 2026 10:00:00 +0000",
        );
        let (head, body) = raw.split_once("\r\n\r\n").unwrap();
        assert!(head.contains("From: Sam Sample <me@gmail.example>\r\n"), "{head}");
        assert!(head.contains("To: =?UTF-8?B?"), "{head}");
        assert!(head.contains("<ana@example.net>, b@example.net, \"Last, First\" <c@example.net>\r\n"), "{head}");
        assert!(head.contains("Bcc: hidden@example.net\r\n"));
        assert!(
            head.contains("In-Reply-To: <m1@example.net>\r\nReferences: <m0@example.net> <m1@example.net>\r\n"),
            "{head}"
        );
        assert!(head.contains("Subject: =?UTF-8?B?"));
        assert_eq!(STANDARD.decode(body.replace("\r\n", "")).unwrap(), b"See you\r\nthere");
        assert!(!head.contains("Cc:"), "no empty headers");

        let injected =
            SendRequest { to: vec!["a@b.c".into()], subject: "Hi\r\nBcc: evil@x.y".into(), ..Default::default() };
        let raw = build_message(&Address { name: None, email: "me@gmail.example".into() }, &injected, None, "d");
        assert!(!raw.contains("\r\nBcc:") && !raw.contains("In-Reply-To"), "{raw}");
    }

    #[test]
    fn attachments_make_a_multipart_message() {
        let pdf: Vec<u8> = (0..=255).cycle().take(200).collect();
        let req = SendRequest {
            to: vec!["ana@example.net".into()],
            subject: "Files".into(),
            text: "Two files\nattached".into(),
            attachments: vec![
                OutgoingAttachment {
                    filename: "Résumé 2026.pdf".into(),
                    mime_type: "application/pdf".into(),
                    content: pdf.clone(),
                },
                OutgoingAttachment {
                    filename: "say \"hi\".txt".into(),
                    mime_type: "text/plain\r\nX-Evil: 1".into(),
                    content: b"hi".to_vec(),
                },
            ],
            ..Default::default()
        };
        let t = Threading { in_reply_to: "<m1@example.net>".into(), references: String::new() };
        let raw = build_message(&Address { name: None, email: "me@gmail.example".into() }, &req, Some(&t), "d");
        assert!(raw.lines().all(|l| l.len() <= 998), "no line over RFC 5322's limit");
        assert!(!raw.contains("X-Evil"), "{raw}");
        let (head, body) = raw.split_once("\r\n\r\n").unwrap();
        assert!(
            head.contains("In-Reply-To: <m1@example.net>\r\nReferences: <m1@example.net>\r\nMIME-Version: 1.0\r\n"),
            "{head}"
        );
        let boundary = head.split("boundary=\"").nth(1).unwrap().split('"').next().unwrap();
        assert!(head.ends_with(&format!("Content-Type: multipart/mixed; boundary=\"{boundary}\"")), "{head}");
        assert!(body.ends_with(&format!("\r\n--{boundary}--\r\n")));
        let parts: Vec<&str> = body.split(&format!("--{boundary}")).collect();
        assert_eq!(parts.len(), 5, "preamble, text, two files, end: {body}");
        let part = |p: &str| {
            let (h, b) = p.trim_start_matches("\r\n").split_once("\r\n\r\n").unwrap();
            (h.to_string(), STANDARD.decode(b.replace("\r\n", "")).unwrap())
        };
        let (h, b) = part(parts[1]);
        assert_eq!(h, "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64");
        assert_eq!(b, b"Two files\r\nattached");
        let (h, b) = part(parts[2]);
        assert_eq!(
            h,
            format!(
                "Content-Type: application/pdf; name=\"=?UTF-8?B?{}?=\"\r\nContent-Disposition: attachment; filename*=UTF-8''R%C3%A9sum%C3%A9%202026.pdf\r\nContent-Transfer-Encoding: base64",
                STANDARD.encode("Résumé 2026.pdf")
            )
        );
        assert_eq!(b, pdf);
        let (h, b) = part(parts[3]);
        assert_eq!(
            h,
            "Content-Type: application/octet-stream; name=\"say \\\"hi\\\".txt\"\r\nContent-Disposition: attachment; filename=\"say \\\"hi\\\".txt\"\r\nContent-Transfer-Encoding: base64"
        );
        assert_eq!(b, b"hi");
        assert_eq!(parts[4], "--\r\n");
    }

    #[test]
    fn bodies_and_attachments_come_from_parts() {
        let g = gmail();
        let part = json!({ "partId": "", "mimeType": "multipart/mixed", "parts": [
            { "partId": "0", "mimeType": "multipart/alternative", "parts": [
                { "partId": "0.0", "mimeType": "text/plain", "headers": [{ "name": "Content-Type", "value": "text/plain; charset=\"ISO-8859-1\"" }], "body": { "size": 4, "data": URL_SAFE.encode(b"caf\xe9") } },
                { "partId": "0.1", "mimeType": "text/html", "body": { "size": 9, "data": URL_SAFE.encode("<b>hi</b>") } }
            ]},
            { "partId": "1", "mimeType": "application/pdf", "filename": "trip.pdf", "headers": [{ "name": "Content-Disposition", "value": "attachment; filename=trip.pdf" }], "body": { "size": 5, "attachmentId": "ANGj" } },
            { "partId": "2", "mimeType": "image/png", "filename": "logo.png", "headers": [{ "name": "Content-ID", "value": "<logo>" }, { "name": "Content-Disposition", "value": "inline" }], "body": { "size": 3, "attachmentId": "ANGk" } }
        ]});
        let (mut t, mut h, mut a) = (None, None, Vec::new());
        g.walk("m1", &part, &mut t, &mut h, &mut a);
        assert_eq!(t.as_deref(), Some("café"));
        assert_eq!(h.as_deref(), Some("<b>hi</b>"));
        assert_eq!(
            a.iter().map(|a| (a.id.as_str(), a.inline)).collect::<Vec<_>>(),
            [("gmail:m1:1", false), ("gmail:m1:2", true)]
        );
        assert_eq!(Gmail::find_part(&part, "0.1").unwrap()["mimeType"], "text/html");
    }

    #[test]
    fn errors_map_to_kinds() {
        let g = gmail();
        let e = g.gws_error(Some(2), &json!({ "error": { "code": 401, "message": "Authentication failed: invalid_grant", "reason": "authError" } }), "", "threads list");
        assert_eq!(e.kind, ErrorKind::AccountAuth);
        assert!(e.message.contains("cloudmail account login gmail"), "{}", e.message);
        assert_eq!(
            g.gws_error(
                Some(1),
                &json!({ "error": { "code": 404, "message": "Requested entity was not found." } }),
                "",
                "x"
            )
            .kind,
            ErrorKind::NotFound
        );
        assert_eq!(
            g.gws_error(Some(1), &json!({ "error": { "code": 401, "message": "Invalid Credentials" } }), "", "x").kind,
            ErrorKind::AccountAuth
        );
        let e = g.gws_error(Some(4), &Value::Null, "error[discovery]: dns error", "x");
        assert!(
            e.kind == ErrorKind::AccountUnavailable && e.message.contains("couldn't reach Google"),
            "{}",
            e.message
        );
    }

    #[test]
    fn a_missing_client_says_how_to_set_one() {
        let cfg = AccountConfig { client_id: Some("id".into()), client_secret: Some(" ".into()), ..Default::default() };
        if GOOGLE_CLIENT_ID.is_empty() && std::env::var(CLIENT_ID_ENV).is_err() {
            assert_eq!(client_credentials(&cfg), None, "a blank secret isn't a client");
        }
        let cfg = AccountConfig { client_id: Some("id".into()), client_secret: Some("s".into()), ..Default::default() };
        if std::env::var(CLIENT_ID_ENV).is_err() {
            assert_eq!(client_credentials(&cfg), Some(("id".into(), "s".into())));
        }
        assert!(Gmail::no_client_error().message.contains(CLIENT_ID_ENV));
    }
}
