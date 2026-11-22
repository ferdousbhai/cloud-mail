//! Response envelope, output modes and exit codes.

pub use cloudmail_api::text::shell_arg;
use serde::Serialize;
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};

use cloudmail_api::{Error as ApiError, ErrorKind};

/// Process exit codes: Omarchy's contract for a command line, 0 when the command did what it was asked and 1 for any
/// failure, with the error on stderr. What kind of failure it was is in the error itself ([`Kind`]), where a caller
/// can read it without knowing a table of numbers. Documented in `cloudmail agent-guide` and `cloudmail commands`.
pub mod exit {
    pub const OK: i32 = 0;
    pub const FAILURE: i32 = 1;

    pub const TABLE: &[(i32, &str)] = &[
        (OK, "success"),
        (
            FAILURE,
            "any failure: the error is on stderr, as JSON with its `code` and `kind` when not on a terminal",
        ),
    ];
}

/// The broad class of a failure, given as `error.kind` beside the finer `error.code`. These are the classes the exit
/// codes 2 to 5 used to tell apart, kept as stable strings now that every failure exits 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Usage,
    Auth,
    NotFound,
    Unavailable,
    Other,
}

impl Kind {
    pub const ALL: [Kind; 5] = [Kind::Usage, Kind::Auth, Kind::NotFound, Kind::Unavailable, Kind::Other];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Usage => "usage",
            Kind::Auth => "auth",
            Kind::NotFound => "not_found",
            Kind::Unavailable => "unavailable",
            Kind::Other => "other",
        }
    }

    pub fn meaning(self) -> &'static str {
        match self {
            Kind::Usage => {
                "invalid arguments, a destructive command run without --yes, or a request the worker rejected"
            }
            Kind::Auth => {
                "not configured, the API token was rejected, the Cloudflare CLI (cf) isn't logged in, or a linked account (HEY, Gmail) isn't signed in"
            }
            Kind::NotFound => "the thread, message, attachment, mailbox or sender does not exist",
            Kind::Unavailable => {
                "the worker could not be reached or returned an error, or a linked account's CLI is missing or failed"
            }
            Kind::Other => "any other failure (local I/O, a setup step, a cancelled confirmation or editor)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Human-readable text (default on a terminal).
    Human,
    /// The full JSON envelope (default when stdout is not a terminal).
    Json,
    /// Only the `data` field as JSON.
    Quiet,
    /// One ID per line.
    Ids,
    /// Only the number of results.
    Count,
}

impl Mode {
    pub fn is_machine(self) -> bool {
        self != Mode::Human
    }
}

pub fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

pub fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}

