//! Stand-ins for what cloudmail talks to on the session bus and at Apple, so tests never reach the
//! real ones: a private dbus-daemon, a Secret Service on it, icloud-session on it, and iCloud
//! Mail's web services (the requests icloud.com's Mail app sends, as `icloud.rs` documents them).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use zbus::object_server::ObjectServer;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value as ZValue};

// ---------- the bus ----------

/// A dbus-daemon of the test's own, with nothing activatable on it.
pub struct Bus {
    child: Child,
    pub address: String,
    conns: Vec<zbus::blocking::Connection>,
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Bus {
    pub fn start(dir: &Path) -> Bus {
        std::fs::create_dir_all(dir).unwrap();
        let conf = dir.join("bus.conf");
        std::fs::write(
            &conf,
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=/tmp</listen>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
        )
        .unwrap();
        let mut child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", conf.display()))
            .args(["--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon runs (the tests need the dbus package)");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        Bus { child, address: line.trim().to_string(), conns: Vec::new() }
    }

    fn serve(
        &mut self,
        name: &str,
        build: impl FnOnce(zbus::blocking::connection::Builder<'static>) -> zbus::blocking::connection::Builder<'static>,
    ) {
        let builder = zbus::blocking::connection::Builder::address(self.address.as_str())
            .unwrap()
            .name(name.to_string())
            .unwrap();
        self.conns.push(build(builder).build().unwrap());
    }
}

// ---------- the Secret Service ----------

const SECRETS: &str = "/org/freedesktop/secrets";
const LOGIN: &str = "/org/freedesktop/secrets/collection/login";

#[derive(Debug, Clone)]
pub struct Secret {
    pub path: String,
    pub attributes: HashMap<String, String>,
    pub value: String,
}

pub type Keyring = Arc<Mutex<Vec<Secret>>>;

fn opath(p: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(p.to_string()).unwrap()
}

struct SecretService;

#[zbus::interface(name = "org.freedesktop.Secret.Service")]
impl SecretService {
    fn open_session(&self, algorithm: String, _input: OwnedValue) -> zbus::fdo::Result<(OwnedValue, OwnedObjectPath)> {
        if algorithm != "plain" {
            return Err(zbus::fdo::Error::NotSupported(algorithm));
        }
        Ok((OwnedValue::try_from(ZValue::from("")).unwrap(), opath("/org/freedesktop/secrets/session/s1")))
    }

    fn read_alias(&self, name: String) -> OwnedObjectPath {
        opath(if name == "default" { LOGIN } else { "/" })
    }

    fn unlock(&self, objects: Vec<OwnedObjectPath>) -> (Vec<OwnedObjectPath>, OwnedObjectPath) {
        (objects, opath("/"))
    }
}

struct Collection(Keyring);

#[zbus::interface(name = "org.freedesktop.Secret.Collection")]
impl Collection {
    fn search_items(&self, attributes: HashMap<String, String>) -> Vec<OwnedObjectPath> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|s| attributes.iter().all(|(k, v)| s.attributes.get(k) == Some(v)))
            .map(|s| opath(&s.path))
            .collect()
    }

    async fn create_item(
        &self,
        properties: HashMap<String, OwnedValue>,
        secret: (OwnedObjectPath, Vec<u8>, Vec<u8>, String),
        replace: bool,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> (OwnedObjectPath, OwnedObjectPath) {
        let label = String::try_from(properties["org.freedesktop.Secret.Item.Label"].clone()).unwrap();
        let attributes =
            HashMap::<String, String>::try_from(properties["org.freedesktop.Secret.Item.Attributes"].clone()).unwrap();
        let path = {
            let mut items = self.0.lock().unwrap();
            if replace {
                items.retain(|s| s.attributes != attributes);
            }
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
            let path = format!("{LOGIN}/i{}", NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst));
            assert!(!label.is_empty(), "items are labelled for the keyring's own UI");
            items.push(Secret { path: path.clone(), attributes, value: String::from_utf8(secret.2).unwrap() });
            path
        };
        server.at(path.as_str(), Item(self.0.clone(), path.clone())).await.unwrap();
        (opath(&path), opath("/"))
    }

    #[zbus(property)]
    fn locked(&self) -> bool {
        false
    }
}

struct Item(Keyring, String);

#[zbus::interface(name = "org.freedesktop.Secret.Item")]
impl Item {
    fn get_secret(&self, session: OwnedObjectPath) -> zbus::fdo::Result<(OwnedObjectPath, Vec<u8>, Vec<u8>, String)> {
        let items = self.0.lock().unwrap();
        let s =
            items.iter().find(|s| s.path == self.1).ok_or_else(|| zbus::fdo::Error::UnknownObject(self.1.clone()))?;
        Ok((session, Vec::new(), s.value.clone().into_bytes(), "text/plain".into()))
    }

