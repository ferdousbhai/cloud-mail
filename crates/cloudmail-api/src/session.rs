//! icloud-session (icloud-for-omarchy), the D-Bus user service that owns this machine's one
//! signed-in iCloud web session: cloudmail asks it for the session and sends iCloud.com's own
//! requests with it, as icloud.com's Mail does. cloudmail never sees an Apple password.
//!
//! The contract is icloud-session's D-Bus interface (its `session/README.md`): bus and interface
//! `io.github.ferdousbhai.ICloudSession` at `/io/github/ferdousbhai/ICloudSession`; properties
//! `SignedIn`, `AppleId`, `FullName`, `Dsid`, `SigningIn`; `Session()` → `(cookie_header,
//! client_params, webservices)` with errors `…Error.SignInRequired` and `…Error.KeyringUnavailable`;
//! `MergeCookies(as)` for the `Set-Cookie`s Apple sends back; `ReportSignInRequired()` → `b` after
//! a 421/401 (true: fetch `Session()` and retry once); `SignIn()` opens its sign-in window.
//!
//! Requests carry what its own client library sends (icloud-for-omarchy `session/src/lib.rs`):
//! the cookie header, `Origin`/`Referer: https://www.icloud.com`, and the client params
//! (`clientBuildNumber`, `clientMasteringNumber`, `clientId`, `dsid`) in the query.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use zbus::blocking::Connection;
use zbus::zvariant::OwnedValue;

use crate::error::{Error, ErrorKind, Result};

pub const BUS_NAME: &str = "io.github.ferdousbhai.ICloudSession";
pub const OBJECT_PATH: &str = "/io/github/ferdousbhai/ICloudSession";
pub const INTERFACE: &str = "io.github.ferdousbhai.ICloudSession";
const ERROR_SIGN_IN_REQUIRED: &str = "io.github.ferdousbhai.ICloudSession.Error.SignInRequired";
const ERROR_KEYRING_UNAVAILABLE: &str = "io.github.ferdousbhai.ICloudSession.Error.KeyringUnavailable";
const ORIGIN: &str = "https://www.icloud.com";
const REFERER: &str = "https://www.icloud.com/";
const TIMEOUT: Duration = Duration::from_secs(60);
const CLIENT_PARAMS: &[&str] = &["clientBuildNumber", "clientMasteringNumber", "clientId", "dsid"];

/// The session properties.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    pub signed_in: bool,
    pub signing_in: bool,
    pub apple_id: String,
    pub full_name: String,
    pub dsid: String,
}

/// One `Session()` answer.
#[derive(Debug, Clone)]
struct Jar {
    cookie: String,
    params: HashMap<String, String>,
    webservices: HashMap<String, String>,
}

enum Payload {
    Json(Vec<u8>),
    Bytes(Vec<u8>),
}

/// An HTTP answer from Apple.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

pub struct Session {
    bus: Mutex<Option<Connection>>,
    jar: Mutex<Option<Jar>>,
    agent: ureq::Agent,
}

fn unavailable(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::AccountUnavailable, message)
}

pub fn sign_in_required() -> Error {
    Error::new(
        ErrorKind::AccountAuth,
        "iCloud isn't signed in on this computer: sign in with icloud-session (`cloudmail account login icloud`)",
    )
}

