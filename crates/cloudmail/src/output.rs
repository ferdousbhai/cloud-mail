//! Response envelope, output modes and exit codes.

use serde::Serialize;
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};

use cloudmail_api::{Error as ApiError, ErrorKind};

/// Process exit codes. Documented in `cloudmail agent-guide` and `cloudmail commands`.
pub mod exit {
    pub const OK: i32 = 0;
    pub const GENERIC: i32 = 1;
    pub const USAGE: i32 = 2;
    pub const AUTH: i32 = 3;
    pub const NOT_FOUND: i32 = 4;
    pub const API: i32 = 5;

    pub const TABLE: &[(i32, &str)] = &[
        (OK, "success"),
        (GENERIC, "other failure (local I/O, a setup step, a cancelled confirmation or editor)"),
        (USAGE, "invalid arguments, a destructive command run without --yes, or a request the worker rejected"),
        (AUTH, "not configured, the API token was rejected, the Cloudflare CLI (cf) isn't logged in, or a linked account (HEY, Gmail) isn't signed in"),
        (NOT_FOUND, "the thread, message, attachment, mailbox or sender does not exist"),
        (API, "the worker could not be reached or returned an error, or a linked account's CLI is missing or failed"),
    ];
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
    Breadcrumb { action: action.into(), command: command.into(), description: description.into() }
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
        Self { silent: true, ..Default::default() }
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
        self.meta.insert(key.into(), serde_json::to_value(value).unwrap_or(Value::Null));
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
                Some(ids) => ids.iter().map(|id| terminal_safe(id).replace(['\n', '\t'], " ")).collect::<Vec<_>>().join("\n"),
                // The command has already run (it may have changed something), so say so rather than fail.
                None => {
                    eprintln!("note: this command has no IDs to list; printing its result instead");
                    pretty(&self.data)
                }
            },
            Mode::Human => {
                let mut out = terminal_safe(if self.human.trim().is_empty() { &self.summary } else { &self.human });
                if !self.breadcrumbs.is_empty() && stdout_is_tty() {
                    out.push_str("\n\n");
                    out.push_str(&dim("Next:"));
                    for b in &self.breadcrumbs {
                        out.push_str(&format!("\n  {}  {}", terminal_safe(&b.command), dim(&terminal_safe(&b.description))));
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
        out.push(if c.is_control() && c != '\n' && c != '\t' { '?' } else { c });
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// A value quoted for a shell command line. Breadcrumbs are meant to be run, and some values
/// come from senders (an address like `"x;rm -rf ~"@evil.example` is valid), so anything beyond
/// plain id/address characters is single-quoted.
pub fn shell_arg(s: &str) -> String {
    let plain = !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "@._+-%/:=,".contains(c));
    if plain { s.to_string() } else { format!("'{}'", s.replace('\'', r"'\''")) }
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
    pub exit: i32,
}

impl CliError {
    pub fn new(code: &str, exit: i32, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), hint: None, exit }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new("usage", exit::USAGE, message)
    }

    pub fn generic(message: impl Into<String>) -> Self {
        Self::new("error", exit::GENERIC, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", exit::NOT_FOUND, message)
    }

    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::new("cancelled", exit::GENERIC, message)
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn json(&self) -> Value {
        let mut err = json!({ "code": self.code, "message": self.message });
        if let Some(h) = &self.hint {
            err["hint"] = json!(h);
        }
        json!({ "ok": false, "error": err })
    }

    pub fn print(&self, mode: Mode) {
        if mode.is_machine() {
            write_line(&pretty(&self.json()));
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
        let (exit, hint) = match e.kind {
            ErrorKind::Config => (exit::AUTH, Some("run `cloudmail setup`, or `cloudmail config set api-url …` and `cloudmail config set api-token …`")),
            ErrorKind::Unauthorized => (exit::AUTH, Some("the api_token doesn't match the worker's API_TOKEN secret; check `cloudmail config show`")),
            ErrorKind::NotFound => (exit::NOT_FOUND, None),
            ErrorKind::BadRequest => (exit::USAGE, None),
            ErrorKind::Network => (exit::API, Some("check api_url with `cloudmail config show` and your connection")),
            ErrorKind::Api | ErrorKind::Decode => (exit::API, None),
            // The message already says which account and what to run.
            ErrorKind::AccountAuth => (exit::AUTH, None),
            ErrorKind::AccountUnavailable => (exit::API, Some("see `cloudmail account list`")),
        };
        let mut err = CliError::new(e.kind.code(), exit, e.message);
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
                eprintln!("{}", terminal_safe(&format!("warning: {} (showing the rest)", w.message)));
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
    fn shell_args_are_quoted_when_they_need_to_be() {
        assert_eq!(shell_arg("t_1a2b"), "t_1a2b");
        assert_eq!(shell_arg("alice@example.com"), "alice@example.com");
        assert_eq!(shell_arg("x;touch${IFS}/tmp/pwn;@evil.example"), "'x;touch${IFS}/tmp/pwn;@evil.example'");
        assert_eq!(shell_arg("it's"), r"'it'\''s'");
        assert_eq!(shell_arg(""), "''");
    }

    #[test]
    fn terminal_safe_keeps_our_styling_only() {
        let styled = format!("{} and {}", dim("d"), bold("b"));
        let evil = "x\x1b]0;PWNED\x07\x1b[2A\x1b[2K\u{9b}31m\nnext";
        let out = terminal_safe(&format!("{styled} {evil}"));
        assert!(out.starts_with(&styled), "{out:?}");
        assert!(!out.contains("\x1b]") && !out.contains("\x1b[2A") && !out.contains('\x07') && !out.contains('\u{9b}'), "{out:?}");
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
    fn api_errors_map_to_exit_codes() {
        let e: CliError = ApiError::new(ErrorKind::Unauthorized, "unauthorized").into();
        assert_eq!((e.exit, e.code.as_str()), (exit::AUTH, "unauthorized"));
        let e: CliError = ApiError::new(ErrorKind::NotFound, "not found").into();
        assert_eq!(e.exit, exit::NOT_FOUND);
        let e: CliError = ApiError::new(ErrorKind::Network, "refused").into();
        assert_eq!(e.exit, exit::API);
        let e2: CliError = ApiError::new(ErrorKind::AccountAuth, "HEY: not signed in").into();
        assert_eq!((e2.exit, e2.code.as_str()), (exit::AUTH, "account_unauthorized"));
        let j = e.json();
        assert_eq!(j["ok"], false);
        assert_eq!(j["error"]["code"], "network_error");
        assert!(j["error"]["hint"].is_string());
    }
}