    fn delete(&self) -> OwnedObjectPath {
        self.0.lock().unwrap().retain(|s| s.path != self.1);
        opath("/")
    }

    #[zbus(property)]
    fn locked(&self) -> bool {
        false
    }
}

/// A Secret Service on the bus, holding these secrets (stored base64, as cloudmail stores them).
pub fn keyring(bus: &mut Bus, items: &[(&str, &str)]) -> Keyring {
    let store: Keyring = Arc::default();
    {
        let mut st = store.lock().unwrap();
        for (i, (name, value)) in items.iter().enumerate() {
            st.push(Secret {
                path: format!("{LOGIN}/seed{i}"),
                attributes: HashMap::from([
                    ("application".into(), "cloudmail".into()),
                    ("secret".into(), (*name).into()),
                ]),
                value: STANDARD.encode(value),
            });
        }
    }
    let seeded: Vec<String> = store.lock().unwrap().iter().map(|s| s.path.clone()).collect();
    let s = store.clone();
    bus.serve("org.freedesktop.secrets", move |mut b| {
        b = b.serve_at(SECRETS, SecretService).unwrap().serve_at(LOGIN, Collection(s.clone())).unwrap();
        for p in seeded {
            b = b.serve_at(p.clone(), Item(s.clone(), p)).unwrap();
        }
        b
    });
    store
}

/// The secret named `name`, as cloudmail stores it.
pub fn secret(store: &Keyring, name: &str) -> Option<String> {
    store
        .lock()
        .unwrap()
        .iter()
        .find(|s| {
            s.attributes.get("application").map(String::as_str) == Some("cloudmail")
                && s.attributes.get("secret").map(String::as_str) == Some(name)
        })
        .map(|s| String::from_utf8(STANDARD.decode(&s.value).unwrap()).unwrap())
}

// ---------- icloud-session ----------

pub const COOKIE: &str = "X-APPLE-WEBAUTH-TOKEN=fake; X-APPLE-WEBAUTH-USER=fake";

#[derive(Debug, Default)]
pub struct SessionState {
    pub signed_in: bool,
    pub signing_in: bool,
    pub full_name: String,
    /// `Session()` fails as icloud-session's does when its keyring can't be read.
    pub keyring_locked: bool,
    /// What `ReportSignInRequired()` answers.
    pub still_signed_in: bool,
    /// `SignIn()` signs in (else the window just closes).
    pub sign_in_works: bool,
    pub webservices: HashMap<String, String>,
    pub calls: Vec<String>,
    pub merged: Vec<String>,
}

pub type SessionShared = Arc<Mutex<SessionState>>;
type SessionReply = (String, HashMap<String, String>, HashMap<String, String>);

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "io.github.ferdousbhai.ICloudSession.Error")]
enum SessionError {
    #[zbus(error)]
    ZBus(zbus::Error),
    SignInRequired(String),
    KeyringUnavailable(String),
}

struct FakeSession(SessionShared);

#[zbus::interface(name = "io.github.ferdousbhai.ICloudSession")]
impl FakeSession {
    #[zbus(property, name = "SignedIn")]
    fn signed_in(&self) -> bool {
        self.0.lock().unwrap().signed_in
    }
    #[zbus(property, name = "SigningIn")]
    fn signing_in(&self) -> bool {
        self.0.lock().unwrap().signing_in
    }
    #[zbus(property, name = "AppleId")]
    fn apple_id(&self) -> String {
        if self.0.lock().unwrap().signed_in { "ann@example.com".into() } else { String::new() }
    }
    #[zbus(property, name = "FullName")]
    fn full_name(&self) -> String {
        self.0.lock().unwrap().full_name.clone()
    }
    #[zbus(property, name = "Dsid")]
    fn dsid(&self) -> String {
        if self.0.lock().unwrap().signed_in { "1234".into() } else { String::new() }
    }

