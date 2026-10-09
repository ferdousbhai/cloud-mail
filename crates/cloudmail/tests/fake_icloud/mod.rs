//! A stand-in for iCloud Mail's servers in tests: an IMAP server (implicit TLS, like
//! imap.mail.me.com:993) and an SMTP server (STARTTLS, like smtp.mail.me.com:587) over in-memory
//! mailboxes, with a certificate made for each run. It speaks just the IMAP and SMTP cloudmail
//! uses and records every command, and never talks to Apple.
//!
//! Point cloudmail at it with CLOUDMAIL_ICLOUD_IMAP / _SMTP (`localhost:<port>`) and trust its
//! certificate with CLOUDMAIL_ICLOUD_CA (`Fake::env` has all three).

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const USER: &str = "me@icloud.com";
pub const PASSWORD: &str = "abcd-efgh-ijkl-mnop";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Ok,
    /// Every sign-in is refused, as with a revoked app-specific password.
    BadPassword,
    /// The IMAP server greets with something that isn't IMAP.
    Garbage,
    /// SMTP also files each message in Sent Messages, as some servers do by themselves.
    AutoSent,
}

#[derive(Debug, Clone)]
pub struct Msg {
    pub uid: u32,
    pub flags: Vec<String>,
    pub internal: String,
    pub raw: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Mailbox {
    pub name: String,
    pub validity: u32,
    pub next_uid: u32,
    pub msgs: Vec<Msg>,
}

#[derive(Debug, Clone)]
pub struct Sent {
    pub from: String,
    pub to: Vec<String>,
    pub data: String,
    pub user: String,
}

#[derive(Debug)]
pub struct State {
    pub mode: Mode,
    pub boxes: Vec<Mailbox>,
    /// Every IMAP command, tag left off; literals as `{n}`.
    pub log: Vec<String>,
    pub logins: Vec<String>,
    pub sent: Vec<Sent>,
}

impl State {
    pub fn mailbox(&self, name: &str) -> Option<&Mailbox> {
        self.boxes.iter().find(|b| b.name.eq_ignore_ascii_case(name))
    }

    fn mailbox_mut(&mut self, name: &str) -> Option<&mut Mailbox> {
        self.boxes.iter_mut().find(|b| b.name.eq_ignore_ascii_case(name))
    }

    pub fn add(&mut self, mailbox: &str, flags: &[&str], internal: &str, raw: &str) {
        let b = self.mailbox_mut(mailbox).expect("mailbox");
        let uid = b.next_uid;
        b.next_uid += 1;
        b.msgs.push(Msg { uid, flags: flags.iter().map(|f| f.to_string()).collect(), internal: internal.into(), raw: raw.replace('\n', "\r\n").into_bytes() });
    }
}

pub struct Fake {
    pub state: Arc<Mutex<State>>,
    pub imap_port: u16,
    pub smtp_port: u16,
    pub ca: PathBuf,
}

impl Fake {
    /// Servers over INBOX, Sent Messages, Archive (when `archive`), Drafts and Deleted Messages,
    /// empty; the certificate's PEM goes in `dir`.
    pub fn start(dir: &Path, archive: bool) -> Fake {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let ca = dir.join("fake-icloud-ca.pem");
        std::fs::write(&ca, cert.cert.pem()).unwrap();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
        let tls = Arc::new(
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![CertificateDer::from(cert.cert.der().to_vec())], key)
                .unwrap(),
        );
        let mut names = vec!["INBOX", "Sent Messages", "Drafts", "Deleted Messages"];
        if archive {
            names.push("Archive");
        }
        let boxes = names.iter().enumerate().map(|(i, n)| Mailbox { name: n.to_string(), validity: 100 + i as u32, next_uid: 1, msgs: Vec::new() }).collect();
        let state = Arc::new(Mutex::new(State { mode: Mode::Ok, boxes, log: Vec::new(), logins: Vec::new(), sent: Vec::new() }));
        let imap = TcpListener::bind("127.0.0.1:0").unwrap();
        let smtp = TcpListener::bind("127.0.0.1:0").unwrap();
        let (imap_port, smtp_port) = (imap.local_addr().unwrap().port(), smtp.local_addr().unwrap().port());
        let (s, t) = (state.clone(), tls.clone());
        std::thread::spawn(move || {
            for conn in imap.incoming().flatten() {
                let (s, t) = (s.clone(), t.clone());
                std::thread::spawn(move || imap_session(conn, t, s));
            }
        });
        let (s, t) = (state.clone(), tls);
        std::thread::spawn(move || {
            for conn in smtp.incoming().flatten() {
                let (s, t) = (s.clone(), t.clone());
                std::thread::spawn(move || smtp_session(conn, t, s));
            }
        });
        Fake { state, imap_port, smtp_port, ca }
    }

    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            ("CLOUDMAIL_ICLOUD_IMAP".into(), format!("localhost:{}", self.imap_port)),
            ("CLOUDMAIL_ICLOUD_SMTP".into(), format!("localhost:{}", self.smtp_port)),
            ("CLOUDMAIL_ICLOUD_CA".into(), self.ca.display().to_string()),
        ]
    }

    pub fn set_mode(&self, mode: Mode) {
        self.state.lock().unwrap().mode = mode;
    }

    pub fn log(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }

    pub fn clear_log(&self) {
        self.state.lock().unwrap().log.clear();
    }

    pub fn uids(&self, mailbox: &str) -> Vec<u32> {
        self.state.lock().unwrap().mailbox(mailbox).map(|b| b.msgs.iter().map(|m| m.uid).collect()).unwrap_or_default()
    }

    /// Flags of the message whose raw text contains `needle`, in a mailbox.
    pub fn flags_of(&self, mailbox: &str, needle: &str) -> Vec<String> {
        let st = self.state.lock().unwrap();
        st.mailbox(mailbox).and_then(|b| b.msgs.iter().find(|m| String::from_utf8_lossy(&m.raw).contains(needle))).map(|m| m.flags.clone()).unwrap_or_default()
    }

    pub fn raw_in(&self, mailbox: &str) -> Vec<String> {
        let st = self.state.lock().unwrap();
        st.mailbox(mailbox).map(|b| b.msgs.iter().map(|m| String::from_utf8_lossy(&m.raw).into_owned()).collect()).unwrap_or_default()
    }
}

