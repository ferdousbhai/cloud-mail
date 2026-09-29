use serde::Serialize;
use serde::de::DeserializeOwned;
use std::time::Duration;

use crate::config::Config;
use crate::error::{Error, ErrorKind, Result};
use crate::types::*;

const MAX_BODY: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct ThreadQuery {
    /// inbox, archive, screener, sent, blocked or all
    pub folder: String,
    pub q: Option<String>,
    pub before: Option<i64>,
    pub since: Option<i64>,
    pub limit: u32,
}

#[derive(Clone)]
pub struct Client {
    base: String,
    token: String,
    agent: ureq::Agent,
}

type HttpResult = std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>;

struct Raw {
    bytes: Vec<u8>,
    content_type: Option<String>,
    disposition: Option<String>,
}

impl Client {
    pub fn new(config: &Config) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .build()
            .into();
        Self {
            base: config.api_url.trim_end_matches('/').to_string(),
            token: config.api_token.clone(),
            agent,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.token)
    }

    fn finish(resp: HttpResult) -> Result<Raw> {
        let mut resp = resp.map_err(|e| Error::new(ErrorKind::Network, e.to_string()))?;
        let status = resp.status();
        let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
        let content_type = header("content-type");
        let disposition = header("content-disposition");
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_vec()
            .map_err(|e| Error::new(ErrorKind::Network, e.to_string()))?;
        if status.is_success() {
            return Ok(Raw { bytes, content_type, disposition });
        }
        #[derive(serde::Deserialize)]
        struct ApiError {
            error: String,
        }
        let message = serde_json::from_slice::<ApiError>(&bytes)
            .map(|e| e.error)
            .unwrap_or_else(|_| format!("HTTP {}", status.as_u16()));
        let kind = match status.as_u16() {
            401 | 403 => ErrorKind::Unauthorized,
            404 => ErrorKind::NotFound,
            400 | 422 => ErrorKind::BadRequest,
            _ => ErrorKind::Api,
        };
        Err(Error { kind, message, status: Some(status.as_u16()) })
    }

    fn decode<T: DeserializeOwned>(raw: Raw) -> Result<T> {
        serde_json::from_slice(&raw.bytes).map_err(|e| Error::new(ErrorKind::Decode, format!("bad response: {e}")))
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        Self::decode(self.get_raw(path)?)
    }

    fn get_raw(&self, path: &str) -> Result<Raw> {
        Self::finish(self.agent.get(self.url(path)).header("Authorization", self.auth()).call())
    }

    fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        Self::decode(Self::finish(
            self.agent.post(self.url(path)).header("Authorization", self.auth()).send_json(body),
        )?)
    }

    fn put<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        Self::decode(Self::finish(
            self.agent.put(self.url(path)).header("Authorization", self.auth()).send_json(body),
        )?)
    }

    fn patch<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        Self::decode(Self::finish(
            self.agent.patch(self.url(path)).header("Authorization", self.auth()).send_json(body),
        )?)
    }

    fn delete(&self, path: &str) -> Result<()> {
        Self::finish(self.agent.delete(self.url(path)).header("Authorization", self.auth()).call()).map(drop)
    }

    fn enc(s: &str) -> String {
        urlencoding::encode(s).into_owned()
    }

    /// Unauthenticated liveness check.
    pub fn health(&self) -> Result<()> {
        Self::finish(self.agent.get(self.url("/health")).call()).map(drop)
    }

    pub fn counts(&self) -> Result<Counts> {
        self.get("/api/counts")
    }

    pub fn threads(&self, folder: &str, query: Option<&str>, before: Option<i64>, limit: u32) -> Result<Vec<ThreadSummary>> {
        self.list_threads(&ThreadQuery {
            folder: folder.to_string(),
            q: query.map(str::to_string),
            before,
            since: None,
            limit,
        })
    }

    pub fn list_threads(&self, q: &ThreadQuery) -> Result<Vec<ThreadSummary>> {
        #[derive(serde::Deserialize)]
        struct Threads {
            threads: Vec<ThreadSummary>,
        }
        let folder = if q.folder.is_empty() { "inbox" } else { &q.folder };
        let mut path = format!("/api/threads?folder={}&limit={}", Self::enc(folder), q.limit.max(1));
        if let Some(text) = q.q.as_deref().filter(|s| !s.trim().is_empty()) {
            path.push_str(&format!("&q={}", Self::enc(text)));
        }
        if let Some(b) = q.before {
            path.push_str(&format!("&before={b}"));
        }
        if let Some(s) = q.since {
            path.push_str(&format!("&since={s}"));
        }
        self.get::<Threads>(&path).map(|t| t.threads)
    }

    pub fn thread(&self, id: &str) -> Result<ThreadDetail> {
        self.get(&format!("/api/threads/{}", Self::enc(id)))
    }

    /// Moves a thread to "inbox" or "archive".
    pub fn move_thread(&self, id: &str, folder: &str) -> Result<()> {
        self.post::<serde_json::Value>(
            &format!("/api/threads/{}/move", Self::enc(id)),
            &serde_json::json!({ "folder": folder }),
        )
        .map(drop)
    }

    pub fn set_unread(&self, id: &str, unread: bool) -> Result<()> {
        self.post::<serde_json::Value>(
            &format!("/api/threads/{}/read", Self::enc(id)),
            &serde_json::json!({ "unread": unread }),
        )
        .map(drop)
    }

    pub fn delete_thread(&self, id: &str) -> Result<()> {
        self.delete(&format!("/api/threads/{}", Self::enc(id)))
    }

    pub fn screener(&self) -> Result<Vec<PendingSender>> {
        #[derive(serde::Deserialize)]
        struct Senders {
            senders: Vec<PendingSender>,
        }
        self.get::<Senders>("/api/screener").map(|s| s.senders)
    }

    /// Screens a sender in ("approved") or out ("blocked"); returns how many threads moved.
    pub fn decide_sender(&self, email: &str, status: &str) -> Result<i64> {
        #[derive(serde::Deserialize)]
        struct Decided {
            #[serde(default)]
            moved: i64,
        }
        self.post::<Decided>(
            &format!("/api/senders/{}", Self::enc(email)),
            &serde_json::json!({ "status": status }),
        )
        .map(|d| d.moved)
    }

    pub fn senders(&self, status: &str) -> Result<Vec<Sender>> {
        #[derive(serde::Deserialize)]
        struct Senders {
            senders: Vec<Sender>,
        }
        self.get::<Senders>(&format!("/api/senders?status={}", Self::enc(status))).map(|s| s.senders)
    }

    pub fn send(&self, req: &SendRequest) -> Result<SendResponse> {
        self.post("/api/send", req)
    }

    pub fn attachment(&self, id: &str) -> Result<Vec<u8>> {
        self.download_attachment(id).map(|d| d.bytes)
    }

    pub fn download_attachment(&self, id: &str) -> Result<Download> {
        let raw = self.get_raw(&format!("/api/attachments/{}", Self::enc(id)))?;
        Ok(Download {
            filename: raw.disposition.as_deref().and_then(filename_from_disposition),
            content_type: raw.content_type,
            bytes: raw.bytes,
        })
    }

    /// The original .eml of a received message.
    pub fn raw_message(&self, id: &str) -> Result<Vec<u8>> {
        self.get_raw(&format!("/api/messages/{}/raw", Self::enc(id))).map(|r| r.bytes)
    }

    pub fn identities(&self) -> Result<Identities> {
        self.get("/api/identities")
    }

    pub fn mailboxes(&self) -> Result<Vec<Mailbox>> {
        #[derive(serde::Deserialize)]
        struct Mailboxes {
            mailboxes: Vec<Mailbox>,
        }
        self.get::<Mailboxes>("/api/mailboxes").map(|m| m.mailboxes)
    }

    /// Creates or updates a mailbox.
    pub fn put_mailbox(&self, email: &str, update: &MailboxUpdate) -> Result<Mailbox> {
        #[derive(serde::Deserialize)]
        struct Put {
            mailbox: Mailbox,
        }
        self.put::<Put>(&format!("/api/mailboxes/{}", Self::enc(email)), update).map(|p| p.mailbox)
    }

    pub fn delete_mailbox(&self, email: &str) -> Result<()> {
        self.delete(&format!("/api/mailboxes/{}", Self::enc(email)))
    }

    pub fn settings(&self) -> Result<Settings> {
        #[derive(serde::Deserialize)]
        struct Wrapped {
            settings: Settings,
        }
        self.get::<Wrapped>("/api/settings").map(|w| w.settings)
    }

    pub fn update_settings(&self, patch: &serde_json::Value) -> Result<Settings> {
        #[derive(serde::Deserialize)]
        struct Wrapped {
            settings: Settings,
        }
        self.patch::<Wrapped>("/api/settings", patch).map(|w| w.settings)
    }
}

/// Extracts the filename from a Content-Disposition header (RFC 5987 `filename*` preferred).
pub fn filename_from_disposition(value: &str) -> Option<String> {
    let mut plain = None;
    for part in value.split(';').map(str::trim) {
        if let Some(v) = part.strip_prefix("filename*=") {
            let encoded = v.split_once("''").map(|(_, rest)| rest).unwrap_or(v);
            if let Ok(decoded) = urlencoding::decode(encoded.trim_matches('"')) {
                return Some(decoded.into_owned());
            }
        } else if let Some(v) = part.strip_prefix("filename=") {
            plain = Some(v.trim_matches('"').to_string());
        }
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_disposition() {
        assert_eq!(
            filename_from_disposition("attachment; filename*=UTF-8''menu%202.pdf").as_deref(),
            Some("menu 2.pdf")
        );
        assert_eq!(filename_from_disposition("attachment; filename=\"m_1.eml\"").as_deref(), Some("m_1.eml"));
        assert_eq!(filename_from_disposition("inline"), None);
    }
}
