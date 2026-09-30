//! Mail providers: your Cloudmail worker, and linked accounts (HEY; Gmail can follow the same shape).
//!
//! A provider speaks in cloudmail's own types. A linked account prefixes every ID it hands out
//! with its name (`hey:…`), so any later action on that ID goes back to it; the worker's own IDs
//! (`t_…`, `m_…`, `a_…`) stay as they are.

use serde::Serialize;
use std::sync::Arc;

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
    /// Whether the provider has anything in this folder (HEY has no Sent or Blocked box).
    fn has_folder(&self, folder: &str) -> bool;
    /// Whether an ID (thread, message, attachment or screener sender) belongs to this provider.
    fn owns(&self, id: &str) -> bool {
        id.strip_prefix(self.name()).is_some_and(|rest| rest.starts_with(':'))
    }
    fn status(&self) -> AccountStatus;
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
    fn download_attachment(&self, id: &str) -> Result<Download>;
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

    fn download_attachment(&self, id: &str) -> Result<Download> {
        Client::download_attachment(self, id)
    }
}

/// Providers cloudmail knows how to link, for `account add` and its help.
pub const KNOWN_PROVIDERS: &[(&str, &str)] = &[("hey", "HEY (hey.com), through the official `hey` CLI")];

/// Opens a configured linked account.
pub fn open(name: &str, cfg: &AccountConfig) -> Result<Arc<dyn Provider>> {
    match cfg.provider(name) {
        "hey" => Ok(Arc::new(crate::hey::Hey::new(name, cfg))),
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