/// A connection, before or after STARTTLS.
enum Conn {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ServerConnection, TcpStream>>),
    /// While the plain stream is being wrapped in TLS.
    Gone,
}

fn gone() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotConnected, "no connection")
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(s) => s.read(buf),
            Conn::Gone => Err(gone()),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(s) => s.write(buf),
            Conn::Gone => Err(gone()),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
            Conn::Gone => Err(gone()),
        }
    }
}

struct Lines {
    conn: Conn,
    buf: Vec<u8>,
}

impl Lines {
    fn fill(&mut self) -> bool {
        let mut chunk = [0u8; 8192];
        match self.conn.read(&mut chunk) {
            Ok(0) | Err(_) => false,
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                true
            }
        }
    }

    fn line(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(i) = self.buf.windows(2).position(|w| w == b"\r\n") {
                let line = self.buf[..i].to_vec();
                self.buf.drain(..i + 2);
                return Some(line);
            }
            if !self.fill() {
                return None;
            }
        }
    }

    fn bytes(&mut self, n: usize) -> Option<Vec<u8>> {
        while self.buf.len() < n {
            if !self.fill() {
                return None;
            }
        }
        Some(self.buf.drain(..n).collect())
    }

    fn send(&mut self, s: impl AsRef<[u8]>) {
        let _ = self.conn.write_all(s.as_ref());
        let _ = self.conn.flush();
    }
}

// ---------- IMAP ----------

#[derive(Debug, Clone)]
enum Tok {
    Atom(String),
    Str(Vec<u8>),
    /// A parenthesized list, as written.
    List(String),
}

impl Tok {
    fn text(&self) -> String {
        match self {
            Tok::Atom(s) | Tok::List(s) => s.clone(),
            Tok::Str(b) => String::from_utf8_lossy(b).into_owned(),
        }
    }
}

/// Reads one command with its literals (answering each `{n}` with a go-ahead) as tokens.
fn read_command(l: &mut Lines) -> Option<(Vec<Tok>, String)> {
    let mut full = Vec::new();
    let mut logged = String::new();
    loop {
        let line = l.line()?;
        logged.push_str(&String::from_utf8_lossy(&line));
        full.extend_from_slice(&line);
        let text = String::from_utf8_lossy(&line).into_owned();
        if let Some(open) = text.rfind('{')
            && text.ends_with('}')
            && let Ok(n) = text[open + 1..text.len() - 1].parse::<usize>()
        {
            l.send("+ go ahead\r\n");
            let data = l.bytes(n)?;
            // Marks the literal for the tokenizer: \0<len>\0<bytes>.
            full.truncate(full.len() - (text.len() - open));
            full.extend_from_slice(format!("\0{n}\0").as_bytes());
            full.extend_from_slice(&data);
            continue;
        }
        break;
    }
    Some((tokenize(&full), logged))
}