    #[zbus(name = "Session")]
    fn session(&self) -> Result<SessionReply, SessionError> {
        let mut st = self.0.lock().unwrap();
        st.calls.push("Session".into());
        if !st.signed_in {
            return Err(SessionError::SignInRequired("sign in to iCloud required".into()));
        }
        if st.keyring_locked {
            return Err(SessionError::KeyringUnavailable("the login keyring is locked".into()));
        }
        // As the real daemon: the dsid is only the `Dsid` property, never a client param.
        let params = HashMap::from([
            ("clientBuildNumber".into(), "2636Hotfix65".into()),
            ("clientMasteringNumber".into(), "2636Hotfix65".into()),
            ("clientId".into(), "test-client".into()),
        ]);
        Ok((COOKIE.into(), params, st.webservices.clone()))
    }

    #[zbus(name = "MergeCookies")]
    fn merge_cookies(&self, set_cookies: Vec<String>) {
        self.0.lock().unwrap().merged.extend(set_cookies);
    }

    #[zbus(name = "ReportSignInRequired")]
    fn report_sign_in_required(&self) -> bool {
        let mut st = self.0.lock().unwrap();
        st.calls.push("ReportSignInRequired".into());
        if !st.still_signed_in {
            st.signed_in = false;
        }
        st.still_signed_in
    }

    #[zbus(name = "SignIn")]
    fn sign_in(&self) {
        let mut st = self.0.lock().unwrap();
        st.calls.push("SignIn".into());
        st.signed_in = st.sign_in_works;
    }

    #[zbus(name = "SignOut")]
    fn sign_out(&self) {
        self.0.lock().unwrap().calls.push("SignOut".into());
    }
}

/// icloud-session on the bus, its webservices at `mail_url`.
pub fn icloud_session(bus: &mut Bus, mail_url: &str, signed_in: bool) -> SessionShared {
    let st: SessionShared = Arc::new(Mutex::new(SessionState {
        signed_in,
        full_name: "Ann Example".into(),
        still_signed_in: true,
        sign_in_works: true,
        webservices: HashMap::from([
            ("mccgateway".into(), format!("{mail_url}/")),
            ("mcc".into(), mail_url.into()),
            ("ckdatabasews".into(), mail_url.into()),
        ]),
        ..Default::default()
    }));
    let s = st.clone();
    bus.serve("io.github.ferdousbhai.ICloudSession", move |b| {
        b.serve_at("/io/github/ferdousbhai/ICloudSession", FakeSession(s)).unwrap()
    });
    st
}

// ---------- iCloud Mail's web services ----------

#[derive(Debug, Clone)]
pub struct FMessage {
    pub uid: String,
    pub folder: String,
    pub thread: String,
    pub message_id: String,
    pub from: String,
    pub to: Vec<String>,
    pub subject: String,
    pub date: i64,
    pub seen: bool,
    pub text: String,
    pub html: Option<String>,
    /// (partId, name, type, bytes)
    pub attachment: Option<(String, String, String, Vec<u8>)>,
    pub references: String,
}

impl FMessage {
    pub fn new(uid: &str, folder: &str, thread: &str, from: &str, subject: &str, date: i64) -> Self {
        FMessage {
            uid: uid.into(),
            folder: folder.into(),
            thread: thread.into(),
            message_id: format!("<{uid}.{thread}@example.com>"),
            from: from.into(),
            to: vec!["Ann Example <ann@icloud.com>".into()],
            subject: subject.into(),
            date,
            seen: true,
            text: format!("Text of {subject}"),
            html: None,
            attachment: None,
            references: String::new(),
        }
    }

    fn long_header(&self) -> String {
        let mut h = format!(
            "From: {}\r\nTo: {}\r\nSubject: {}\r\nMessage-ID: {}\r\n",
            self.from,
            self.to.join(", "),
            self.subject,
            self.message_id
        );
        if !self.references.is_empty() {
            h.push_str(&format!("References: {}\r\n", self.references));
        }
        h
    }

