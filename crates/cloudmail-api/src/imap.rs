//! A small blocking IMAP4rev1 client over TLS: the commands the iCloud provider uses (LOGIN, LIST,
//! EXAMINE/SELECT, UID SEARCH/FETCH/STORE/MOVE, APPEND, CREATE), nothing else.
//!
//! Responses are parsed by `imap-proto` (the parser behind the `imap` and `async-imap` crates);
//! this module only frames them: it reads until a whole response (literals included) parses,
//! sends string arguments quoted or as synchronizing literals, and matches tagged completions.
//! Everything blocks with socket timeouts, like the rest of cloudmail's providers.

use imap_proto::{AttributeValue, MailboxDatum, MessageSection, NameAttribute, Response, ResponseCode, SectionPath, Status};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, StreamOwned};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

/// The most bytes one response may take (a whole message with its attachments).
const MAX_RESPONSE: usize = 96 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum Fail {
    /// The connection failed, timed out or closed.
    Io(String),
    /// LOGIN was refused.
    Auth(String),
    /// The server answered a command with NO or BAD.
    No(String),
    /// The server said something this client can't read.
    Protocol(String),
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fail::Io(m) | Fail::Auth(m) | Fail::No(m) | Fail::Protocol(m) => f.write_str(m),
        }
    }
}

pub type Result<T> = std::result::Result<T, Fail>;

/// One argument of a command: sent as it is, or as an IMAP string (quoted, or a literal when it
/// can't be quoted).
#[derive(Clone)]
pub enum Arg {
    Raw(String),
    Str(Vec<u8>),
}

pub fn raw(s: impl Into<String>) -> Arg {
    Arg::Raw(s.into())
}

pub fn string(s: impl AsRef<[u8]>) -> Arg {
    Arg::Str(s.as_ref().to_vec())
}

/// A mailbox from LIST, with its special-use attributes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mailbox {
    pub name: String,
    pub sent: bool,
    pub archive: bool,
    pub selectable: bool,
}

/// What a UID FETCH returned for one message.
#[derive(Debug, Clone, Default)]
pub struct Fetched {
    pub uid: u32,
    pub flags: Vec<String>,
    pub internal_date: Option<String>,
    pub size: u32,
    pub header: Option<Vec<u8>>,
    pub text: Option<Vec<u8>>,
    pub full: Option<Vec<u8>>,
}

impl Fetched {
    pub fn seen(&self) -> bool {
        self.flags.iter().any(|f| f.eq_ignore_ascii_case("\\Seen"))
    }
}

pub struct Session {
    stream: StreamOwned<ClientConnection, TcpStream>,
    buf: Vec<u8>,
    tag: u32,
    caps: Vec<String>,
    /// The selected mailbox, whether it is read-only, and its UIDVALIDITY.
    selected: Option<(String, bool, u32)>,
}

/// `1:3,7` for a list of UIDs.
pub fn uid_set(uids: &[u32]) -> String {
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        let mut end = start;
        while i + 1 < sorted.len() && sorted[i + 1] == end + 1 {
            i += 1;
            end = sorted[i];
        }
        out.push(if start == end { start.to_string() } else { format!("{start}:{end}") });
        i += 1;
    }
    out.join(",")
}

/// Whether bytes can go as a quoted string rather than a literal.
fn quotable(s: &[u8]) -> bool {
    s.len() < 1000 && s.iter().all(|&b| b.is_ascii() && b != b'\r' && b != b'\n' && b != 0)
}

fn quoted(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() + 2);
    out.push(b'"');
    for &b in s {
        if b == b'"' || b == b'\\' {
            out.push(b'\\');
        }
        out.push(b);
    }
    out.push(b'"');
    out
}

fn io(e: std::io::Error) -> Fail {
    match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => Fail::Io("the mail server stopped answering (timed out)".into()),
        _ => Fail::Io(e.to_string()),
    }
}

fn text_of(outcome: &imap_proto::Outcome<'_>) -> String {
    outcome.information.as_deref().unwrap_or("").trim().to_string()
}