fn tokenize(b: &[u8]) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b' ' => i += 1,
            0 => {
                let end = i + 1 + b[i + 1..].iter().position(|&c| c == 0).unwrap();
                let n: usize = String::from_utf8_lossy(&b[i + 1..end]).parse().unwrap();
                out.push(Tok::Str(b[end + 1..end + 1 + n].to_vec()));
                i = end + 1 + n;
            }
            b'"' => {
                let mut s = Vec::new();
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    s.push(b[i]);
                    i += 1;
                }
                i += 1;
                out.push(Tok::Str(s));
            }
            b'(' => {
                let start = i;
                let mut depth = 0;
                while i < b.len() {
                    match b[i] {
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
                i += 1;
                out.push(Tok::List(String::from_utf8_lossy(&b[start..i]).into_owned()));
            }
            _ => {
                let start = i;
                // An atom runs to a space, except inside [...] (BODY.PEEK[HEADER.FIELDS (A B)]).
                let mut bracket = 0;
                while i < b.len() && (b[i] != b' ' || bracket > 0) {
                    match b[i] {
                        b'[' => bracket += 1,
                        b']' => bracket -= 1,
                        _ => {}
                    }
                    i += 1;
                }
                out.push(Tok::Atom(String::from_utf8_lossy(&b[start..i]).into_owned()));
            }
        }
    }
    out
}

fn literal(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{{{}}}\r\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out
}

fn split_message(raw: &[u8]) -> (&[u8], &[u8]) {
    match raw.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(i) => (&raw[..i + 4], &raw[i + 4..]),
        None => (raw, b""),
    }
}