fn dbus_error(e: zbus::Error) -> Error {
    match &e {
        zbus::Error::MethodError(name, _, _) if name.as_str() == ERROR_SIGN_IN_REQUIRED => sign_in_required(),
        zbus::Error::MethodError(name, msg, _) if name.as_str() == ERROR_KEYRING_UNAVAILABLE => unavailable(format!(
            "icloud-session can't read its keyring ({}); unlock GNOME Keyring (or start a Secret Service) and try again",
            msg.as_deref().unwrap_or("no reason given")
        )),
        zbus::Error::MethodError(name, _, _)
            if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown"
                || name.as_str() == "org.freedesktop.DBus.Error.NameHasNoOwner" =>
        {
            unavailable("icloud-session isn't installed or running (it comes with icloud-for-omarchy)")
        }
        zbus::Error::MethodError(name, msg, _) => {
            unavailable(format!("icloud-session: {}: {}", name.as_str(), msg.as_deref().unwrap_or("")))
        }
        _ => unavailable(format!("can't reach icloud-session on the session bus: {e}")),
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub fn new() -> Self {
        let agent: ureq::Agent =
            ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(TIMEOUT)).build().into();
        Self { bus: Mutex::new(None), jar: Mutex::new(None), agent }
    }

    fn bus(&self) -> Result<Connection> {
        let mut slot = self.bus.lock().unwrap();
        if let Some(c) = &*slot {
            return Ok(c.clone());
        }
        let c = Connection::session().map_err(|e| unavailable(format!("no D-Bus session bus ({e})")))?;
        *slot = Some(c.clone());
        Ok(c)
    }

    fn call<R>(&self, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> Result<R>
    where
        R: for<'d> serde::Deserialize<'d> + zbus::zvariant::Type,
    {
        let bus = self.bus()?;
        let reply = bus.call_method(Some(BUS_NAME), OBJECT_PATH, Some(INTERFACE), method, body).map_err(dbus_error)?;
        reply
            .body()
            .deserialize()
            .map_err(|e| unavailable(format!("icloud-session answered {method} with something unexpected ({e})")))
    }

    /// The session's properties.
    pub fn status(&self) -> Result<Status> {
        let all: HashMap<String, OwnedValue> = self.call_properties()?;
        let text = |k: &str| all.get(k).and_then(|v| <&str>::try_from(v).ok()).unwrap_or("").to_string();
        let flag = |k: &str| all.get(k).and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
        Ok(Status {
            signed_in: flag("SignedIn"),
            signing_in: flag("SigningIn"),
            apple_id: text("AppleId"),
            full_name: text("FullName"),
            dsid: text("Dsid"),
        })
    }

    fn call_properties(&self) -> Result<HashMap<String, OwnedValue>> {
        let bus = self.bus()?;
        let reply = bus
            .call_method(Some(BUS_NAME), OBJECT_PATH, Some("org.freedesktop.DBus.Properties"), "GetAll", &(INTERFACE,))
            .map_err(dbus_error)?;
        reply.body().deserialize().map_err(|e| unavailable(format!("icloud-session's properties are unreadable ({e})")))
    }

    /// Asks icloud-session to open its sign-in window; returns at once.
    pub fn sign_in(&self) -> Result<()> {
        self.call::<()>("SignIn", &())
    }

    fn jar(&self) -> Result<Jar> {
        if let Some(j) = self.jar.lock().unwrap().clone() {
            return Ok(j);
        }
        let (cookie, mut params, webservices): (String, HashMap<String, String>, HashMap<String, String>) =
            self.call("Session", &())?;
        // The client params captured at sign-in carry no dsid; the daemon has it as the `Dsid`
        // property, which its own client library appends the same way.
        if params.get("dsid").is_none_or(|d| d.is_empty()) {
            let dsid = self.status()?.dsid;
            if !dsid.is_empty() {
                params.insert("dsid".into(), dsid);
            }
        }
        let jar = Jar { cookie, params, webservices };
        *self.jar.lock().unwrap() = Some(jar.clone());
        Ok(jar)
    }

    /// The base URL of one of Apple's webservices (as `/validate` named it), without a trailing slash.
    pub fn webservice(&self, key: &str) -> Result<String> {
        let jar = self.jar()?;
        jar.webservices.get(key).map(|u| u.trim_end_matches('/').to_string()).ok_or_else(|| {
            unavailable(format!("this iCloud account has no `{key}` webservice (is iCloud Mail turned on for it?)"))
        })
    }

    pub fn dsid(&self) -> Result<String> {
        let jar = self.jar()?;
        jar.params
            .get("dsid")
            .cloned()
            .filter(|d| !d.is_empty())
            .ok_or_else(|| unavailable("icloud-session gave no dsid"))
    }

    /// Sends a request with the session. On 421/401, icloud-session confirms with Apple: a session
    /// it renewed gets one retry; one that ended is `AccountAuth`.
    pub fn send(&self, method: &str, url: &str, json: Option<&serde_json::Value>) -> Result<Reply> {
        let body = json.map(|v| Payload::Json(serde_json::to_vec(v).unwrap_or_default()));
        self.send_payload(method, url, body.as_ref())
    }

    /// `POST`s raw bytes (an upload) with the session.
    pub fn post_bytes(&self, url: &str, bytes: &[u8]) -> Result<Reply> {
        self.send_payload("POST", url, Some(&Payload::Bytes(bytes.to_vec())))
    }

    fn send_payload(&self, method: &str, url: &str, body: Option<&Payload>) -> Result<Reply> {
        for attempt in 0..2 {
            let jar = self.jar()?;
            let reply = self.send_once(&jar, method, url, body)?;
            if reply.status != 421 && reply.status != 401 {
                return Ok(reply);
            }
            *self.jar.lock().unwrap() = None;
            let still: bool = self.call("ReportSignInRequired", &())?;
            if !still || attempt == 1 {
                return Err(sign_in_required());
            }
        }
        unreachable!("the loop returns")
    }

    fn send_once(&self, jar: &Jar, method: &str, url: &str, body: Option<&Payload>) -> Result<Reply> {
        let mut full = url::Url::parse(url).map_err(|e| unavailable(format!("bad webservice URL {url}: {e}")))?;
        {
            let present: Vec<String> = full.query_pairs().map(|(k, _)| k.into_owned()).collect();
            let mut q = full.query_pairs_mut();
            for k in CLIENT_PARAMS {
                if let Some(v) = jar.params.get(*k).filter(|_| !present.iter().any(|p| p == k)) {
                    q.append_pair(k, v);
                }
            }
        }
        let host = full.host_str().unwrap_or("").to_string();
        let result = match body {
            _ if method == "GET" => self
                .agent
                .get(full.as_str())
                .header("Cookie", &jar.cookie)
                .header("Origin", ORIGIN)
                .header("Referer", REFERER)
                .call(),
            body => {
                let (content_type, bytes): (&str, &[u8]) = match body {
                    Some(Payload::Json(b)) => ("application/json", b),
                    Some(Payload::Bytes(b)) => ("application/octet-stream", b),
                    None => ("application/json", b"null"),
                };
                self.agent
                    .post(full.as_str())
                    .header("Cookie", &jar.cookie)
                    .header("Origin", ORIGIN)
                    .header("Referer", REFERER)
                    .header("Content-Type", content_type)
                    .header("Accept", "application/json")
                    .send(bytes)
            }
        };
        let mut response = result.map_err(|e| unavailable(format!("can't reach {host} ({e}); offline?")))?;
        let set_cookies: Vec<String> = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_string)
            .collect();
        if !set_cookies.is_empty() {
            // Rotated cookies go back to their owner; the next request asks for the new jar.
            let refs: Vec<&str> = set_cookies.iter().map(String::as_str).collect();
            self.call::<()>("MergeCookies", &(refs,))?;
            *self.jar.lock().unwrap() = None;
        }
        let status = response.status().as_u16();
        let content_type = response.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);
        let body = response
            .body_mut()
            .with_config()
            .limit(96 * 1024 * 1024)
            .read_to_vec()
            .map_err(|e| unavailable(format!("{host}: {e}")))?;
        Ok(Reply { status, content_type, body })
    }
}

/// Watches `SigningIn`/`SignedIn` until a sign-in started with `sign_in` ends; whether it signed in.
pub fn wait_for_sign_in(session: &Session, timeout: Duration) -> Result<bool> {
    let deadline = std::time::Instant::now() + timeout;
    let mut seen_window = false;
    loop {
        let s = session.status()?;
        if s.signed_in {
            return Ok(true);
        }
        seen_window |= s.signing_in;
        if (seen_window && !s.signing_in) || std::time::Instant::now() > deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}
