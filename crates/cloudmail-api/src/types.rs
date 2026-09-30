use serde::{Deserialize, Deserializer, Serialize};

fn empty_as_none<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<String>::deserialize(d)?.filter(|s| !s.is_empty()))
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Address {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: String,
}

impl Address {
    fn trimmed_name(&self) -> Option<&str> {
        self.name.as_deref().map(str::trim).filter(|n| !n.is_empty())
    }

    pub fn display(&self) -> String {
        self.trimmed_name().map_or_else(|| self.email.clone(), str::to_string)
    }

    /// "Name <email>" suitable for an address field.
    pub fn formatted(&self) -> String {
        self.trimmed_name().map_or_else(|| self.email.clone(), |n| format!("{n} <{}>", self.email))
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
    /// The mailbox the thread came to; the worker sends "" when it doesn't know, read as None.
    #[serde(deserialize_with = "empty_as_none")]
    pub to_address: Option<String>,
    pub message_count: i64,
    pub unread: bool,
    pub has_attachments: bool,
    pub last_at: i64,
}

impl ThreadSummary {
    pub fn sender(&self) -> Option<String> {
        self.from.as_ref().map(Address::display)
    }

    pub fn is_from(&self, email: &str) -> bool {
        self.from.as_ref().is_some_and(|a| a.email.eq_ignore_ascii_case(email))
    }
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
    /// Who a reply goes to: the original recipients of your own message, else Reply-To, else From.
    pub fn reply_recipients(&self) -> &[Address] {
        if self.outgoing {
            &self.to
        } else if !self.reply_to.is_empty() {
            &self.reply_to
        } else {
            std::slice::from_ref(&self.from)
        }
    }

    /// The plain-text body, converted from the HTML when there is no text part.
    pub fn plain_text(&self) -> String {
        match (&self.text, &self.html) {
            (Some(t), _) if !t.trim().is_empty() => t.clone(),
            (_, Some(h)) => crate::text::html_to_text(h),
            _ => String::new(),
        }
    }

    /// The From address may be forged: the worker couldn't authenticate it (see
    /// `MessageAuth::verified`). Older stored mail falls back to "DMARC gave anything but pass or none".
    pub fn unverified(&self) -> bool {
        let Some(auth) = self.auth.as_ref() else { return false };
        match auth.verified {
            Some(verified) => !verified,
            None => auth.dmarc.as_deref().is_some_and(|d| !d.eq_ignore_ascii_case("pass") && !d.eq_ignore_ascii_case("none")),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct MessageAuth {
    pub dmarc: Option<String>,
    pub spf: Option<String>,
    pub dkim: Option<String>,
    pub spam_score: Option<f64>,
    /// The worker's verdict on the From address: DMARC pass, or (no DMARC policy) aligned DKIM/SPF.
    /// Absent on mail stored by older workers.
    pub verified: Option<bool>,
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
    pub fn address(&self) -> Address {
        Address { name: self.name.clone(), email: self.email.clone() }
    }

    pub fn display(&self) -> String {
        self.address().display()
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
    /// The thread the sent message was saved in. If it went out but couldn't be saved (see
    /// `warning`): the replied-to thread, or absent for a new message.
    #[serde(default)]
    pub thread_id: Option<String>,
    pub message: Option<Message>,
    /// Set when the mail was sent but something after sending failed; the send itself succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ThreadDetail {
    pub thread: ThreadSummary,
    pub messages: Vec<Message>,
}

impl ThreadDetail {
    /// The message a reply answers: the latest one you received, else the latest one.
    pub fn reply_target(&self) -> Option<&Message> {
        self.messages.iter().rev().find(|m| !m.outgoing).or(self.messages.last())
    }
}

/// Downloaded file contents plus the worker's reported type and name.
#[derive(Debug, Clone)]
pub struct Download {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
    pub filename: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_to_address_reads_as_none() {
        let t: ThreadSummary = serde_json::from_str(r#"{"id":"t_1","to_address":""}"#).unwrap();
        assert_eq!(t.to_address, None);
        let t: ThreadSummary = serde_json::from_str(r#"{"id":"t_1","to_address":"hi@example.com"}"#).unwrap();
        assert_eq!(t.to_address.as_deref(), Some("hi@example.com"));
        let t: ThreadSummary = serde_json::from_str(r#"{"id":"t_1"}"#).unwrap();
        assert_eq!(t.to_address, None);
    }
}