/// Reads a line from the terminal after a prompt.
pub fn prompt(question: &str) -> CliResult<String> {
    eprint!("{question} ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().to_string())
}

/// Asks a yes/no question at the terminal; anything but y/yes is no.
pub fn ask_yes(question: &str) -> CliResult<bool> {
    let answer = prompt(&format!("{question} [y/N]"))?;
    Ok(matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"))
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Breadcrumb {
    pub action: String,
    pub command: String,
    pub description: String,
}

pub fn crumb(action: &str, command: &str, description: &str) -> Breadcrumb {
    Breadcrumb {
        action: action.into(),
        command: command.into(),
        description: description.into(),
    }
}

/// What a command produced; rendered according to the output mode.
#[derive(Debug, Default)]
pub struct Response {
    pub data: Value,
    pub summary: String,
    pub breadcrumbs: Vec<Breadcrumb>,
    pub meta: serde_json::Map<String, Value>,
    /// Text for terminals; falls back to `summary` when empty.
    pub human: String,
    /// IDs for `--ids-only`; `--count` uses their number when set, else the length of `data`.
    pub ids: Option<Vec<String>>,
    /// The command already wrote its output (raw bytes, a stream); print nothing more.
    pub silent: bool,
}

impl Response {
    pub fn new(data: impl Serialize, summary: impl Into<String>) -> Self {
        Self {
            data: serde_json::to_value(data).unwrap_or(Value::Null),
            summary: summary.into(),
            ..Default::default()
        }
    }

    pub fn silent() -> Self {
        Self {
            silent: true,
            ..Default::default()
        }
    }

    pub fn human(mut self, text: impl Into<String>) -> Self {
        self.human = text.into();
        self
    }

    pub fn crumbs(mut self, crumbs: Vec<Breadcrumb>) -> Self {
        self.breadcrumbs = crumbs;
        self
    }

    pub fn ids(mut self, ids: Vec<String>) -> Self {
        self.ids = Some(ids);
        self
    }

    pub fn meta(mut self, key: &str, value: impl Serialize) -> Self {
        self.meta
            .insert(key.into(), serde_json::to_value(value).unwrap_or(Value::Null));
        self
    }

    pub fn envelope(&self) -> Value {
        let mut v = json!({
            "ok": true,
            "data": self.data,
            "summary": self.summary,
            "breadcrumbs": self.breadcrumbs,
        });
        if !self.meta.is_empty() {
            v["meta"] = Value::Object(self.meta.clone());
        }
        v
    }

    fn count(&self) -> usize {
        match (&self.ids, &self.data) {
            (Some(ids), _) => ids.len(),
            (None, Value::Array(a)) => a.len(),
            (None, Value::Null) => 0,
            _ => 1,
        }
    }

    pub fn print(&self, mode: Mode) {
        if self.silent {
            return;
        }
        let text = match mode {
            Mode::Json => pretty(&self.envelope()),
            Mode::Quiet => pretty(&self.data),
            Mode::Count => self.count().to_string(),
            Mode::Ids => match &self.ids {
                // IDs can be addresses a sender chose, so they get the same terminal treatment.
                Some(ids) => ids
                    .iter()
                    .map(|id| terminal_safe(id).replace(['\n', '\t'], " "))
                    .collect::<Vec<_>>()
                    .join("\n"),
                // The command has already run (it may have changed something), so say so rather than fail.
                None => {
                    eprintln!("note: this command has no IDs to list; printing its result instead");
                    pretty(&self.data)
                }
            },
            Mode::Human => {
                let mut out = terminal_safe(if self.human.trim().is_empty() {
                    &self.summary
                } else {
                    &self.human
                });
                if !self.breadcrumbs.is_empty() && stdout_is_tty() {
                    out.push_str("\n\n");
                    out.push_str(&dim("Next:"));
                    for b in &self.breadcrumbs {
                        out.push_str(&format!(
                            "\n  {}  {}",
                            terminal_safe(&b.command),
                            dim(&terminal_safe(&b.description))
                        ));
                    }
                }
                out
            }
        };
        write_line(&text);
    }
}

/// Text for a terminal. Anything from the server (subjects, names, filenames, error messages)
/// can contain escape sequences that move the cursor, rewrite earlier lines such as the sender
/// warning, set the clipboard or the title. Only this program's own styling (dim, bold, reset)
/// survives; every other control character except newline and tab becomes `?`.
pub fn terminal_safe(s: &str) -> String {
    const STYLE: [&str; 3] = ["\x1b[2m", "\x1b[1m", "\x1b[0m"];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if let Some(code) = STYLE.iter().find(|code| rest.starts_with(**code)) {
            out.push_str(code);
            rest = &rest[code.len()..];
            continue;
        }
        out.push(if c.is_control() && c != '\n' && c != '\t' {
            '?'
        } else {
            c
        });
        rest = &rest[c.len_utf8()..];
    }
    out
}

pub fn pretty(v: &Value) -> String {
    json_safe(&serde_json::to_string_pretty(v).unwrap_or_default())
}

/// JSON with DEL and the C1 controls escaped as `\u00XX`. serde_json leaves U+007F–U+009F raw,
/// and terminals act on C1 codes (U+009B is CSI), so a sender's subject could drive a terminal
/// that shows `--json` output. These characters only occur inside strings, so the JSON is the same.
pub fn json_safe(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if ('\u{7f}'..='\u{9f}').contains(&c) {
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

fn write_line(text: &str) {
    if !text.is_empty() {
        // Ignore EPIPE (e.g. `cloudmail inbox | head`).
        let _ = writeln!(std::io::stdout(), "{text}");
    }
}

fn style(code: &str, s: &str) -> String {
    if stdout_is_tty() && std::env::var_os("NO_COLOR").is_none() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn dim(s: &str) -> String {
    style("2", s)
}

pub fn bold(s: &str) -> String {
    style("1", s)
}

#[derive(Debug, Clone)]
pub struct CliError {
    pub code: String,
    pub message: String,
    pub hint: Option<String>,
    pub kind: Kind,
}

impl CliError {
    pub fn new(code: &str, kind: Kind, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            hint: None,
            kind,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new("usage", Kind::Usage, message)
    }

    pub fn generic(message: impl Into<String>) -> Self {
        Self::new("error", Kind::Other, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", Kind::NotFound, message)
    }

    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::new("cancelled", Kind::Other, message)
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn json(&self) -> Value {
        let mut err = json!({ "code": self.code, "kind": self.kind.as_str(), "message": self.message });
        if let Some(h) = &self.hint {
            err["hint"] = json!(h);
        }
        json!({ "ok": false, "error": err })
    }

    /// Writes the error to stderr, as JSON for a program and as text for a person: stdout is only ever a command's
    /// result, so a caller reading it never mistakes a failure for data.
    pub fn print(&self, mode: Mode) {
        if mode.is_machine() {
            eprintln!("{}", pretty(&self.json()));
        } else {
            let mut msg = format!("error: {}", self.message);
            if let Some(h) = &self.hint {
                msg.push_str(&format!("\nhint: {h}"));
            }
            eprintln!("{}", terminal_safe(&msg));
        }
    }
}

impl From<ApiError> for CliError {
    fn from(e: ApiError) -> Self {
        let (kind, hint) = match e.kind {
            ErrorKind::Config => (
                Kind::Auth,
                Some(
                    "invited? `cloudmail connect <invite link>`; your own deployment: `cloudmail setup`; only Gmail, iCloud Mail or HEY: `cloudmail account add`",
                ),
            ),
            ErrorKind::Unauthorized => (
                Kind::Auth,
                Some(
                    "the worker didn't accept this token for that: check `cloudmail config show`; if you were invited, ask for a new invite",
                ),
            ),
            ErrorKind::NotFound => (Kind::NotFound, None),
            ErrorKind::BadRequest => (Kind::Usage, None),
            ErrorKind::Network => (
                Kind::Unavailable,
                Some("check api_url with `cloudmail config show` and your connection"),
            ),
            ErrorKind::Api | ErrorKind::Decode => (Kind::Unavailable, None),
            // The message already says which account and what to run.
            ErrorKind::AccountAuth => (Kind::Auth, None),
            ErrorKind::AccountUnavailable => (Kind::Unavailable, Some("see `cloudmail account list`")),
        };
        let mut err = CliError::new(e.kind.code(), kind, e.message);
        err.hint = hint.map(str::to_string);
        err
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::generic(e.to_string())
    }
}

pub type CliResult<T = Response> = Result<T, CliError>;

impl Response {
    /// Records linked-account failures the command carried on past: `meta.warnings` in JSON, a
    /// line each on stderr for people.
    pub fn warnings(self, warnings: &[cloudmail_api::AccountWarning], mode: Mode) -> Self {
        if warnings.is_empty() {
            return self;
        }
        if mode == Mode::Human {
            for w in warnings {
                // The message already names the account ("HEY: …", "Gmail: …").
                eprintln!(
                    "{}",
                    terminal_safe(&format!("warning: {} (showing the rest)", w.message))
                );
            }
        }
        self.meta("warnings", warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_output_escapes_c1_controls() {
        let v = json!({ "subject": "a\u{9b}2Jb\u{7f}" });
        let out = pretty(&v);
        assert!(!out.contains('\u{9b}') && !out.contains('\u{7f}'), "{out:?}");
        let back: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(back, v, "still the same JSON");
    }

    #[test]
    fn terminal_safe_keeps_our_styling_only() {
        let styled = format!("{} and {}", dim("d"), bold("b"));
        let evil = "x\x1b]0;PWNED\x07\x1b[2A\x1b[2K\u{9b}31m\nnext";
        let out = terminal_safe(&format!("{styled} {evil}"));
        assert!(out.starts_with(&styled), "{out:?}");
        assert!(
            !out.contains("\x1b]") && !out.contains("\x1b[2A") && !out.contains('\x07') && !out.contains('\u{9b}'),
            "{out:?}"
        );
        assert!(out.ends_with("\nnext"));
    }

    #[test]
    fn envelope_shape() {
        let r = Response::new(json!([1, 2]), "2 things")
            .crumbs(vec![crumb("read", "cloudmail thread read <id>", "Read a thread")])
            .meta("folder", "inbox");
        let v = r.envelope();
        assert_eq!(v["ok"], true);
        assert_eq!(v["summary"], "2 things");
        assert_eq!(v["breadcrumbs"][0]["command"], "cloudmail thread read <id>");
        assert_eq!(v["meta"]["folder"], "inbox");
        assert_eq!(r.count(), 2);
    }

    #[test]
    fn envelope_omits_empty_meta() {
        assert!(Response::new(json!({}), "x").envelope().get("meta").is_none());
    }

    #[test]
    fn api_errors_map_to_kinds() {
        let e: CliError = ApiError::new(ErrorKind::Unauthorized, "unauthorized").into();
        assert_eq!((e.kind, e.code.as_str()), (Kind::Auth, "unauthorized"));
        let e: CliError = ApiError::new(ErrorKind::NotFound, "not found").into();
        assert_eq!(e.kind, Kind::NotFound);
        let e: CliError = ApiError::new(ErrorKind::BadRequest, "too large").into();
        assert_eq!(e.kind, Kind::Usage);
        let e: CliError = ApiError::new(ErrorKind::Network, "refused").into();
        assert_eq!(e.kind, Kind::Unavailable);
        let e2: CliError = ApiError::new(ErrorKind::AccountAuth, "HEY: not signed in").into();
        assert_eq!((e2.kind, e2.code.as_str()), (Kind::Auth, "account_unauthorized"));
        let j = e.json();
        assert_eq!(j["ok"], false);
        assert_eq!(j["error"]["code"], "network_error");
        assert_eq!(j["error"]["kind"], "unavailable");
        assert!(j["error"]["hint"].is_string());
    }

    #[test]
    fn every_failure_exits_one() {
        assert_eq!(exit::TABLE, &[(0, exit::TABLE[0].1), (1, exit::TABLE[1].1)]);
        let kinds: Vec<&str> = Kind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            kinds,
            ["usage", "auth", "not_found", "unavailable", "other"],
            "kinds are a stable contract"
        );
    }
}