impl Session {
    /// Connects with TLS (implicit, as on port 993) and reads the greeting and capabilities.
    pub fn connect(host: &str, port: u16, tls: Arc<ClientConfig>, timeout: Duration) -> Result<Self> {
        let addrs: Vec<_> = (host, port).to_socket_addrs().map_err(|e| Fail::Io(format!("can't look up {host}: {e}")))?.collect();
        let mut last = Fail::Io(format!("{host} has no address"));
        let mut tcp = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, timeout) {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last = Fail::Io(format!("can't connect to {host}:{port}: {e}")),
            }
        }
        let tcp = tcp.ok_or(last)?;
        tcp.set_read_timeout(Some(timeout)).map_err(io)?;
        tcp.set_write_timeout(Some(timeout)).map_err(io)?;
        let name = ServerName::try_from(host.to_string()).map_err(|e| Fail::Io(format!("{host}: {e}")))?;
        let conn = ClientConnection::new(tls, name).map_err(|e| Fail::Io(format!("TLS: {e}")))?;
        let mut session = Self { stream: StreamOwned::new(conn, tcp), buf: Vec::new(), tag: 0, caps: Vec::new(), selected: None };
        match session.read_response(true)? {
            Response::Data { status: Status::Ok, outcome } => {
                if let Some(ResponseCode::Capabilities(caps)) = outcome.code {
                    session.caps = caps.iter().map(cap_name).collect();
                }
            }
            Response::Data { status, outcome } => return Err(Fail::Io(format!("the mail server isn't taking connections ({status:?}: {})", text_of(&outcome)))),
            _ => return Err(Fail::Protocol("the mail server's greeting isn't IMAP".into())),
        }
        if session.caps.is_empty() {
            session.capabilities()?;
        }
        Ok(session)
    }

    pub fn has_capability(&self, cap: &str) -> bool {
        self.caps.iter().any(|c| c.eq_ignore_ascii_case(cap))
    }

    fn capabilities(&mut self) -> Result<()> {
        let mut caps = Vec::new();
        for r in self.run(&[raw("CAPABILITY")])? {
            if let Response::Capabilities(list) = r {
                caps.extend(list.iter().map(cap_name));
            }
        }
        self.caps = caps;
        Ok(())
    }

    /// Reads one whole response. Untagged lines this client can't parse (an extension's data) are
    /// skipped unless `strict`.
    fn read_response(&mut self, strict: bool) -> Result<Response<'static>> {
        loop {
            if !self.buf.is_empty() {
                match Response::parse(&self.buf) {
                    Ok((rest, response)) => {
                        let used = self.buf.len() - rest.len();
                        let owned = response.into_owned();
                        self.buf.drain(..used);
                        return Ok(owned);
                    }
                    Err(nom::Err::Incomplete(_)) => {}
                    Err(_) => {
                        let line_end = self.buf.windows(2).position(|w| w == b"\r\n");
                        match line_end {
                            Some(end) if !strict && self.buf.starts_with(b"* ") && !self.buf[..end].ends_with(b"}") => {
                                self.buf.drain(..end + 2);
                                continue;
                            }
                            Some(end) => {
                                let line = String::from_utf8_lossy(&self.buf[..end.min(200)]).into_owned();
                                return Err(Fail::Protocol(format!("the mail server said something unexpected: {line}")));
                            }
                            None => {}
                        }
                    }
                }
            }
            if self.buf.len() > MAX_RESPONSE {
                return Err(Fail::Protocol("the mail server's answer is too large".into()));
            }
            let mut chunk = [0u8; 16 * 1024];
            let n = self.stream.read(&mut chunk).map_err(io)?;
            if n == 0 {
                return Err(Fail::Io("the mail server closed the connection".into()));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Sends a command and collects its untagged responses until its tagged completion.
    pub fn run(&mut self, args: &[Arg]) -> Result<Vec<Response<'static>>> {
        self.tag += 1;
        let tag = format!("c{}", self.tag);
        let mut line = tag.clone().into_bytes();
        for arg in args {
            line.push(b' ');
            match arg {
                Arg::Raw(s) => line.extend_from_slice(s.as_bytes()),
                Arg::Str(s) if quotable(s) => line.extend_from_slice(&quoted(s)),
                Arg::Str(s) => {
                    // A synchronizing literal: send up to `{n}`, wait for the go-ahead, then the bytes.
                    line.extend_from_slice(format!("{{{}}}\r\n", s.len()).as_bytes());
                    self.stream.write_all(&line).map_err(io)?;
                    self.stream.flush().map_err(io)?;
                    line.clear();
                    loop {
                        match self.read_response(false)? {
                            Response::Continue(_) => break,
                            Response::Done { status, outcome, .. } => {
                                return Err(Fail::No(format!("the mail server refused the command ({status:?}: {})", text_of(&outcome))));
                            }
                            _ => {}
                        }
                    }
                    line.extend_from_slice(s);
                }
            }
        }
        line.extend_from_slice(b"\r\n");
        self.stream.write_all(&line).map_err(io)?;
        self.stream.flush().map_err(io)?;
        let mut out = Vec::new();
        loop {
            match self.read_response(false)? {
                Response::Done { tag: t, status, outcome } if t.0 == tag => {
                    return match status {
                        Status::Ok => Ok(out),
                        _ => Err(Fail::No(text_of(&outcome))),
                    };
                }
                Response::Data { status: Status::Bye, outcome } => {
                    return Err(Fail::Io(format!("the mail server ended the session: {}", text_of(&outcome))));
                }
                other => out.push(other),
            }
        }
    }

    pub fn login(&mut self, user: &str, password: &str) -> Result<()> {
        match self.run(&[raw("LOGIN"), string(user), string(password)]) {
            Ok(_) => {
                // Servers often announce more capabilities once signed in.
                self.capabilities()?;
                Ok(())
            }
            Err(Fail::No(m)) => Err(Fail::Auth(m)),
            Err(e) => Err(e),
        }
    }

    pub fn list(&mut self) -> Result<Vec<Mailbox>> {
        let mut out = Vec::new();
        for r in self.run(&[raw("LIST"), string(""), string("*")])? {
            if let Response::MailboxData(MailboxDatum::List(l)) = r {
                let has = |a: &NameAttribute<'_>| l.name_attributes.contains(a);
                out.push(Mailbox {
                    name: l.name.to_string(),
                    sent: has(&NameAttribute::Sent),
                    archive: has(&NameAttribute::Archive),
                    selectable: !has(&NameAttribute::NoSelect),
                });
            }
        }
        Ok(out)
    }

    /// Opens a mailbox (EXAMINE when `read_only`, so reading never marks mail seen) and returns its
    /// UIDVALIDITY. A mailbox already open the same way isn't opened again.
    pub fn open(&mut self, mailbox: &str, read_only: bool) -> Result<u32> {
        if let Some((name, ro, validity)) = &self.selected
            && name == mailbox
            && (*ro == read_only || !*ro)
        {
            return Ok(*validity);
        }
        self.selected = None;
        let responses = self.run(&[raw(if read_only { "EXAMINE" } else { "SELECT" }), string(mailbox)])?;
        let validity = responses
            .iter()
            .find_map(|r| match r {
                Response::Data { outcome, .. } => match outcome.code {
                    Some(ResponseCode::UidValidity(v)) => Some(v),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or(0);
        self.selected = Some((mailbox.to_string(), read_only, validity));
        Ok(validity)
    }

    pub fn uid_search(&mut self, criteria: Vec<Arg>) -> Result<Vec<u32>> {
        let mut args = vec![raw("UID SEARCH")];
        args.extend(criteria);
        let mut uids = Vec::new();
        for r in self.run(&args)? {
            if let Response::MailboxData(MailboxDatum::Search(list)) = r {
                uids.extend(list);
            }
        }
        uids.sort_unstable();
        Ok(uids)
    }

    /// `UID FETCH <uids> <items>`, by UID.
    pub fn uid_fetch(&mut self, uids: &[u32], items: &str) -> Result<Vec<Fetched>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<Fetched> = Vec::new();
        for r in self.run(&[raw("UID FETCH"), raw(uid_set(uids)), raw(items)])? {
            let Response::Fetch(_, attrs) = r else { continue };
            let mut f = Fetched::default();
            for a in attrs {
                match a {
                    AttributeValue::Uid(u) => f.uid = u,
                    AttributeValue::Flags(flags) => f.flags = flags.iter().map(|s| s.to_string()).collect(),
                    AttributeValue::InternalDate(d) => f.internal_date = Some(d.to_string()),
                    AttributeValue::Rfc822Size(s) => f.size = s,
                    AttributeValue::Rfc822(Some(d)) => f.full = Some(d.into_owned()),
                    AttributeValue::Rfc822Header(Some(d)) => f.header = Some(d.into_owned()),
                    AttributeValue::BodySection { section, data, .. } => {
                        let data = data.map(|d| d.into_owned()).unwrap_or_default();
                        match section {
                            None => f.full = Some(data),
                            Some(SectionPath::Full(MessageSection::Header)) => f.header = Some(data),
                            Some(SectionPath::Full(MessageSection::Text)) => f.text = Some(data),
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            if f.uid != 0 {
                // A server may answer one message in several FETCH responses.
                match out.iter_mut().find(|o| o.uid == f.uid) {
                    Some(o) => merge(o, f),
                    None => out.push(f),
                }
            }
        }
        Ok(out)
    }

    /// `UID STORE <uids> <change> (<flags>)`, e.g. `+FLAGS.SILENT (\Seen)`.
    pub fn uid_store(&mut self, uids: &[u32], change: &str, flags: &str) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        self.run(&[raw("UID STORE"), raw(uid_set(uids)), raw(change), raw(format!("({flags})"))]).map(drop)
    }

    /// Moves messages out of the selected mailbox: MOVE when the server has it, else COPY, then
    /// \Deleted and UID EXPUNGE of just those messages (never a plain EXPUNGE, which would remove
    /// anything else marked deleted).
    pub fn uid_move(&mut self, uids: &[u32], to: &str) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let set = uid_set(uids);
        if self.has_capability("MOVE") {
            return self.run(&[raw("UID MOVE"), raw(set), string(to)]).map(drop);
        }
        if !self.has_capability("UIDPLUS") {
            return Err(Fail::No("the mail server can neither MOVE nor expunge single messages".into()));
        }
        self.run(&[raw("UID COPY"), raw(set.clone()), string(to)])?;
        self.run(&[raw("UID STORE"), raw(set.clone()), raw("+FLAGS.SILENT (\\Deleted)")])?;
        self.run(&[raw("UID EXPUNGE"), raw(set)]).map(drop)
    }

    pub fn append(&mut self, mailbox: &str, flags: &str, message: &[u8]) -> Result<()> {
        self.run(&[raw("APPEND"), string(mailbox), raw(format!("({flags})")), Arg::Str(message.to_vec())]).map(drop)
    }

    pub fn create(&mut self, mailbox: &str) -> Result<()> {
        self.run(&[raw("CREATE"), string(mailbox)]).map(drop)
    }

    pub fn noop(&mut self) -> Result<()> {
        self.run(&[raw("NOOP")]).map(drop)
    }

    pub fn logout(mut self) {
        let _ = self.run(&[raw("LOGOUT")]);
    }
}

fn cap_name(c: &imap_proto::Capability<'_>) -> String {
    match c {
        imap_proto::Capability::Imap4rev1 => "IMAP4rev1".into(),
        imap_proto::Capability::Auth(a) => format!("AUTH={a}"),
        imap_proto::Capability::Atom(a) => a.to_string(),
    }
}

fn merge(into: &mut Fetched, f: Fetched) {
    if !f.flags.is_empty() {
        into.flags = f.flags;
    }
    into.internal_date = into.internal_date.take().or(f.internal_date);
    into.size = into.size.max(f.size);
    into.header = into.header.take().or(f.header);
    into.text = into.text.take().or(f.text);
    into.full = into.full.take().or(f.full);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_sets_compress_runs() {
        assert_eq!(uid_set(&[7, 1, 2, 3, 9, 10, 3]), "1:3,7,9:10");
        assert_eq!(uid_set(&[5]), "5");
    }

    #[test]
    fn strings_quote_or_need_a_literal() {
        assert_eq!(quoted(br#"a "b" \c"#), br#""a \"b\" \\c""#.to_vec());
        assert!(quotable(b"you@icloud.com"));
        assert!(!quotable("caf\u{e9}".as_bytes()));
        assert!(!quotable(b"two\r\nlines"));
    }

    #[test]
    fn fetch_responses_parse_with_literals() {
        let wire = b"* 3 FETCH (UID 41 FLAGS (\\Seen) INTERNALDATE \"02-Oct-2026 09:15:00 +0000\" RFC822.SIZE 120 BODY[HEADER.FIELDS (SUBJECT)] {15}\r\nSubject: Hi\r\n\r\n BODY[TEXT]<0> {5}\r\nhello)\r\n";
        let (rest, r) = Response::parse(wire).unwrap();
        assert!(rest.is_empty());
        let Response::Fetch(3, attrs) = r else { panic!("{r:?}") };
        assert!(attrs.iter().any(|a| matches!(a, AttributeValue::BodySection { section: Some(SectionPath::Full(MessageSection::Header)), .. })));
        assert!(attrs.iter().any(|a| matches!(a, AttributeValue::BodySection { section: Some(SectionPath::Full(MessageSection::Text)), .. })));
        assert!(matches!(Response::parse(&wire[..wire.len() - 4]), Err(nom::Err::Incomplete(_))), "half a response asks for more");
    }

    #[test]
    fn a_refused_login_parses() {
        let (_, r) = Response::parse(b"c1 NO [AUTHENTICATIONFAILED] Authentication failed.\r\n").unwrap();
        assert!(matches!(r, Response::Done { status: Status::No, .. }), "{r:?}");
    }
}
