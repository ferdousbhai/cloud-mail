//! Mail providers: your Cloudmail worker, and linked accounts (HEY, Gmail, iCloud Mail).
//!
//! A provider speaks in cloudmail's own types. A linked account prefixes every ID it hands out
//! with its name (`hey:…`), so any later action on that ID goes back to it; the worker's own IDs
//! (`t_…`, `m_…`, `a_…`) stay as they are.

use serde::Serialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::client::{Client, ThreadQuery};
use crate::config::AccountConfig;
use crate::error::{Error, ErrorKind, Result};
use crate::types::*;

/// Folders every provider understands. `screener` lists threads waiting there.
pub const CORE_FOLDERS: &[&str] = &["inbox", "archive", "sent", "screener", "blocked", "all"];

/// A folder only some linked accounts have (HEY's The Feed, …), with its display name.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct ExtraFolder {
    pub folder: &'static str,
    pub title: &'static str,
}

/// What `account list` and the app show about an account.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AccountStatus {
    pub name: String,
    pub provider: String,
    pub label: String,
    /// Signed in and reachable.
    pub ok: bool,
    /// The account's own addresses, when known.
    pub addresses: Vec<String>,
    /// What's wrong, or a short note about the account.
    pub detail: String,
}

/// One account's failure during an operation that went on without it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AccountWarning {
    pub account: String,
    pub code: String,
    pub message: String,
}

impl AccountWarning {
    pub fn new(account: &str, e: &Error) -> Self {
        Self { account: account.to_string(), code: e.kind.code().to_string(), message: e.message.clone() }
    }
}

pub trait Provider: Send + Sync {
    /// The account's name, which prefixes its IDs ("hey").
    fn name(&self) -> &str;
    /// Human name of the service ("HEY").
    fn label(&self) -> &str;
    /// Folders beyond the core ones.
    fn extra_folders(&self) -> &[ExtraFolder] {
        &[]
    }
    /// Where archiving puts a thread, as a folder name (HEY's is Paper Trail).
    fn archive_folder(&self) -> &str {
        "archive"
    }
    /// Whether the provider has anything in this folder (HEY has no Sent or Blocked box).
    fn has_folder(&self, folder: &str) -> bool;
    /// Whether an ID (thread, message, attachment or screener sender) belongs to this provider.
    fn owns(&self, id: &str) -> bool {
        id.strip_prefix(self.name()).is_some_and(|rest| rest.starts_with(':'))
    }
    fn status(&self) -> AccountStatus;
    /// Signs in again in the browser (the provider's own sign-in), for an account whose sign-in
    /// expired or was revoked. Blocks until the sign-in finishes.
    fn sign_in(&self) -> Result<()> {
        Err(Error::new(ErrorKind::BadRequest, format!("{} has no sign-in", self.label())))
    }
    fn threads(&self, q: &ThreadQuery) -> Result<Vec<ThreadSummary>>;
    fn search(&self, query: &str, limit: u32) -> Result<Vec<ThreadSummary>>;
    /// A thread with its messages; `html` asks for the original HTML bodies too.
    fn thread(&self, id: &str, html: bool) -> Result<ThreadDetail>;
    /// Moves a thread to "inbox", "archive" or one of the provider's extra folders.
    fn move_thread(&self, id: &str, folder: &str) -> Result<()>;
    fn set_unread(&self, id: &str, unread: bool) -> Result<()>;
    fn screener(&self) -> Result<Vec<PendingSender>>;
    /// Screens a sender in ("approved") or out ("blocked") by the ID `screener` gave it (or its
    /// address, for the worker); returns how many threads moved when known.
    fn decide_sender(&self, id: &str, status: &str) -> Result<i64>;
    /// Addresses you can send from with this provider.
    fn identities(&self) -> Result<Vec<Address>>;
    /// Sends a new message, or a reply when `reply_to_message_id` is one of this provider's message IDs.
    fn send(&self, req: &SendRequest) -> Result<SendResponse>;
    /// The most bytes of attachments one message can carry.
    fn attachment_limit(&self) -> u64;
    fn download_attachment(&self, id: &str) -> Result<Download>;
    /// A message's original .eml, when the provider gives it out.
    fn raw_message(&self, id: &str) -> Result<Vec<u8>> {
        Err(Error::new(ErrorKind::BadRequest, format!("{id} is a {} message, and {} doesn't give out original .eml files", self.label(), self.label())))
    }
    /// The Message-ID headers of a thread's messages as last listed, for spotting copies of
    /// worker mail exactly; empty when the provider doesn't expose them.
    fn message_ids(&self, _thread_id: &str) -> Vec<String> {
        Vec::new()
    }
    /// Whether your worker's Screener decides this account's senders (it has no Screener of its
    /// own). Its threads then wait in the Screener until their sender is approved.
    fn screened_by_worker(&self) -> bool {
        false
    }
    /// The people this account already corresponds with (Inbox senders, recipients of sent mail,
    /// about `limit` conversations of each), screened in when the account is linked.
    fn correspondents(&self, _limit: u32) -> Result<Vec<Address>> {
        Ok(Vec::new())
    }
}

/// Your Cloudmail worker, as a provider. Its IDs are unprefixed.
impl Provider for Client {
    fn name(&self) -> &str {
        "cloudmail"
    }

    fn label(&self) -> &str {
        "Cloudmail"
    }

    fn has_folder(&self, folder: &str) -> bool {
        CORE_FOLDERS.contains(&folder)
    }

