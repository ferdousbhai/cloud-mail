use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Address {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: String,
}

impl Address {
    pub fn display(&self) -> String {
        match self.name.as_deref().map(str::trim) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => self.email.clone(),
        }
    }

    /// "Name <email>" suitable for an address field.
    pub fn formatted(&self) -> String {
        match self.name.as_deref().map(str::trim) {
            Some(n) if !n.is_empty() => format!("{n} <{}>", self.email),
            _ => self.email.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ThreadSummary {
    pub id: String,
    pub subject: String,
    pub folder: String,
    pub snippet: String,
    pub from: Option<Address>,
    pub to_address: Option<String>,
    pub message_count: i64,
    pub unread: bool,
    pub has_attachments: bool,
    pub last_at: i64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Attachment {
    pub id: String,
    pub filename: String,
    pub mime_type: String,
    pub size: i64,
    pub inline: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Message {
    pub id: String,
    pub thread_id: String,
    pub outgoing: bool,
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub subject: String,
    pub date: i64,
    pub text: Option<String>,
    pub html: Option<String>,
    pub message_id: Option<String>,
    pub attachments: Vec<Attachment>,
    /// Cloudflare's authentication verdicts for received mail; None for sent mail and older messages.
    pub auth: Option<MessageAuth>,
}

impl Message {
    /// True when the receiving MX reported a DMARC failure: the From address may be forged.
    pub fn dmarc_failed(&self) -> bool {
        self.auth.as_ref().and_then(|a| a.dmarc.as_deref()).is_some_and(|d| d.eq_ignore_ascii_case("fail"))
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct MessageAuth {
    pub dmarc: Option<String>,
    pub spf: Option<String>,
    pub dkim: Option<String>,
    pub spam_score: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PendingSender {
    pub email: String,
    pub name: Option<String>,
    pub thread_count: i64,
    pub last_subject: Option<String>,
    pub last_at: i64,
}

impl PendingSender {
    pub fn display(&self) -> String {
        Address { name: self.name.clone(), email: self.email.clone() }.display()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Sender {
    pub email: String,
    pub name: String,
    pub status: String,
    pub decided_at: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Counts {
    pub screener: i64,
    pub inbox: i64,
    pub inbox_unread: i64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Identities {
    pub identities: Vec<Address>,
    pub default: Option<Address>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Mailbox {
    pub email: String,
    pub name: String,
    /// true: first-time senders go to the Screener; false: straight to the Inbox.
    pub screen: bool,
    pub position: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MailboxUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    /// Verified address that receives a copy of every message; empty when off.
    pub forward_to: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SendRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SendResponse {
    pub thread_id: String,
    pub message: Option<Message>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ThreadDetail {
    pub thread: ThreadSummary,
    pub messages: Vec<Message>,
}

/// Downloaded file contents plus the worker's reported type and name.
#[derive(Debug, Clone)]
pub struct Download {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
    pub filename: Option<String>,
}