/// The header fields named, as a header block.
fn header_fields(header: &[u8], names: &[String]) -> Vec<u8> {
    let text = String::from_utf8_lossy(header);
    let mut out = String::new();
    let mut keep = false;
    for line in text.split("\r\n") {
        if line.is_empty() {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            if keep {
                out.push_str(line);
                out.push_str("\r\n");
            }
            continue;
        }
        let name = line.split(':').next().unwrap_or("");
        keep = names.iter().any(|n| n.eq_ignore_ascii_case(name));
        if keep {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    out.push_str("\r\n");
    out.into_bytes()
}

fn header_value(raw: &[u8], field: &str) -> String {
    let (header, _) = split_message(raw);
    let unfolded = String::from_utf8_lossy(header).replace("\r\n ", " ").replace("\r\n\t", " ");
    unfolded
        .split("\r\n")
        .filter_map(|l| l.split_once(':'))
        .filter(|(n, _)| n.eq_ignore_ascii_case(field))
        .map(|(_, v)| v.trim().to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parses search keys into a test, consuming tokens.
fn search_key(toks: &[Tok], i: &mut usize) -> Box<dyn Fn(&Msg) -> bool> {
    let word = toks[*i].text().to_ascii_uppercase();
    *i += 1;
    match word.as_str() {
        "ALL" => Box::new(|_| true),
        "UNSEEN" => Box::new(|m: &Msg| !m.flags.iter().any(|f| f == "\\Seen")),
        "TEXT" => {
            let needle = toks[*i].text().to_lowercase();
            *i += 1;
            Box::new(move |m: &Msg| String::from_utf8_lossy(&m.raw).to_lowercase().contains(&needle))
        }
        "HEADER" => {
            let field = toks[*i].text();
            let needle = toks[*i + 1].text().to_lowercase();
            *i += 2;
            Box::new(move |m: &Msg| header_value(&m.raw, &field).to_lowercase().contains(&needle))
        }
        "OR" => {
            let a = search_key(toks, i);
            let b = search_key(toks, i);
            Box::new(move |m: &Msg| a(m) || b(m))
        }
        "CHARSET" => {
            *i += 1;
            search_key(toks, i)
        }
        other => panic!("fake iCloud: unknown search key {other}"),
    }
}

fn uids_in(set: &str, b: &Mailbox) -> Vec<u32> {
    let max = b.msgs.iter().map(|m| m.uid).max().unwrap_or(0);
    let mut out = Vec::new();
    for part in set.split(',') {
        let (lo, hi) = match part.split_once(':') {
            Some((a, b)) => (a.parse().unwrap_or(max), if b == "*" { max } else { b.parse().unwrap_or(max) }),
            None => {
                let v = part.parse().unwrap_or(max);
                (v, v)
            }
        };
        out.extend(b.msgs.iter().map(|m| m.uid).filter(|u| *u >= lo.min(hi) && *u <= hi.max(lo)));
    }
    out
}

fn fetch_item(m: &Msg, items: &str) -> Vec<u8> {
    let mut out = format!("UID {} FLAGS ({})", m.uid, m.flags.join(" ")).into_bytes();
    if items.contains("INTERNALDATE") {
        out.extend_from_slice(format!(" INTERNALDATE \"{}\"", m.internal).as_bytes());
    }
    if items.contains("RFC822.SIZE") {
        out.extend_from_slice(format!(" RFC822.SIZE {}", m.raw.len()).as_bytes());
    }
    let (header, text) = split_message(&m.raw);
    if let Some(start) = items.find("HEADER.FIELDS (") {
        let rest = &items[start + "HEADER.FIELDS (".len()..];
        let names: Vec<String> = rest[..rest.find(')').unwrap()].split_whitespace().map(str::to_string).collect();
        out.extend_from_slice(format!(" BODY[HEADER.FIELDS ({})] ", names.join(" ")).as_bytes());
        out.extend(literal(&header_fields(header, &names)));
    }
    if let Some(start) = items.find("BODY.PEEK[TEXT]<") {
        let rest = &items[start + "BODY.PEEK[TEXT]<".len()..];
        let (from, len) = rest[..rest.find('>').unwrap()].split_once('.').unwrap();
        let (from, len): (usize, usize) = (from.parse().unwrap(), len.parse().unwrap());
        let part = &text[from.min(text.len())..(from + len).min(text.len())];
        out.extend_from_slice(format!(" BODY[TEXT]<{from}> ").as_bytes());
        out.extend(literal(part));
    }
    if items.contains("BODY.PEEK[]") {
        out.extend_from_slice(b" BODY[] ");
        out.extend(literal(&m.raw));
    }
    out
}

fn imap_session(tcp: TcpStream, tls: Arc<ServerConfig>, state: Arc<Mutex<State>>) {
    let mode = state.lock().unwrap().mode;
    let conn = ServerConnection::new(tls).unwrap();
    let mut l = Lines { conn: Conn::Tls(Box::new(StreamOwned::new(conn, tcp))), buf: Vec::new() };
    if mode == Mode::Garbage {
        l.send("HTTP/1.1 400 Bad Request\r\n\r\n");
        return;
    }
    l.send("* OK [CAPABILITY IMAP4rev1 AUTH=PLAIN] Fake iCloud IMAP ready\r\n");
    let mut selected: Option<(String, bool)> = None;
    let mut signed_in = false;
    while let Some((toks, logged)) = read_command(&mut l) {
        if toks.len() < 2 {
            break;
        }
        let tag = toks[0].text();
        let mut cmd = toks[1].text().to_ascii_uppercase();
        let mut args = &toks[2..];
        if cmd == "UID" {
            cmd = format!("UID {}", args[0].text().to_ascii_uppercase());
            args = &args[1..];
        }
        let mut st = state.lock().unwrap();
        st.log.push(logged.split_once(' ').map(|(_, rest)| rest.to_string()).unwrap_or_default().replace(PASSWORD, "<password>"));
        let ok = |l: &mut Lines, what: &str| l.send(format!("{tag} OK {what}\r\n"));
        if !signed_in && !matches!(cmd.as_str(), "CAPABILITY" | "LOGIN" | "LOGOUT" | "NOOP") {
            l.send(format!("{tag} BAD sign in first\r\n"));
            continue;
        }
        match cmd.as_str() {
            "CAPABILITY" => {
                l.send("* CAPABILITY IMAP4rev1 MOVE UIDPLUS IDLE\r\n");
                ok(&mut l, "CAPABILITY completed");
            }
            "NOOP" => ok(&mut l, "NOOP completed"),
            "LOGOUT" => {
                l.send("* BYE see you\r\n");
                ok(&mut l, "LOGOUT completed");
                break;
            }
            "LOGIN" => {
                let (user, pass) = (args[0].text(), args[1].text());
                st.logins.push(user.clone());
                // Like iCloud's documented form: the address's name part (the whole address works too).
                if st.mode != Mode::BadPassword && pass == PASSWORD && (user == "me" || user == USER) {
                    signed_in = true;
                    ok(&mut l, "LOGIN completed");
                } else {
                    l.send(format!("{tag} NO [AUTHENTICATIONFAILED] Authentication failed.\r\n"));
                }
            }
            "LIST" => {
                for b in &st.boxes {
                    l.send(format!("* LIST (\\HasNoChildren) \"/\" \"{}\"\r\n", b.name));
                }
                ok(&mut l, "LIST completed");
            }
            "SELECT" | "EXAMINE" => {
                let name = args[0].text();
                match st.mailbox(&name) {
                    Some(b) => {
                        l.send(format!("* {} EXISTS\r\n* 0 RECENT\r\n* FLAGS (\\Seen \\Deleted)\r\n* OK [UIDVALIDITY {}] UIDs valid\r\n* OK [UIDNEXT {}] next\r\n", b.msgs.len(), b.validity, b.next_uid));
                        selected = Some((b.name.clone(), cmd == "EXAMINE"));
                        ok(&mut l, if cmd == "EXAMINE" { "[READ-ONLY] EXAMINE completed" } else { "[READ-WRITE] SELECT completed" });
                    }
                    None => l.send(format!("{tag} NO no such mailbox\r\n")),
                }
            }
            "CREATE" => {
                let name = args[0].text();
                let validity = 200 + st.boxes.len() as u32;
                st.boxes.push(Mailbox { name, validity, next_uid: 1, msgs: Vec::new() });
                ok(&mut l, "CREATE completed");
            }
            "UID SEARCH" => {
                let b = st.mailbox(&selected.as_ref().unwrap().0).unwrap();
                let mut i = 0;
                let mut tests = Vec::new();
                while i < args.len() {
                    tests.push(search_key(args, &mut i));
                }
                let uids: Vec<String> = b.msgs.iter().filter(|m| tests.iter().all(|t| t(m))).map(|m| m.uid.to_string()).collect();
                l.send(format!("* SEARCH{}{}\r\n", if uids.is_empty() { "" } else { " " }, uids.join(" ")));
                ok(&mut l, "SEARCH completed");
            }
            "UID FETCH" => {
                let b = st.mailbox(&selected.as_ref().unwrap().0).unwrap();
                let uids = uids_in(&args[0].text(), b);
                let items = args[1..].iter().map(Tok::text).collect::<Vec<_>>().join(" ");
                for (seq, m) in b.msgs.iter().enumerate() {
                    if uids.contains(&m.uid) {
                        let mut line = format!("* {} FETCH (", seq + 1).into_bytes();
                        line.extend(fetch_item(m, &items));
                        line.extend_from_slice(b")\r\n");
                        l.send(line);
                    }
                }
                ok(&mut l, "FETCH completed");
            }
            "UID STORE" => {
                let (name, read_only) = selected.clone().unwrap();
                if read_only {
                    l.send(format!("{tag} NO mailbox is read-only\r\n"));
                    continue;
                }
                let b = st.mailbox_mut(&name).unwrap();
                let uids = uids_in(&args[0].text(), b);
                let change = args[1].text();
                let flags: Vec<String> = args[2].text().trim_matches(['(', ')']).split_whitespace().map(str::to_string).collect();
                for m in b.msgs.iter_mut().filter(|m| uids.contains(&m.uid)) {
                    for f in &flags {
                        if change.starts_with('+') && !m.flags.contains(f) {
                            m.flags.push(f.clone());
                        } else if change.starts_with('-') {
                            m.flags.retain(|x| x != f);
                        }
                    }
                }
                ok(&mut l, "STORE completed");
            }
            "UID MOVE" => {
                let (name, read_only) = selected.clone().unwrap();
                if read_only {
                    l.send(format!("{tag} NO mailbox is read-only\r\n"));
                    continue;
                }
                let dest = args[1].text();
                if st.mailbox(&dest).is_none() {
                    l.send(format!("{tag} NO [TRYCREATE] no such mailbox\r\n"));
                    continue;
                }
                let from = st.mailbox_mut(&name).unwrap();
                let uids = uids_in(&args[0].text(), from);
                let moving: Vec<Msg> = from.msgs.iter().filter(|m| uids.contains(&m.uid)).cloned().collect();
                from.msgs.retain(|m| !uids.contains(&m.uid));
                let to = st.mailbox_mut(&dest).unwrap();
                for mut m in moving {
                    m.uid = to.next_uid;
                    to.next_uid += 1;
                    to.msgs.push(m);
                }
                ok(&mut l, "MOVE completed");
            }
            "APPEND" => {
                let name = args[0].text();
                let flags: Vec<&str> = match &args[1] {
                    Tok::List(f) => f.trim_matches(['(', ')']).split_whitespace().collect(),
                    _ => Vec::new(),
                };
                let raw = match &args[args.len() - 1] {
                    Tok::Str(b) => b.clone(),
                    other => other.text().into_bytes(),
                };
                let flags: Vec<String> = flags.iter().map(|f| f.to_string()).collect();
                match st.mailbox_mut(&name) {
                    Some(b) => {
                        let uid = b.next_uid;
                        b.next_uid += 1;
                        b.msgs.push(Msg { uid, flags, internal: "09-Oct-2026 12:00:00 +0000".into(), raw });
                        ok(&mut l, "APPEND completed");
                    }
                    None => l.send(format!("{tag} NO [TRYCREATE] no such mailbox\r\n")),
                }
            }
            other => l.send(format!("{tag} BAD unknown command {other}\r\n")),
        }
    }
}

// ---------- SMTP ----------

fn smtp_session(tcp: TcpStream, tls: Arc<ServerConfig>, state: Arc<Mutex<State>>) {
    let mut l = Lines { conn: Conn::Plain(tcp), buf: Vec::new() };
    l.send("220 fake.mail.me.com ESMTP\r\n");
    let mut user = String::new();
    let (mut from, mut to) = (String::new(), Vec::new());
    while let Some(line) = l.line() {
        let line = String::from_utf8_lossy(&line).into_owned();
        let upper = line.to_ascii_uppercase();
        let secure = matches!(l.conn, Conn::Tls(_));
        if upper.starts_with("EHLO") {
            l.send(if secure { "250-fake.mail.me.com\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n" } else { "250-fake.mail.me.com\r\n250 STARTTLS\r\n" });
        } else if upper == "STARTTLS" {
            l.send("220 go ahead\r\n");
            let Conn::Plain(tcp) = std::mem::replace(&mut l.conn, Conn::Gone) else { return };
            l.conn = Conn::Tls(Box::new(StreamOwned::new(ServerConnection::new(tls.clone()).unwrap(), tcp)));
        } else if upper.starts_with("AUTH PLAIN") {
            if !secure {
                l.send("530 STARTTLS first\r\n");
                continue;
            }
            use base64::Engine;
            let token = line.split_whitespace().nth(2).map(str::to_string).unwrap_or_else(|| {
                l.send("334 \r\n");
                String::from_utf8_lossy(&l.line().unwrap_or_default()).into_owned()
            });
            let decoded = base64::engine::general_purpose::STANDARD.decode(token.trim()).unwrap_or_default();
            let parts: Vec<String> = decoded.split(|b| *b == 0).map(|p| String::from_utf8_lossy(p).into_owned()).collect();
            let st = state.lock().unwrap();
            if st.mode != Mode::BadPassword && parts.len() == 3 && parts[1] == USER && parts[2] == PASSWORD {
                user = parts[1].clone();
                l.send("235 2.7.0 Authentication successful\r\n");
            } else {
                l.send("535 5.7.8 Authentication credentials invalid\r\n");
            }
        } else if upper.starts_with("MAIL FROM:") {
            if user.is_empty() {
                l.send("530 5.7.0 Authentication required\r\n");
                continue;
            }
            from = line[10..].trim().trim_matches(['<', '>']).split_whitespace().next().unwrap_or("").trim_matches(['<', '>']).to_string();
            to.clear();
            l.send("250 OK\r\n");
        } else if upper.starts_with("RCPT TO:") {
            to.push(line[8..].trim().trim_matches(['<', '>']).to_string());
            l.send("250 OK\r\n");
        } else if upper == "DATA" {
            l.send("354 go ahead\r\n");
            let mut data = String::new();
            while let Some(d) = l.line() {
                let d = String::from_utf8_lossy(&d).into_owned();
                if d == "." {
                    break;
                }
                data.push_str(d.strip_prefix('.').unwrap_or(&d));
                data.push_str("\r\n");
            }
            let mut st = state.lock().unwrap();
            if st.mode == Mode::AutoSent {
                st.add("Sent Messages", &["\\Seen"], "09-Oct-2026 12:00:00 +0000", &data.replace("\r\n", "\n"));
            }
            st.sent.push(Sent { from: from.clone(), to: to.clone(), data, user: user.clone() });
            l.send("250 2.0.0 OK queued\r\n");
        } else if upper == "QUIT" {
            l.send("221 bye\r\n");
            break;
        } else if upper == "RSET" || upper == "NOOP" {
            l.send("250 OK\r\n");
        } else {
            l.send("502 unknown command\r\n");
        }
    }
}