    fn owns(&self, id: &str) -> bool {
        !id.contains(':')
    }

    fn status(&self) -> AccountStatus {
        let health = self.counts();
        AccountStatus {
            name: "cloudmail".into(),
            provider: "cloudmail".into(),
            label: "Cloudmail".into(),
            ok: health.is_ok(),
            addresses: self.identities().map(|i| i.identities.into_iter().map(|a| a.email).collect()).unwrap_or_default(),
            detail: match health {
                Ok(_) => self.base_url().to_string(),
                Err(e) => e.message,
            },
        }
    }

    fn threads(&self, q: &ThreadQuery) -> Result<Vec<ThreadSummary>> {
        self.list_threads(q)
    }

    fn search(&self, query: &str, limit: u32) -> Result<Vec<ThreadSummary>> {
        self.list_threads(&ThreadQuery { folder: "all".into(), q: Some(query.into()), limit, ..Default::default() })
    }

    fn thread(&self, id: &str, _html: bool) -> Result<ThreadDetail> {
        Client::thread(self, id)
    }

    fn move_thread(&self, id: &str, folder: &str) -> Result<()> {
        Client::move_thread(self, id, folder)
    }

    fn set_unread(&self, id: &str, unread: bool) -> Result<()> {
        Client::set_unread(self, id, unread)
    }

    fn screener(&self) -> Result<Vec<PendingSender>> {
        Client::screener(self)
    }

    fn decide_sender(&self, id: &str, status: &str) -> Result<i64> {
        Client::decide_sender(self, id, status)
    }

    fn identities(&self) -> Result<Vec<Address>> {
        Client::identities(self).map(|i| i.identities)
    }

    fn send(&self, req: &SendRequest) -> Result<SendResponse> {
        Client::send(self, req)
    }

    fn attachment_limit(&self) -> u64 {
        crate::attach::WORKER_LIMIT
    }

    fn download_attachment(&self, id: &str) -> Result<Download> {
        Client::download_attachment(self, id)
    }

    fn raw_message(&self, id: &str) -> Result<Vec<u8>> {
        Client::raw_message(self, id)
    }
}

/// Providers cloudmail knows how to link, for `account add` and its help.
pub const KNOWN_PROVIDERS: &[(&str, &str)] = &[
    ("hey", "HEY (hey.com), through the official `hey` CLI"),
    ("gmail", "Gmail, through Google's Workspace CLI `gws`"),
    ("icloud", "iCloud Mail, through the iCloud sign-in icloud-session keeps"),
];

/// How an account is named on screen: its provider's name for the usual account names, else the
/// name it was given.
pub fn account_label(account: &str) -> String {
    match account {
        "hey" => "HEY".into(),
        "gmail" => "Gmail".into(),
        "icloud" => "iCloud".into(),
        other => other.to_string(),
    }
}

/// Opens a configured linked account.
pub fn open(name: &str, cfg: &AccountConfig) -> Result<Arc<dyn Provider>> {
    match cfg.provider(name) {
        "hey" => Ok(Arc::new(crate::hey::Hey::new(name, cfg))),
        "gmail" => Ok(Arc::new(crate::gmail::Gmail::new(name, cfg))),
        "icloud" => Ok(Arc::new(crate::icloud::Icloud::new(name, cfg))),
        other => Err(Error::new(
            ErrorKind::Config,
            format!("account {name}: unknown provider \"{other}\" (known: {})", KNOWN_PROVIDERS.iter().map(|(p, _)| *p).collect::<Vec<_>>().join(", ")),
        )),
    }
}

/// Account names become ID prefixes, so they are short lowercase words.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 32 && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') && name != "cloudmail"
}

/// How a linked account's command-line tool ended.
pub(crate) enum Run {
    Done { status: ExitStatus, stdout: Vec<u8>, stderr: String },
    Missing,
    TimedOut,
    Failed(std::io::Error),
}

/// Runs a prepared command (stdout and stderr piped, stdin fed when given), killing it after `timeout`.
pub(crate) fn run_command(cmd: &mut Command, stdin: Option<&str>, timeout: Duration) -> Run {
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Run::Missing,
        Err(e) => return Run::Failed(e),
    };
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let input = input.to_string();
        std::thread::spawn(move || {
            let _ = pipe.write_all(input.as_bytes());
        });
    }
    let reader = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = reader(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = reader(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(15)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Run::TimedOut;
            }
            Err(e) => return Run::Failed(e),
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&err.join().unwrap_or_default()).trim().to_string();
    Run::Done { status, stdout, stderr }
}

/// A fresh directory only you can read, removed with everything in it when dropped: where files
/// handed to a linked account's CLI (attachments to send, downloads) live for a moment.
pub(crate) struct PrivateDir(PathBuf);

impl PrivateDir {
    pub fn new(parent: &Path, prefix: &str) -> std::io::Result<Self> {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let dir = parent.join(format!("{prefix}-{}-{nanos}", std::process::id()));
        Self::create(&dir)?;
        Ok(Self(dir))
    }

    fn create(dir: &Path) -> std::io::Result<()> {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Writes `bytes` to `<sub>/<name>` (owner-only, never through an existing file); a
    /// subdirectory per file lets two files share a name.
    pub fn write(&self, sub: &str, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
        let dir = self.0.join(sub);
        Self::create(&dir)?;
        let path = dir.join(name);
        let mut file = std::fs::OpenOptions::new();
        file.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
        file.open(&path)?.write_all(bytes)?;
        Ok(path)
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