    fn parts(&self) -> Vec<Value> {
        let mut parts = vec![
            json!({ "jsonType": "part", "partId": "1", "contentType": "text/plain", "isAttach": false, "size": self.text.len() }),
        ];
        if let Some(h) = &self.html {
            parts.push(json!({ "jsonType": "part", "partId": "2", "contentType": "text/html", "isAttach": false, "size": h.len() }));
        }
        if let Some((id, name, kind, bytes)) = &self.attachment {
            parts.push(json!({ "jsonType": "part", "partId": id, "contentType": kind, "isAttach": true, "size": bytes.len(), "fileName": name, "disposition": "ATTACHMENT" }));
        }
        parts
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub cookie: String,
    pub body: Value,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct MailState {
    pub messages: Vec<FMessage>,
    pub log: Vec<Request>,
    pub drafts: Vec<Value>,
    pub sent: Vec<Value>,
    pub uploads: Vec<(HashMap<String, String>, Vec<u8>)>,
    pub account: Value,
    /// Every answer is this HTTP status (421: the session has ended).
    pub status: Option<u16>,
    /// Answers aren't JSON.
    pub garbage: bool,
    /// The first answer rotates a cookie.
    pub rotate_cookie: bool,
}

pub type MailShared = Arc<Mutex<MailState>>;

pub struct FakeMail {
    pub url: String,
    pub state: MailShared,
}

impl FakeMail {
    pub fn log(&self, path: &str) -> Vec<Request> {
        self.state.lock().unwrap().log.iter().filter(|r| r.path.ends_with(path)).cloned().collect()
    }

    pub fn seen(&self, uid: &str) -> bool {
        self.state.lock().unwrap().messages.iter().find(|m| m.uid == uid).is_some_and(|m| m.seen)
    }

    pub fn folder_of(&self, uid: &str) -> String {
        self.state.lock().unwrap().messages.iter().find(|m| m.uid == uid).map(|m| m.folder.clone()).unwrap_or_default()
    }
}

fn thread_digest(list: &[&FMessage], folder: &str) -> Value {
    let latest = list.iter().max_by_key(|m| m.date).unwrap();
    let mut flags = Vec::new();
    if list.iter().filter(|m| m.folder == folder).all(|m| m.seen) {
        flags.push("\\Seen");
    }
    if list.iter().any(|m| m.attachment.is_some()) {
        flags.push("\\HasAttachment");
    }
    json!({
        "jsonType": "thread", "threadId": latest.thread, "timestamp": latest.date, "count": list.len(), "folderMessageCount": list.iter().filter(|m| m.folder == folder).count(),
        "senders": list.iter().map(|m| m.from.clone()).collect::<Vec<_>>(), "subject": list.iter().min_by_key(|m| m.date).unwrap().subject, "flags": flags,
        "preview": latest.text, "modseq": 1,
    })
}

fn answer(st: &mut MailState, r: &Request) -> (u16, Vec<u8>) {
    let ok = |v: Value| (200, serde_json::to_vec(&v).unwrap());
    let folder = r.body["sessionHeaders"]["folder"].as_str().unwrap_or("").to_string();
    let headers =
        json!({ "folder": folder, "modseq": 1, "threadmodseq": 1, "condstore": 1, "qresync": 1, "threadmode": 1 });
    match (r.method.as_str(), r.path.as_str()) {
        ("POST", "/mailws2/v1/thread/search") => {
            let words = r.body["searchText"].as_str().map(str::to_lowercase);
            let mut threads: Vec<String> =
                st.messages.iter().filter(|m| m.folder == folder).map(|m| m.thread.clone()).collect();
            threads.sort();
            threads.dedup();
            let mut list: Vec<Value> = threads
                .iter()
                .map(|t| st.messages.iter().filter(|m| &m.thread == t).collect::<Vec<_>>())
                .filter(|l| {
                    words.as_ref().is_none_or(|w| {
                        l.iter().any(|m| m.subject.to_lowercase().contains(w) || m.text.to_lowercase().contains(w))
                    })
                })
                .filter(|l| {
                    r.body["filters"]["unseen"] != json!(true) || l.iter().any(|m| m.folder == folder && !m.seen)
                })
                .map(|l| thread_digest(&l, &folder))
                .filter(|d| r.body["before"].as_i64().is_none_or(|b| d["timestamp"].as_i64().unwrap() < b))
                .collect();
            list.sort_by_key(|d| std::cmp::Reverse(d["timestamp"].as_i64().unwrap()));
            list.truncate(r.body["maxResults"].as_u64().unwrap_or(50) as usize);
            ok(
                json!({ "threadList": list, "folderStatus": { "undeletedMessages": 1, "unseenUndeletedMessages": 0 }, "sessionHeaders": headers, "events": [] }),
            )
        }
        ("POST", "/mailws2/v1/thread/get") => {
            let t = r.body["threadId"].as_str().unwrap_or("");
            let list: Vec<Value> = st
                .messages
                .iter()
                .filter(|m| m.thread == t)
                .map(|m| {
                    json!({
                        "uid": m.uid, "folder": m.folder, "messageId": m.message_id, "from": [m.from], "to": m.to, "cc": [], "bcc": [], "subject": m.subject, "date": m.date,
                        "modseq": 1, "size": 100, "flags": if m.seen { vec!["\\Seen"] } else { vec![] }, "parts": m.parts(),
                    })
                })
                .collect();
            ok(json!({ "messageMetadataList": list, "sessionHeaders": headers, "events": [] }))
        }
        ("POST", "/mailws2/v1/message/get") => {
            let uid = r.body["uid"].as_str().unwrap_or("");
            let Some(m) = st.messages.iter_mut().find(|m| m.uid == uid && m.folder == folder) else {
                return (404, b"{}".to_vec());
            };
            if r.body["dontMarkAsRead"] != json!(true) {
                m.seen = true;
            }
            let wanted: Vec<String> = r.body["parts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_str().map(str::to_string))
                .collect();
            let parts: Vec<Value> = wanted
                .iter()
                .filter_map(|p| match p.as_str() {
                    "1" => Some(json!({ "guid": format!("messagepart:{folder}/{uid}-1"), "content": m.text })),
                    "2" => m
                        .html
                        .as_ref()
                        .map(|h| json!({ "guid": format!("messagepart:{folder}/{uid}-2"), "content": h })),
                    _ => None,
                })
                .collect();
            let (header, from, to) = (m.long_header(), vec![m.from.clone()], m.to.clone());
            ok(
                json!({ "guid": format!("message:{folder}/{uid}"), "longHeader": header, "to": to, "from": from, "cc": [], "bcc": [], "contentType": "multipart/mixed", "bimi": {}, "smime": {}, "parts": parts, "sessionHeaders": headers, "events": [] }),
            )
        }
        ("POST", "/mailws2/v1/thread/flag") => {
            let ids: Vec<String> = r.body["threadIds"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect();
            let seen = r.body["method"] == "ADD";
            assert_eq!(r.body["flags"], json!(["SEEN"]));
            for m in st.messages.iter_mut().filter(|m| ids.contains(&m.thread) && m.folder == folder) {
                m.seen = seen;
            }
            ok(json!({ "sessionHeaders": headers }))
        }
        ("POST", "/mailws2/v1/thread/move") => {
            let ids: Vec<String> = r.body["threadIds"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect();
            let dest = r.body["destFolder"].as_str().unwrap_or("").to_string();
            for m in st.messages.iter_mut().filter(|m| ids.contains(&m.thread) && m.folder == folder) {
                m.folder = dest.clone();
            }
            ok(json!({ "sessionHeaders": headers }))
        }
        ("POST", "/mailws2/v1/message/savedraft") => {
            st.drafts.push(r.body.clone());
            ok(json!({ "uid": format!("{}", 76 + st.drafts.len()) }))
        }
        ("POST", "/mailws2/v1/draft/send") => {
            let guid = r.body["messageGuid"].as_str().unwrap_or("");
            match guid
                .strip_prefix("Drafts/")
                .and_then(|u| u.parse::<usize>().ok())
                .and_then(|u| st.drafts.get(u - 77).cloned())
            {
                Some(d) => {
                    st.sent.push(d);
                    ok(json!({}))
                }
                None => (404, br#"{"errorCode":"NOT_FOUND","errorDescription":"no such draft"}"#.to_vec()),
            }
        }
        ("POST", "/mailws2/v1/message/part") => {
            st.uploads.push((r.query.clone(), r.bytes.clone()));
            let n = st.uploads.len();
            ok(
                json!({ "uid": "u", "messagePart": { "guid": format!("cachedpart:{n}"), "datatype": r.query["X-type"], "encoding": "base64", "name": r.query["X-name"], "url": format!("https://p00-mccgateway.icloud.com/mailws2/v1/message/part?guid=cachedpart%3A{n}"), "size": r.bytes.len() } }),
            )
        }
        ("GET", "/mailws2/v1/message/part") => {
            let guid = r.query.get("guid").cloned().unwrap_or_default();
            let found = st.messages.iter().find_map(|m| {
                m.attachment
                    .as_ref()
                    .filter(|(id, ..)| guid == format!("messagepart:{}/{}-{id}", m.folder, m.uid))
                    .map(|(.., b)| b.clone())
            });
            found.map(|b| (200, b)).unwrap_or((404, Vec::new()))
        }
        ("GET", "/mailws2/v1/message/download") => {
            let guid = r.query.get("guid").cloned().unwrap_or_default();
            let found = st
                .messages
                .iter()
                .find(|m| guid == format!("message:{}/{}", m.folder, m.uid))
                .map(|m| format!("{}\r\n{}", m.long_header(), m.text).into_bytes());
            found.map(|b| (200, b)).unwrap_or((404, Vec::new()))
        }
        ("GET", "/cc/mail/v1/account/1234/preference/web/all") => {
            ok(json!({ "account": st.account, "clientPreference": {}, "serverPreference": {} }))
        }
        _ => (404, br#"{"errorCode":"UNKNOWN","errorDescription":"no such endpoint"}"#.to_vec()),
    }
}

/// iCloud Mail's web services, for an account with `ann@icloud.com` (and a custom-domain address).
pub fn icloud_mail(messages: Vec<FMessage>) -> FakeMail {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let state: MailShared = Arc::new(Mutex::new(MailState {
        messages,
        account: json!({
            "emailId": "ann", "fullName": "",
            "supportedDomains": [{ "domain": "icloud.com", "allowSendFrom": true }, { "domain": "me.com", "allowSendFrom": false }],
            "aliases": [], "customDomains": [{ "emailId": "hi", "domain": "ann.dev", "allowSendFrom": true, "fullName": "Ann at Home" }]
        }),
        ..Default::default()
    }));
    let st = state.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let st = st.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut parts = line.split_whitespace();
                    let (method, target) =
                        (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
                    let (mut len, mut cookie) = (0usize, String::new());
                    loop {
                        let mut h = String::new();
                        reader.read_line(&mut h).unwrap();
                        let h = h.trim_end();
                        if h.is_empty() {
                            break;
                        }
                        let (k, v) = h.split_once(':').unwrap_or((h, ""));
                        match k.to_ascii_lowercase().as_str() {
                            "content-length" => len = v.trim().parse().unwrap_or(0),
                            "cookie" => cookie = v.trim().to_string(),
                            _ => {}
                        }
                    }
                    let mut bytes = vec![0; len];
                    reader.read_exact(&mut bytes).unwrap();
                    let (path, q) = target.split_once('?').unwrap_or((&target, ""));
                    let query = url_pairs(q);
                    let req = Request {
                        method,
                        path: path.to_string(),
                        query,
                        cookie,
                        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                        bytes,
                    };
                    let (status, body, set_cookie) = {
                        let mut st = st.lock().unwrap();
                        st.log.push(req.clone());
                        let rotate = std::mem::take(&mut st.rotate_cookie);
                        let (status, body) = if req.cookie != COOKIE {
                            (421, Vec::new())
                        } else if let Some(s) = st.status {
                            (s, Vec::new())
                        } else if st.garbage {
                            (200, b"<html>Service Unavailable</html>".to_vec())
                        } else {
                            answer(&mut st, &req)
                        };
                        (status, body, rotate)
                    };
                    let mut head = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n",
                        body.len()
                    );
                    if set_cookie {
                        head.push_str("set-cookie: X-APPLE-WEBAUTH-TOKEN=rotated; Path=/; Domain=.icloud.com\r\n");
                    }
                    head.push_str("\r\n");
                    if stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(&body)).is_err() {
                        return;
                    }
                }
            });
        }
    });
    FakeMail { url, state }
}

fn url_pairs(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            let d = |s: &str| urlencoding_decode(&s.replace('+', " "));
            (d(k), d(v))
        })
        .collect()
}

fn urlencoding_decode(s: &str) -> String {
    urlencoding::decode(s).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string())
}
