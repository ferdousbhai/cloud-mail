//! End-to-end tests: the real `cloudmail` binary against an in-process mock worker.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

mod support;

const NO_BUS: &str = "unix:path=/nonexistent/cloudmail-test-bus";

#[derive(Debug, Clone)]
struct Req {
    method: String,
    path: String,
    body: Value,
    auth: String,
}

struct Mock {
    url: String,
    log: Arc<Mutex<Vec<Req>>>,
}

fn mock(handler: impl Fn(&Req) -> (u16, Vec<u8>, &'static str) + Send + Sync + 'static) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let handler = Arc::new(handler);
    let log2 = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let (handler, log) = (handler.clone(), log2.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut parts = line.split_whitespace();
                    let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
                    let (mut len, mut auth) = (0usize, String::new());
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
                            "authorization" => auth = v.trim().to_string(),
                            _ => {}
                        }
                    }
                    let mut body = vec![0; len];
                    reader.read_exact(&mut body).unwrap();
                    let req = Req { method, path, body: serde_json::from_slice(&body).unwrap_or(Value::Null), auth };
                    log.lock().unwrap().push(req.clone());
                    let (status, bytes, ctype) = if req.auth != "Bearer test-token" {
                        (401, br#"{"error":"unauthorized"}"#.to_vec(), "application/json")
                    } else {
                        handler(&req)
                    };
                    let head = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\ncontent-disposition: attachment; filename*=UTF-8''menu%20card.pdf\r\n\r\n",
                        bytes.len()
                    );
                    if stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(&bytes)).is_err() {
                        return;
                    }
                }
            });
        }
    });
    Mock { url, log }
}

fn ok(v: Value) -> (u16, Vec<u8>, &'static str) {
    (200, v.to_string().into_bytes(), "application/json")
}

fn thread_summary(id: &str, folder: &str, last_at: i64) -> Value {
    json!({
        "id": id, "subject": format!("Subject {id}"), "folder": folder, "snippet": "hi",
        "from": { "name": "Joe", "email": "joe@x.com" }, "to_address": "support@example.org",
        "message_count": 1, "unread": true, "has_attachments": true, "last_at": last_at
    })
}

fn detail() -> Value {
    json!({
        "thread": thread_summary("t_1", "inbox", 1000),
        "messages": [{
            "id": "m_1", "thread_id": "t_1", "outgoing": false,
            "from": { "name": "Joe", "email": "joe@x.com" },
            "to": [{ "name": "", "email": "support@example.org" }, { "name": "Ops", "email": "ops@x.com" }],
            "cc": [{ "name": "", "email": "hi@example.com" }], "reply_to": [],
            "subject": "Order", "date": 1000, "text": null,
            "html": "<p>Where is <b>it</b>?</p>", "message_id": "<a@x>",
            "attachments": [{ "id": "a_1", "filename": "menu card.pdf", "mime_type": "application/pdf", "size": 4, "inline": false }],
            "auth": { "dmarc": "fail", "spf": "pass", "dkim": null, "spam_score": null }
        }]
    })
}

fn default_handler(req: &Req) -> (u16, Vec<u8>, &'static str) {
    let p = req.path.as_str();
    match (req.method.as_str(), p) {
        // Every linked account's sender is approved, unless a test decides otherwise.
        ("POST", "/api/senders/lookup") => {
            let all: Vec<Value> = req.body["emails"].as_array().into_iter().flatten().map(|e| json!({ "email": e, "status": "approved" })).collect();
            ok(json!({ "senders": all }))
        }
        ("POST", "/api/senders/batch") => ok(json!({ "ok": true, "changed": 0 })),
        ("GET", "/api/counts") => ok(json!({ "screener": 1, "inbox": 2, "inbox_unread": 1 })),
        ("GET", _) if p.starts_with("/api/threads?") => {
            if p.contains("since=2000") {
                ok(json!({ "threads": [thread_summary("t_new", "screener", 3000)] }))
            } else if p.contains("since=3000") {
                ok(json!({ "threads": [] }))
            } else if p.contains("limit=1&") || p.ends_with("limit=1") {
                ok(json!({ "threads": [thread_summary("t_top", "inbox", 2000)] }))
            } else {
                ok(json!({ "threads": [thread_summary("t_1", "inbox", 1000), thread_summary("t_2", "inbox", 900)] }))
            }
        }
        ("GET", "/api/threads/t_1") => ok(detail()),
        ("GET", "/api/identities") => ok(json!({
            "identities": [{ "name": "Me", "email": "hi@example.com" }, { "name": "Support", "email": "support@example.org" }],
            "default": { "name": "Me", "email": "hi@example.com" }
        })),
        ("GET", "/api/screener") => ok(json!({ "senders": [{ "email": "new@y.com", "name": "New", "thread_count": 2, "last_subject": "Hello", "last_at": 5 }] })),
        ("GET", "/api/mailboxes") => ok(json!({ "mailboxes": [{ "email": "hi@example.com", "name": "Me", "screen": true, "position": 0 }] })),
        ("GET", "/api/settings") => ok(json!({ "settings": { "forward_to": "" } })),
        ("GET", "/api/attachments/a_1") => (200, b"%PDF".to_vec(), "application/pdf"),
        ("POST", "/api/threads/t_1/move") | ("POST", "/api/threads/t_1/read") => ok(json!({ "ok": true })),
        ("POST", "/api/senders/new%40y.com") => ok(json!({ "ok": true, "moved": 2 })),
        ("POST", "/api/send") => ok(json!({ "ok": true, "thread_id": "t_1", "message": null })),
        ("PUT", "/api/mailboxes/support%40x.com") => ok(json!({ "ok": true, "mailbox": { "email": "support@x.com", "name": "X", "screen": false, "position": 1 } })),
        ("PATCH", "/api/settings") => ok(json!({ "ok": true, "settings": { "forward_to": req.body["forward_to"] } })),
        _ => (404, br#"{"error":"not found"}"#.to_vec(), "application/json"),
    }
}

fn temp_home(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cloudmail-it-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cloudmail(m: &Mock, args: &[&str], stdin: Option<&str>) -> Output {
    cloudmail_env(m, args, stdin, &[])
}

/// `cloudmail` with environment variables changed (Some) or removed (None) after the defaults.
fn cloudmail_env(m: &Mock, args: &[&str], stdin: Option<&str>, env: &[(&str, Option<&str>)]) -> Output {
    let home = temp_home("home");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cloudmail"));
    // Never the real session bus (keyring, icloud-session): a test that needs one gives its own.
    cmd.env("DBUS_SESSION_BUS_ADDRESS", NO_BUS);
    cmd.args(args)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("HOME", &home)
        .env("CLOUDMAIL_API_URL", &m.url)
        .env("CLOUDMAIL_API_TOKEN", "test-token")
        .env_remove("CLOUD_MAIL_API_URL")
        .env_remove("CLOUD_MAIL_API_TOKEN")
        .current_dir(&home)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    let mut child = cmd.spawn().unwrap();
    if let Some(s) = stdin {
        child.stdin.take().unwrap().write_all(s.as_bytes()).unwrap();
    }
    child.wait_with_output().unwrap()
}

fn json_out(o: &Output) -> Value {
    serde_json::from_slice(&o.stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

fn requests(m: &Mock) -> Vec<Req> {
    m.log.lock().unwrap().clone()
}

#[test]
fn inbox_envelope_and_selectors() {
    let m = mock(default_handler);
    let o = cloudmail(&m, &["inbox"], None);
    assert!(o.status.success());
    let v = json_out(&o);
    assert_eq!(v["ok"], true);
    assert_eq!(v["summary"], "2 threads in inbox");
    assert_eq!(v["data"][0]["id"], "t_1");
    assert!(v["breadcrumbs"].as_array().unwrap().iter().any(|b| b["command"] == "cloudmail thread read <thread-id>"));

    let o = cloudmail(&m, &["inbox", "--ids-only"], None);
    assert_eq!(String::from_utf8_lossy(&o.stdout), "t_1\nt_2\n");
    let o = cloudmail(&m, &["inbox", "--count"], None);
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "2");
    assert!(requests(&m).iter().any(|r| r.path == "/api/threads?folder=inbox&limit=25"));
}

#[test]
fn thread_read_strips_html_and_flags_dmarc() {
    let m = mock(default_handler);
    let v = json_out(&cloudmail(&m, &["thread", "read", "t_1"], None));
    let msg = &v["data"]["messages"][0];
    assert_eq!(msg["text"], "Where is it?");
    assert!(msg.get("html").is_none());
    assert_eq!(msg["auth"]["dmarc"], "fail");
    assert!(!requests(&m).iter().any(|r| r.path.ends_with("/read")), "read must not mark read by default");

    let o = cloudmail(&m, &["thread", "read", "t_1", "--styled"], None);
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(text.contains("sender not verified (its domain"), "{text}");
    assert!(text.contains("menu card.pdf"));

    cloudmail(&m, &["thread", "read", "t_1", "--mark-read"], None);
    assert!(requests(&m).iter().any(|r| r.path == "/api/threads/t_1/read" && r.body["unread"] == false));
}

#[test]
fn screener_and_bulk_actions() {
    let m = mock(default_handler);
    let v = json_out(&cloudmail(&m, &["screener"], None));
    assert_eq!(v["data"][0]["email"], "new@y.com");

    let v = json_out(&cloudmail(&m, &["screener", "approve", "New <New@Y.com>"], None));
    assert_eq!(v["data"][0]["moved"], 2);
    let r = requests(&m).into_iter().find(|r| r.path == "/api/senders/new%40y.com").unwrap();
    assert_eq!(r.body["status"], "approved");

    // A bad address anywhere in the list decides nobody.
    let o = cloudmail(&m, &["screener", "block", "other@y.com", "not-an-address"], None);
    assert_eq!(o.status.code(), Some(2));
    assert!(!requests(&m).iter().any(|r| r.path == "/api/senders/other%40y.com"));

    // A partial failure still archives what it can, but exits with the failure's code.
    let o = cloudmail(&m, &["thread", "archive", "t_1", "t_missing"], None);
    assert_eq!(o.status.code(), Some(4));
    let v = json_out(&o);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "not_found");
    assert!(v["error"]["message"].as_str().unwrap().starts_with("1 thread archived, 1 failed; t_missing: "), "{v}");
    assert_eq!(v["error"]["hint"], "done: t_1");
    assert!(requests(&m).iter().any(|r| r.path == "/api/threads/t_1/move"));

    let o = cloudmail(&m, &["thread", "delete", "t_1"], None);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(json_out(&o)["error"]["code"], "confirmation_required");
    assert!(!requests(&m).iter().any(|r| r.method == "DELETE"));
}

#[test]
fn reply_uses_receiving_mailbox_and_threads() {
    let m = mock(default_handler);
    let v = json_out(&cloudmail(&m, &["reply", "t_1", "--all", "-m", "On its way", "--dry-run"], None));
    let req = &v["data"]["request"];
    assert_eq!(req["from"], "support@example.org");
    assert_eq!(req["to"], json!(["Joe <joe@x.com>"]));
    assert_eq!(req["cc"], json!(["Ops <ops@x.com>"]));
    assert_eq!(req["subject"], "Re: Order");
    assert_eq!(req["reply_to_message_id"], "m_1");
    assert!(req["text"].as_str().unwrap().starts_with("On its way\n\nOn "));
    assert!(req["text"].as_str().unwrap().contains("> Where is it?"));
    assert!(!requests(&m).iter().any(|r| r.path == "/api/send"), "dry run must not send");

    let o = cloudmail(&m, &["reply", "t_1", "--no-quote"], Some("Thanks!\n"));
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let sent = requests(&m).into_iter().find(|r| r.path == "/api/send").unwrap();
    assert_eq!(sent.body["text"], "Thanks!\n");
    assert_eq!(sent.body["reply_to_message_id"], "m_1");
}

#[test]
fn compose_from_stdin_and_mailbox_settings() {
    let m = mock(default_handler);
    let o = cloudmail(&m, &["compose", "--to", "a@b.com, \"Last, First\" <c@d.com>", "--subject", "Hi"], Some("Body\n"));
    assert!(o.status.success());
    let sent = requests(&m).into_iter().find(|r| r.path == "/api/send").unwrap();
    assert_eq!(sent.body["to"], json!(["a@b.com", "\"Last, First\" <c@d.com>"]));
    assert_eq!(sent.body["text"], "Body\n");

    let o = cloudmail(&m, &["mailbox", "add", "support@x.com", "--name", "X", "--direct"], None);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let put = requests(&m).into_iter().find(|r| r.method == "PUT").unwrap();
    assert_eq!(put.body, json!({ "name": "X", "screen": false }));

    let v = json_out(&cloudmail(&m, &["settings", "set", "forward-to", ""], None));
    assert_eq!(v["data"]["forward_to"], "");
    assert!(requests(&m).iter().any(|r| r.method == "PATCH" && r.body == json!({ "forward_to": "" })));
}

#[test]
fn watch_streams_jsonl_from_newest_activity() {
    let m = mock(default_handler);
    let o = cloudmail(&m, &["watch", "--interval", "0", "--max-polls", "2"], None);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let lines: Vec<Value> = String::from_utf8_lossy(&o.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["event"], "thread");
    assert_eq!(lines[0]["thread"]["id"], "t_new");
    let paths: Vec<String> = requests(&m).into_iter().map(|r| r.path).collect();
    assert!(paths.iter().any(|p| p.contains("since=2000")));
    assert!(paths.iter().any(|p| p.contains("since=3000")));
}

#[test]
fn unread_is_filtered_by_the_worker_and_kept_in_the_next_page_hint() {
    // A full page of unread threads, as a worker that honours unread=1 returns it.
    let m = mock(|_: &Req| {
        let threads: Vec<Value> = (1..=2i64).map(|i| thread_summary(&format!("t_{i}"), "inbox", 100 - i)).collect();
        ok(json!({ "threads": threads }))
    });
    let v = json_out(&cloudmail(&m, &["inbox", "--unread", "--limit", "2", "--since", "5"], None));
    let path = requests(&m).into_iter().map(|r| r.path).find(|p| p.starts_with("/api/threads")).unwrap();
    assert!(path.contains("unread=1") && path.contains("since=5"), "{path}");
    let more = v["breadcrumbs"].as_array().unwrap().iter().find(|b| b["action"] == "more").expect("a next-page hint");
    let cmd = more["command"].as_str().unwrap();
    assert!(cmd.contains("--before 98") && cmd.contains("--unread") && cmd.contains("--since 5"), "{cmd}");
}

#[test]
fn watch_pages_back_through_a_burst_larger_than_one_page() {
    // 250 threads changed after `since`; the worker returns at most `limit` per request, newest first.
    let m = mock(|req: &Req| {
        let q = |k: &str| {
            req.path.split(['?', '&']).find_map(|kv| kv.strip_prefix(&format!("{k}="))).and_then(|v| v.parse::<i64>().ok())
        };
        let (since, before, limit) = (q("since").unwrap_or(0), q("before").unwrap_or(i64::MAX), q("limit").unwrap_or(50));
        let threads: Vec<Value> = (1..=250i64)
            .rev()
            .filter(|at| *at > since && *at < before)
            .take(limit as usize)
            .map(|at| thread_summary(&format!("t_{at}"), "inbox", at))
            .collect();
        ok(json!({ "threads": threads }))
    });
    let o = cloudmail(&m, &["watch", "--since", "0", "--interval", "0", "--max-polls", "1"], None);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let ids: Vec<String> = String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap()["thread"]["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 250);
    assert_eq!(ids.first().map(String::as_str), Some("t_1"));
    assert_eq!(ids.last().map(String::as_str), Some("t_250"));
}

#[test]
fn attachment_save_uses_server_filename() {
    let m = mock(default_handler);
    let dir = temp_home("att");
    let out = format!("{}/", dir.display());
    let v = json_out(&cloudmail(&m, &["attachment", "save", "a_1", "-o", &out], None));
    let path = PathBuf::from(v["data"]["path"].as_str().unwrap());
    assert_eq!(path.file_name().unwrap(), "menu card.pdf");
    assert_eq!(std::fs::read(&path).unwrap(), b"%PDF");
}

#[test]
fn errors_have_codes_and_exit_statuses() {
    let m = mock(default_handler);
    let o = cloudmail(&m, &["thread", "read", "t_nope"], None);
    assert_eq!(o.status.code(), Some(4));
    assert_eq!(json_out(&o)["error"]["message"], "no thread t_nope");

    let o = cloudmail_env(&m, &["inbox"], None, &[("CLOUDMAIL_API_TOKEN", Some("wrong"))]);
    assert_eq!(o.status.code(), Some(3));
    assert_eq!(json_out(&o)["error"]["code"], "unauthorized");

    let o = cloudmail_env(&m, &["status"], None, &[("CLOUDMAIL_API_URL", None), ("CLOUDMAIL_API_TOKEN", None)]);
    assert_eq!(o.status.code(), Some(3));
    assert_eq!(json_out(&o)["error"]["code"], "not_configured");

    let o = cloudmail(&m, &["inbox", "--nope"], None);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(json_out(&o)["error"]["code"], "usage");
}

#[test]
fn self_documentation() {
    let m = mock(default_handler);
    let v = json_out(&cloudmail(&m, &["commands"], None));
    let cmds: Vec<&str> = v["data"]["commands"].as_array().unwrap().iter().map(|c| c["command"].as_str().unwrap()).collect();
    for c in ["cloudmail inbox", "cloudmail thread read", "cloudmail screener approve", "cloudmail reply", "cloudmail watch", "cloudmail setup", "cloudmail mailbox add"] {
        assert!(cmds.contains(&c), "missing {c}");
    }
    let o = cloudmail(&m, &["reply", "--help"], None);
    assert!(String::from_utf8_lossy(&o.stdout).contains("Examples:"));
    let v = json_out(&cloudmail(&m, &["agent-guide"], None));
    assert!(v["data"].as_str().unwrap().contains("# cloudmail for agents"));
    let v = json_out(&cloudmail(&m, &[], None));
    assert_eq!(v["data"]["configured"], true);
}

// ---------- linked HEY account, through tests/fake-hey ----------

/// A home whose config links HEY, served by the fake `hey` (which logs every call).
struct HeyHome {
    home: PathBuf,
    log: PathBuf,
}

fn hey_home(tag: &str) -> HeyHome {
    let home = temp_home(&format!("hey-{tag}"));
    let cfg = home.join("config").join("cloudmail");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("config.toml"), "poll_seconds = 60\n\n[accounts.hey]\n").unwrap();
    let log = home.join("hey.log");
    let _ = std::fs::remove_file(&log);
    HeyHome { home, log }
}

fn fake_hey() -> String {
    format!("{}/tests/fake-hey", env!("CARGO_MANIFEST_DIR"))
}

impl HeyHome {
    fn run(&self, m: &Mock, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cloudmail"));
        // Never the real session bus (keyring, icloud-session): a test that needs one gives its own.
        cmd.env("DBUS_SESSION_BUS_ADDRESS", NO_BUS);
        cmd.args(args)
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("HOME", &self.home)
            .env("CLOUDMAIL_API_URL", &m.url)
            .env("CLOUDMAIL_API_TOKEN", "test-token")
            .env("CLOUDMAIL_HEY_COMMAND", fake_hey())
            .env("FAKE_HEY_LOG", &self.log)
            .env_remove("FAKE_HEY_MODE")
            .current_dir(&self.home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    /// Each `hey` invocation, arguments joined by spaces.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log).unwrap_or_default().lines().map(|l| l.replace('\t', " ")).collect()
    }
}

fn ids(v: &Value) -> Vec<String> {
    v["data"].as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap().to_string()).collect()
}

#[test]
fn hey_inbox_merges_by_time_and_hides_forwarded_copies() {
    let m = mock(default_handler);
    let h = hey_home("inbox");
    let o = h.run(&m, &["inbox"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v = json_out(&o);
    // HEY's copy of t_1 (same sender and subject, same time) is hidden; the bundle row is skipped.
    assert_eq!(ids(&v), ["hey:9001:7001", "t_1", "t_2"]);
    assert_eq!(v["meta"]["duplicates_hidden"], 1);
    let hey = &v["data"][0];
    assert_eq!(hey["account"], "hey");
    assert_eq!(hey["folder"], "inbox");
    assert_eq!(hey["unread"], true);
    assert_eq!(hey["from"]["name"], "Carol Chen");
    assert!(v["data"][1].get("account").is_none(), "worker threads keep their JSON shape");
    assert!(h.calls().iter().any(|c| c.starts_with("box view imbox --limit 25")), "{:?}", h.calls());

    let o = h.run(&m, &["inbox", "--styled"], &[]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("HEY"));
}

#[test]
fn inbox_without_accounts_is_unchanged() {
    let m = mock(default_handler);
    let o = cloudmail(&m, &["inbox"], None);
    let v = json_out(&o);
    assert_eq!(ids(&v), ["t_1", "t_2"]);
    assert!(v["meta"].get("warnings").is_none() && v["meta"].get("duplicates_hidden").is_none());
    let o = cloudmail(&m, &["threads", "list", "--folder", "feed"], None);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(json_out(&o)["error"]["hint"], "link one with `cloudmail account add hey`");
}

#[test]
fn hey_boxes_are_extra_folders() {
    let m = mock(default_handler);
    let h = hey_home("boxes");
    let v = json_out(&h.run(&m, &["threads", "list", "--folder", "feed"], &[]));
    assert_eq!(ids(&v), ["hey:9004:7004"]);
    assert!(!requests(&m).iter().any(|r| r.path.contains("folder=feed")), "the worker has no Feed");
    // Archive is the worker's archive plus HEY's Paper Trail.
    let v = json_out(&h.run(&m, &["archive"], &[]));
    assert!(ids(&v).contains(&"hey:9003:7003".to_string()), "{v}");
    assert_eq!(v["data"][0]["folder"], "paper_trail");
}

#[test]
fn hey_thread_read_attachments_and_reply() {
    let m = mock(default_handler);
    let h = hey_home("read");
    let v = json_out(&h.run(&m, &["thread", "read", "hey:9001:7001"], &[]));
    let t = &v["data"]["thread"];
    assert_eq!(t["subject"], "Quarterly numbers", "from hey reply --dry-run, since thread read has none");
    assert_eq!(t["account"], "hey");
    let msgs = v["data"]["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["id"], "hey:9001/8001");
    assert_eq!(msgs[0]["from"]["name"], "Carol Chen");
    assert_eq!(msgs[0]["text"], "Here are the **numbers**.");
    assert_eq!(msgs[1]["outgoing"], true);
    assert_eq!(msgs[0]["attachments"][0]["id"], "hey:8001:1");
    assert_eq!(msgs[0]["attachments"][1]["inline"], true);
    // Reading changes nothing in HEY, and never sends.
    let calls = h.calls();
    assert!(calls.iter().any(|c| c == "thread read 9001"), "{calls:?}");
    assert!(!calls.iter().any(|c| c.starts_with("seen") || (c.starts_with("reply") && !c.contains("--dry-run"))), "{calls:?}");

    let v = json_out(&h.run(&m, &["attachment", "list", "hey:9001:7001"], &[]));
    assert_eq!(v["data"].as_array().unwrap().len(), 1, "inline images aren't listed");
    let dir = h.home.join("dl");
    std::fs::create_dir_all(&dir).unwrap();
    let v = json_out(&h.run(&m, &["attachment", "save", "hey:8001:1", "-o", &format!("{}/", dir.display())], &[]));
    assert_eq!(std::fs::read(dir.join("numbers.pdf")).unwrap(), b"%PDF-", "{v}");

    let v = json_out(&h.run(&m, &["reply", "hey:9001:7001", "--all", "-m", "On it", "--dry-run"], &[]));
    let req = &v["data"]["request"];
    assert_eq!(req["to"], json!(["Carol Chen <carol@example.net>"]));
    assert_eq!(req["cc"], json!(["Ops <ops@example.net>"]), "your own HEY address is left out");
    assert_eq!(req["from"], "me@hey.example");
    assert_eq!(req["reply_to_message_id"], "hey:9001/8001");

    let o = h.run(&m, &["reply", "hey:9001:7001", "-m", "On it"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let sent = h.calls().into_iter().find(|c| c.starts_with("reply 9001 --replace-recipients")).expect("sent through hey reply");
    assert!(sent.contains("--to carol@example.net") && sent.contains("--message-html <div>On it<br><br>On "), "{sent}");
    assert!(!requests(&m).iter().any(|r| r.path == "/api/send"), "a HEY reply never goes through the worker");
}

#[test]
fn hey_moves_marks_and_composes() {
    let m = mock(default_handler);
    let h = hey_home("actions");
    for args in [&["thread", "archive", "hey:9001:7001"][..], &["thread", "unarchive", "hey:9003:7003"], &["thread", "markread", "hey:9001:7001"], &["thread", "unread", "hey:9001:7001"]] {
        let o = h.run(&m, args, &[]);
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stdout));
    }
    let calls = h.calls();
    for want in ["move 7001 --to trailbox", "move 7003 --to imbox", "seen 7001", "unseen 7001"] {
        assert!(calls.iter().any(|c| c == want), "missing `{want}` in {calls:?}");
    }
    // Mixed IDs go to their own providers.
    let o = h.run(&m, &["thread", "archive", "t_1", "hey:9004:7004"], &[]);
    assert!(o.status.success());
    assert!(requests(&m).iter().any(|r| r.path == "/api/threads/t_1/move"));
    // A search hit outside any box has no box item to move.
    let o = h.run(&m, &["thread", "archive", "hey:9010"], &[]);
    assert_eq!(o.status.code(), Some(2));
    // Deleting stays a worker-only action.
    let o = h.run(&m, &["thread", "delete", "hey:9001:7001", "--yes"], &[]);
    assert_eq!(o.status.code(), Some(2));

    let o = h.run(&m, &["compose", "--from", "me@hey.example", "--to", "Al <a@b.com>, c@d.com", "--subject", "Hi", "-m", "Hello"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    assert!(h.calls().iter().any(|c| c == "compose --to a@b.com,c@d.com --subject Hi --from me@hey.example --message-html <div>Hello</div>"), "{:?}", h.calls());
    assert!(!requests(&m).iter().any(|r| r.path == "/api/send"));
    let o = h.run(&m, &["compose", "--to", "a@b.com", "--subject", "Hi", "-m", "Hello"], &[]);
    assert!(o.status.success());
    assert!(requests(&m).iter().any(|r| r.path == "/api/send"), "no --from: your worker sends, as before");
}

#[test]
fn hey_screener_merges_and_decides_by_address_or_id() {
    let m = mock(default_handler);
    let h = hey_home("screener");
    let v = json_out(&h.run(&m, &["screener"], &[]));
    let emails: Vec<&str> = v["data"].as_array().unwrap().iter().map(|s| s["email"].as_str().unwrap()).collect();
    assert_eq!(emails, ["new@y.com", "dana@example.org"], "HEY's copy of a sender waiting in both shows once");
    assert_eq!(v["data"][1]["id"], "hey:5001");
    assert!(v["data"][0].get("id").is_none());

    let o = h.run(&m, &["screener", "approve", "hey:5001"], &[]);
    assert!(o.status.success());
    assert!(h.calls().iter().any(|c| c == "screener approve 5001"));
    assert!(!requests(&m).iter().any(|r| r.path.starts_with("/api/senders/")), "a HEY-only decision leaves the worker alone");

    // An address is decided everywhere it waits.
    let o = h.run(&m, &["screener", "block", "new@y.com"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    assert!(requests(&m).iter().any(|r| r.path == "/api/senders/new%40y.com" && r.body["status"] == "blocked"));
    assert!(h.calls().iter().any(|c| c == "screener deny 5002"), "{:?}", h.calls());

    let v = json_out(&h.run(&m, &["threads", "list", "--folder", "screener"], &[]));
    assert!(ids(&v).contains(&"hey:9005".to_string()), "{v}");
}

#[test]
fn hey_search_spans_accounts() {
    let m = mock(default_handler);
    let h = hey_home("search");
    let v = json_out(&h.run(&m, &["search", "numbers"], &[]));
    assert_eq!(ids(&v), ["hey:9001:7001", "hey:9010", "t_1", "t_2"]);
    assert!(h.calls().iter().any(|c| c == "search numbers"));
}

#[test]
fn a_failing_hey_never_breaks_your_own_mail() {
    let m = mock(default_handler);
    let h = hey_home("isolation");
    for (mode, code) in [("logged_out", "account_unauthorized"), ("crash", "account_unavailable"), ("garbage", "account_unavailable")] {
        let o = h.run(&m, &["inbox"], &[("FAKE_HEY_MODE", mode)]);
        assert!(o.status.success(), "{mode}: {}", String::from_utf8_lossy(&o.stdout));
        let v = json_out(&o);
        assert_eq!(ids(&v), ["t_1", "t_2"], "{mode}");
        assert_eq!(v["meta"]["warnings"][0]["account"], "hey", "{mode}");
        assert_eq!(v["meta"]["warnings"][0]["code"], code, "{mode}");
        let v = json_out(&h.run(&m, &["screener"], &[("FAKE_HEY_MODE", mode)]));
        assert_eq!(v["data"][0]["email"], "new@y.com", "{mode}");
    }
    let o = h.run(&m, &["inbox", "--styled"], &[("CLOUDMAIL_HEY_COMMAND", "/nonexistent/hey")]);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("the hey CLI isn't installed"), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("t_1"));

    // Asking HEY itself for something does fail, with HEY's reason and exit code.
    let o = h.run(&m, &["thread", "read", "hey:9001:7001"], &[("FAKE_HEY_MODE", "logged_out")]);
    assert_eq!(o.status.code(), Some(3));
    let v = json_out(&o);
    assert_eq!(v["error"]["code"], "account_unauthorized");
    assert!(v["error"]["message"].as_str().unwrap().contains("cloudmail account login hey"));
    let o = h.run(&m, &["thread", "read", "hey:1234"], &[]);
    assert_eq!(o.status.code(), Some(4));
}

#[test]
fn account_add_list_remove() {
    let m = mock(default_handler);
    let h = hey_home("accounts");
    let cfg = h.home.join("config/cloudmail/config.toml");
    std::fs::write(&cfg, "poll_seconds = 30\n").unwrap();

    let o = h.run(&m, &["account", "add", "hey"], &[("FAKE_HEY_MODE", "logged_out")]);
    assert_eq!(o.status.code(), Some(3), "no terminal: no browser login, just the hint");
    let v = json_out(&o);
    assert_eq!(v["error"]["code"], "not_logged_in");
    assert!(v["error"]["hint"].as_str().unwrap().contains("auth login"));
    assert!(!std::fs::read_to_string(&cfg).unwrap().contains("accounts"));

    let o = h.run(&m, &["account", "add", "hey"], &[("CLOUDMAIL_HEY_COMMAND", "/nonexistent/hey")]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(json_out(&o)["error"]["code"], "not_installed");

    let o = h.run(&m, &["account", "add", "hey"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let v = json_out(&o);
    assert_eq!(v["data"]["addresses"], json!(["me@hey.example"]));
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(text.contains("poll_seconds = 30") && text.contains("[accounts.hey]"), "{text}");
    assert!(!h.calls().iter().any(|c| c.starts_with("auth login")));

    let v = json_out(&h.run(&m, &["account", "list"], &[]));
    assert_eq!(v["data"]["accounts"][0]["name"], "hey");
    assert_eq!(v["data"]["accounts"][0]["ok"], true);
    let v = json_out(&h.run(&m, &["status"], &[]));
    assert_eq!(v["data"]["accounts"][0]["label"], "HEY");

    // `config set` rewrites the file and must keep the account.
    assert!(h.run(&m, &["config", "set", "poll-seconds", "45"], &[]).status.success());
    assert!(std::fs::read_to_string(&cfg).unwrap().contains("[accounts.hey]"));

    assert!(h.run(&m, &["account", "remove", "hey"], &[]).status.success());
    assert!(!std::fs::read_to_string(&cfg).unwrap().contains("accounts"));
    assert_eq!(h.run(&m, &["account", "remove", "hey"], &[]).status.code(), Some(4));
}

// ---------- linked Gmail account, through tests/fake-gws ----------

/// A home whose config links Gmail, served by the fake `gws` (which logs every call).
struct GmailHome {
    home: PathBuf,
    log: PathBuf,
}

fn gmail_home(tag: &str, config: &str, signed_in: bool) -> GmailHome {
    let home = temp_home(&format!("gmail-{tag}"));
    let cfg = home.join("config").join("cloudmail");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("config.toml"), config).unwrap();
    let gws = cfg.join("gws").join("gmail");
    if signed_in {
        std::fs::create_dir_all(&gws).unwrap();
        std::fs::write(gws.join("credentials.enc"), "fake").unwrap();
    } else {
        let _ = std::fs::remove_dir_all(&gws);
    }
    let log = home.join("gws.log");
    for f in [log.clone(), home.join("gws.log.env"), home.join("gws.log.sent")] {
        let _ = std::fs::remove_file(f);
    }
    GmailHome { home, log }
}

fn fake_gws() -> String {
    format!("{}/tests/fake-gws", env!("CARGO_MANIFEST_DIR"))
}

impl GmailHome {
    fn run(&self, m: &Mock, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cloudmail"));
        // Never the real session bus (keyring, icloud-session): a test that needs one gives its own.
        cmd.env("DBUS_SESSION_BUS_ADDRESS", NO_BUS);
        cmd.args(args)
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("HOME", &self.home)
            .env("CLOUDMAIL_API_URL", &m.url)
            .env("CLOUDMAIL_API_TOKEN", "test-token")
            .env("CLOUDMAIL_GWS_COMMAND", fake_gws())
            .env("CLOUDMAIL_HEY_COMMAND", fake_hey())
            .env("CLOUDMAIL_BROWSER", fake_gws())
            .env("FAKE_GWS_LOG", &self.log)
            .env("FAKE_HEY_LOG", self.home.join("hey.log"))
            // A gws of your own must never stand in for cloudmail's.
            .env("GOOGLE_WORKSPACE_CLI_TOKEN", "a-token-from-your-own-shell")
            .env_remove("FAKE_GWS_MODE")
            .env_remove("FAKE_HEY_MODE")
            .env_remove("CLOUDMAIL_GOOGLE_CLIENT_ID")
            .env_remove("CLOUDMAIL_GOOGLE_CLIENT_SECRET")
            .current_dir(&self.home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    /// Each `gws` invocation, arguments joined by spaces.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log).unwrap_or_default().lines().map(|l| l.replace('\t', " ")).collect()
    }

    /// The environment each `gws` invocation ran with, as fake-gws recorded it.
    fn envs(&self) -> Vec<String> {
        std::fs::read_to_string(self.home.join("gws.log.env")).unwrap_or_default().lines().map(str::to_string).collect()
    }

    /// Every message sent through `gws gmail users messages send`, decoded.
    fn sent(&self) -> String {
        std::fs::read_to_string(self.home.join("gws.log.sent")).unwrap_or_default()
    }

    fn gws_dir(&self) -> PathBuf {
        self.home.join("config/cloudmail/gws/gmail")
    }
}

const GMAIL: &str = "poll_seconds = 60\n\n[accounts.gmail]\n";

/// The default worker, plus a readable t_2 whose message isn't the one Gmail has.
fn gmail_handler(req: &Req) -> (u16, Vec<u8>, &'static str) {
    if req.method == "GET" && req.path == "/api/threads/t_2" {
        let mut d = detail();
        d["thread"] = thread_summary("t_2", "inbox", 900);
        d["messages"][0]["id"] = json!("m_2");
        d["messages"][0]["message_id"] = json!("<t2@x>");
        return ok(d);
    }
    default_handler(req)
}

#[test]
fn gmail_inbox_merges_by_time_and_hides_exact_copies() {
    let m = mock(gmail_handler);
    let h = gmail_home("inbox", GMAIL, true);
    let o = h.run(&m, &["inbox"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v = json_out(&o);
    // t-a2 carries t_1's Message-ID (hidden although far apart in time); t-a3 has t_2's sender,
    // subject and time but another Message-ID, so it stays.
    assert_eq!(ids(&v), ["gmail:t-a1", "t_1", "gmail:t-a3", "t_2"]);
    assert_eq!(v["meta"]["duplicates_hidden"], 1);
    let g = &v["data"][0];
    assert_eq!(g["account"], "gmail");
    assert_eq!(g["folder"], "inbox");
    assert_eq!(g["unread"], true);
    assert_eq!(g["has_attachments"], true);
    assert_eq!(g["message_count"], 2);
    assert_eq!(g["from"]["name"], "Ana Alvarez", "the latest mail you received, not your reply");
    assert_eq!(g["subject"], "Trip itinerary");
    assert_eq!(g["to_address"], "me@gmail.example");
    let calls = h.calls();
    assert!(calls.iter().any(|c| c.starts_with("gmail users threads list --params") && c.contains(r#""labelIds":["INBOX"]"#) && c.contains(r#""maxResults":25"#)), "{calls:?}");
    assert!(calls.iter().any(|c| c.contains("threads get") && c.contains(r#""format":"metadata""#) && c.contains("Message-ID")), "{calls:?}");
    assert!(!calls.iter().any(|c| c.contains("modify") || c.contains("messages send")), "listing changes nothing");
    // Every run is confined to cloudmail's own gws directory and sign-in.
    let dir = h.gws_dir().display().to_string();
    for e in h.envs() {
        assert!(e.contains(&format!("config_dir={dir} ")) && e.contains("keyring=file") && e.contains("token=unset") && e.contains(&format!("adc={dir}/")) && e.ends_with(&format!("cwd={dir}")), "{e}");
    }

    // Without the worker's message to compare, the sender + subject + time match decides.
    let v = json_out(&h.run(&mock(default_handler), &["inbox"], &[]));
    assert_eq!(ids(&v), ["gmail:t-a1", "t_1", "t_2"]);

    let o = h.run(&m, &["inbox", "--styled"], &[]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("Gmail"));
}

#[test]
fn gmail_folders_map_to_labels() {
    let m = mock(default_handler);
    let h = gmail_home("folders", GMAIL, true);
    let v = json_out(&h.run(&m, &["archive"], &[]));
    assert!(ids(&v).contains(&"gmail:t-a4".to_string()), "{v}");
    assert!(!ids(&v).contains(&"gmail:t-a1".to_string()), "a thread still in the Inbox isn't archived: {v}");
    assert_eq!(v["data"][0]["folder"], "archive");
    let v = json_out(&h.run(&m, &["sent"], &[]));
    assert!(ids(&v).contains(&"gmail:t-a5".to_string()), "{v}");
    assert!(h.calls().iter().any(|c| c.contains(r#""labelIds":["SENT"]"#)));
    let v = json_out(&h.run(&m, &["threads", "list", "--folder", "screener"], &[]));
    assert!(!ids(&v).iter().any(|i| i.starts_with("gmail:")), "every sender is approved here, so nothing waits: {v}");
    let o = h.run(&m, &["threads", "list", "--folder", "feed"], &[]);
    assert_eq!(o.status.code(), Some(2), "HEY's boxes need HEY");
}

#[test]
fn gmail_thread_read_attachments_raw_and_reply() {
    let m = mock(default_handler);
    let h = gmail_home("read", GMAIL, true);
    let v = json_out(&h.run(&m, &["thread", "read", "gmail:t-a1"], &[]));
    let t = &v["data"]["thread"];
    assert_eq!(t["subject"], "Trip itinerary");
    assert_eq!(t["account"], "gmail");
    let msgs = v["data"]["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["id"], "gmail:t-a1/m-a1-1");
    assert_eq!(msgs[0]["from"]["email"], "ana@example.net");
    assert_eq!(msgs[0]["cc"][0]["name"], "Ops, Travel");
    assert_eq!(msgs[0]["message_id"], "<trip-1@example.net>");
    assert!(msgs[0]["text"].as_str().unwrap().starts_with("Here is the plan for Friday."), "{}", msgs[0]["text"]);
    let v = json_out(&h.run(&m, &["thread", "read", "gmail:t-a1", "--html"], &[]));
    assert!(v["data"]["messages"][0]["html"].as_str().unwrap().contains("<b>Friday</b>"), "{v}");
    assert_eq!(msgs[0]["attachments"][0]["id"], "gmail:m-a1-1:1");
    assert_eq!(msgs[0]["attachments"][0]["filename"], "itinerary.pdf");
    assert_eq!(msgs[0]["attachments"][1]["inline"], true);
    assert_eq!(msgs[1]["outgoing"], true);
    assert!(h.calls().iter().any(|c| c.contains("threads get") && c.contains(r#""format":"full""#)));
    assert!(!h.calls().iter().any(|c| c.contains("modify")), "reading doesn't mark read unless asked");
    let v = json_out(&h.run(&m, &["thread", "read", "gmail:t-a1", "--mark-read"], &[]));
    assert_eq!(v["data"]["thread"]["unread"], false);
    assert!(h.calls().iter().any(|c| c.contains("threads modify") && c.contains(r#""removeLabelIds":["UNREAD"]"#)));

    let v = json_out(&h.run(&m, &["attachment", "list", "gmail:t-a1"], &[]));
    assert_eq!(v["data"].as_array().unwrap().len(), 1, "inline images aren't listed");
    let dir = h.home.join("dl");
    std::fs::create_dir_all(&dir).unwrap();
    let o = h.run(&m, &["attachment", "save", "gmail:m-a1-1:1", "-o", &format!("{}/", dir.display())], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    assert_eq!(std::fs::read(dir.join("itinerary.pdf")).unwrap(), b"%PDF-");
    assert!(h.calls().iter().any(|c| c.contains("messages attachments get") && c.contains(r#""messageId":"m-a1-1""#)));
    let o = h.run(&m, &["raw", "gmail:t-a1/m-a1-1", "-o", &format!("{}/", dir.display())], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    assert!(std::fs::read_to_string(dir.join("gmail_t-a1_m-a1-1.eml")).unwrap().contains("Subject: Trip itinerary"));

    let v = json_out(&h.run(&m, &["reply", "gmail:t-a1", "--all", "-m", "See you there", "--dry-run"], &[]));
    let req = &v["data"]["request"];
    assert_eq!(req["to"], json!(["Ana Alvarez <ana@example.net>"]));
    assert_eq!(req["cc"], json!(["Ops, Travel <travel@example.net>"]), "your own Gmail address is left out");
    assert_eq!(req["from"], "me@gmail.example");
    assert_eq!(req["reply_to_message_id"], "gmail:t-a1/m-a1-1");

    let o = h.run(&m, &["reply", "gmail:t-a1", "-m", "See you there"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    assert_eq!(json_out(&o)["data"]["thread_id"], "gmail:t-a1");
    let sent = h.sent();
    for want in ["From: Sam Sample <me@gmail.example>\r\n", "To: Ana Alvarez <ana@example.net>\r\n", "Subject: Re: Trip itinerary\r\n", "In-Reply-To: <trip-1@example.net>\r\n", "References: <trip-0@example.net> <trip-1@example.net>\r\n"] {
        assert!(sent.contains(want), "missing {want:?} in\n{sent}");
    }
    assert!(h.calls().iter().any(|c| c.contains("messages send") && c.contains(r#""threadId":"t-a1""#)), "{:?}", h.calls());
    assert!(!requests(&m).iter().any(|r| r.path == "/api/send"), "a Gmail reply never goes through the worker");
}

#[test]
fn gmail_moves_marks_and_composes() {
    let m = mock(default_handler);
    let h = gmail_home("actions", GMAIL, true);
    for (args, body) in [
        (&["thread", "archive", "gmail:t-a1"][..], r#"{"addLabelIds":[],"removeLabelIds":["INBOX"]}"#),
        (&["thread", "unarchive", "gmail:t-a4"], r#"{"addLabelIds":["INBOX"],"removeLabelIds":[]}"#),
        (&["thread", "markread", "gmail:t-a1"], r#"{"addLabelIds":[],"removeLabelIds":["UNREAD"]}"#),
        (&["thread", "unread", "gmail:t-a1"], r#"{"addLabelIds":["UNREAD"],"removeLabelIds":[]}"#),
    ] {
        let o = h.run(&m, args, &[]);
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stdout));
        assert!(h.calls().last().unwrap().ends_with(&format!("--json {body}")), "{args:?}: {:?}", h.calls().last());
    }
    let o = h.run(&m, &["thread", "archive", "t_1", "gmail:t-a2"], &[]);
    assert!(o.status.success());
    assert!(requests(&m).iter().any(|r| r.path == "/api/threads/t_1/move"));
    assert_eq!(h.run(&m, &["thread", "archive", "gmail:nope"], &[]).status.code(), Some(4));
    assert_eq!(h.run(&m, &["thread", "delete", "gmail:t-a1", "--yes"], &[]).status.code(), Some(2), "deleting stays a worker-only action");

    let o = h.run(&m, &["compose", "--from", "sam@alias.example", "--to", "Lee <lee@example.org>", "--subject", "Lunch", "-m", "Thursday?"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let sent = h.sent();
    assert!(sent.contains("From: Sam at Alias <sam@alias.example>\r\n") && sent.contains("To: Lee <lee@example.org>\r\n") && !sent.contains("In-Reply-To"), "{sent}");
    assert!(!h.calls().iter().any(|c| c.contains("threadId")), "a new message starts its own thread");
    assert!(!requests(&m).iter().any(|r| r.path == "/api/send"));
    let o = h.run(&m, &["compose", "--to", "a@b.com", "--subject", "Hi", "-m", "Hello"], &[]);
    assert!(o.status.success());
    assert!(requests(&m).iter().any(|r| r.path == "/api/send"), "no --from: your worker sends, as before");
}

#[test]
fn gmail_search_uses_gmail_queries() {
    let m = mock(default_handler);
    let h = gmail_home("search", GMAIL, true);
    let v = json_out(&h.run(&m, &["search", "trip", "from:ana"], &[]));
    assert_eq!(ids(&v), ["gmail:t-a1", "gmail:t-a6", "t_1", "t_2"]);
    assert!(h.calls().iter().any(|c| c.contains("threads list") && c.contains(r#""q":"trip from:ana""#)), "{:?}", h.calls());
}

#[test]
fn a_failing_gmail_never_breaks_your_own_mail() {
    let m = mock(default_handler);
    let h = gmail_home("isolation", GMAIL, true);
    for (mode, code) in [("expired", "account_unauthorized"), ("revoked", "account_unauthorized"), ("offline", "account_unavailable"), ("crash", "account_unavailable"), ("garbage", "account_unavailable")] {
        let o = h.run(&m, &["inbox"], &[("FAKE_GWS_MODE", mode)]);
        assert!(o.status.success(), "{mode}: {}", String::from_utf8_lossy(&o.stdout));
        let v = json_out(&o);
        assert_eq!(ids(&v), ["t_1", "t_2"], "{mode}");
        assert_eq!(v["meta"]["warnings"][0]["account"], "gmail", "{mode}");
        assert_eq!(v["meta"]["warnings"][0]["code"], code, "{mode}: {v}");
    }
    let v = json_out(&h.run(&m, &["inbox"], &[("FAKE_GWS_MODE", "expired")]));
    assert!(v["meta"]["warnings"][0]["message"].as_str().unwrap().contains("run `cloudmail account login gmail` to sign in again"), "{v}");
    let o = h.run(&m, &["inbox", "--styled"], &[("CLOUDMAIL_GWS_COMMAND", "/nonexistent/gws")]);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("Google's Workspace CLI isn't installed"), "{}", String::from_utf8_lossy(&o.stderr));

    // Signed out: gws isn't even run.
    let h = gmail_home("signed-out", GMAIL, false);
    let v = json_out(&h.run(&m, &["inbox"], &[]));
    assert_eq!(ids(&v), ["t_1", "t_2"]);
    assert_eq!(v["meta"]["warnings"][0]["code"], "account_unauthorized");
    assert!(h.calls().is_empty(), "{:?}", h.calls());

    // Asking Gmail itself for something does fail, with Gmail's reason and exit code.
    let h = gmail_home("isolation2", GMAIL, true);
    let o = h.run(&m, &["thread", "read", "gmail:t-a1"], &[("FAKE_GWS_MODE", "expired")]);
    assert_eq!(o.status.code(), Some(3));
    let v = json_out(&o);
    assert_eq!(v["error"]["code"], "account_unauthorized");
    assert!(v["error"]["message"].as_str().unwrap().contains("cloudmail account login gmail"));
    assert_eq!(h.run(&m, &["thread", "read", "gmail:nope"], &[]).status.code(), Some(4));
    assert_eq!(h.run(&m, &["thread", "read", "gmail:t-a1"], &[("FAKE_GWS_MODE", "offline")]).status.code(), Some(5));
}

#[test]
fn gmail_account_add_list_remove() {
    let m = mock(default_handler);
    let h = gmail_home("accounts", "poll_seconds = 30\n", false);
    let cfg = h.home.join("config/cloudmail/config.toml");
    let client = [("CLOUDMAIL_GOOGLE_CLIENT_ID", "test-client.apps.googleusercontent.com"), ("CLOUDMAIL_GOOGLE_CLIENT_SECRET", "test-secret")];

    // The build's own client needs nothing set: without a terminal it goes straight to the sign-in hint.
    let o = h.run(&m, &["account", "add", "gmail"], &[]);
    assert_eq!(o.status.code(), Some(3), "{}", String::from_utf8_lossy(&o.stdout));
    assert_eq!(json_out(&o)["error"]["code"], "not_logged_in");

    let o = h.run(&m, &["account", "add", "gmail"], &client);
    assert_eq!(o.status.code(), Some(3), "no terminal: no browser sign-in, just the hint");
    let v = json_out(&o);
    assert_eq!(v["error"]["code"], "not_logged_in");
    assert!(v["error"]["hint"].as_str().unwrap().contains("at a terminal"));

    let o = h.run(&m, &["account", "add", "gmail"], &[("CLOUDMAIL_GWS_COMMAND", "/nonexistent/gws")]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(json_out(&o)["error"]["code"], "not_installed");
    assert!(!std::fs::read_to_string(&cfg).unwrap().contains("accounts"));

    let o = h.run(&m, &["account", "add", "gmail", "--login"], &[client[0], client[1], ("FAKE_GWS_MODE", "deny")]);
    assert_eq!(o.status.code(), Some(3));
    assert!(json_out(&o)["error"]["message"].as_str().unwrap().contains("access_denied"));

    let o = h.run(&m, &["account", "add", "gmail", "--login"], &client);
    assert!(o.status.success(), "{}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    let v = json_out(&o);
    assert_eq!(v["data"]["addresses"], json!(["me@gmail.example", "sam@alias.example"]), "verified send-as addresses, default first");
    assert!(v["summary"].as_str().unwrap().contains("Your Screener decides its new senders"), "{v}");
    let batch = requests(&m).into_iter().find(|r| r.path == "/api/senders/batch").expect("its correspondents are screened in");
    assert_eq!((batch.body["status"].as_str(), batch.body["only_undecided"].as_bool()), (Some("approved"), Some(true)));
    let calls = h.calls();
    assert!(calls.iter().any(|c| c == "auth login --scopes https://www.googleapis.com/auth/gmail.modify"), "{calls:?}");
    assert!(calls.iter().any(|c| c.starts_with("browser https://accounts.google.com/o/oauth2/auth?") && c.contains("client_id=test-client")), "the link opens in the browser: {calls:?}");
    assert!(h.envs().iter().any(|e| e.contains("client_id=test-client.apps.googleusercontent.com")));
    assert!(h.gws_dir().join("credentials.enc").is_file());
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(text.contains("poll_seconds = 30") && text.contains("[accounts.gmail]") && !text.contains("client"), "{text}");

    // Already signed in: nothing to do in the browser, even with no client configured.
    let logins = || h.calls().iter().filter(|c| c.starts_with("auth login")).count();
    let before = logins();
    let o = h.run(&m, &["account", "add", "gmail"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    assert_eq!(logins(), before);
    // A sign-in Google no longer accepts has to be done again.
    let o = h.run(&m, &["account", "add", "gmail"], &[client[0], client[1], ("FAKE_GWS_MODE", "expired")]);
    assert_eq!(o.status.code(), Some(3));
    assert!(json_out(&o)["error"]["message"].as_str().unwrap().contains("needs signing in again"));
    // `account login` signs a linked account in again, whatever the terminal.
    let before = logins();
    let o = h.run(&m, &["account", "login", "gmail"], &client);
    assert!(o.status.success(), "{}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    assert_eq!(logins(), before + 1);
    assert!(json_out(&o)["summary"].as_str().unwrap().starts_with("Signed in to Gmail again"));
    let o = h.run(&m, &["account", "login", "nope"], &[]);
    assert_eq!(json_out(&o)["error"]["code"], "not_found");

    let v = json_out(&h.run(&m, &["account", "list"], &[]));
    assert_eq!(v["data"]["accounts"][0]["name"], "gmail");
    assert_eq!(v["data"]["accounts"][0]["ok"], true);
    assert_eq!(v["data"]["accounts"][0]["addresses"][0], "me@gmail.example");
    let v = json_out(&h.run(&m, &["status"], &[]));
    assert_eq!(v["data"]["accounts"][0]["label"], "Gmail");

    // Another Gmail account under its own name, with its own client (its secret in the keyring),
    // directory and ID prefix.
    let mut bus = support::Bus::start(&h.home.join("bus"));
    let keyring = support::keyring(&mut bus, &[]);
    let on_bus = [("DBUS_SESSION_BUS_ADDRESS", bus.address.as_str())];
    let o = h.run(&m, &["account", "add", "gmail", "--name", "work", "--client-id", "own-id", "--client-secret", "own-secret", "--login"], &on_bus);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(text.contains("[accounts.work]") && text.contains("provider = \"gmail\"") && text.contains("client_id = \"own-id\"") && !text.contains("own-secret"), "{text}");
    assert_eq!(support::secret(&keyring, "client_secret:work").as_deref(), Some("own-secret"));
    assert!(h.home.join("config/cloudmail/gws/work/credentials.enc").is_file());
    let v = json_out(&h.run(&m, &["inbox"], &on_bus));
    assert!(ids(&v).contains(&"work:t-a1".to_string()) && ids(&v).contains(&"gmail:t-a1".to_string()), "{v}");
    let o = h.run(&m, &["account", "remove", "work"], &on_bus);
    assert!(o.status.success());
    assert_eq!(support::secret(&keyring, "client_secret:work"), None, "its secret goes with it");

    let o = h.run(&m, &["account", "remove", "gmail"], &[]);
    assert!(o.status.success());
    assert_eq!(json_out(&o)["data"]["signed_out"], true);
    assert!(!h.gws_dir().exists(), "cloudmail's own Gmail sign-in goes with the account");
    assert!(!std::fs::read_to_string(&cfg).unwrap().contains("[accounts.gmail]"));
    assert_eq!(h.run(&m, &["account", "remove", "gmail"], &[]).status.code(), Some(4));
    assert!(!h.calls().iter().any(|c| c.starts_with("auth logout")), "signing out is removing cloudmail's own directory");
}

#[test]
fn gmail_and_hey_together() {
    let m = mock(gmail_handler);
    let h = gmail_home("both", "[accounts.gmail]\n\n[accounts.hey]\n", true);
    let v = json_out(&h.run(&m, &["inbox"], &[]));
    assert_eq!(ids(&v), ["gmail:t-a1", "hey:9001:7001", "t_1", "gmail:t-a3", "t_2"]);
    assert_eq!(v["meta"]["duplicates_hidden"], 2, "one copy each");
    let o = h.run(&m, &["archive", "--styled"], &[]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("Gmail") && out.contains("HEY"), "{out}");
}

/// Files to attach, in a directory of their own: report.pdf (binary), notes.txt, and a second notes.txt.
fn attachment_files(tag: &str) -> PathBuf {
    let dir = temp_home(&format!("files-{tag}"));
    std::fs::write(dir.join("report.pdf"), [0x25, 0x50, 0x44, 0x46, 0x00, 0xff, 0x10]).unwrap();
    std::fs::write(dir.join("notes.txt"), "first").unwrap();
    std::fs::create_dir_all(dir.join("other")).unwrap();
    std::fs::write(dir.join("other/notes.txt"), "second").unwrap();
    dir
}

fn path_arg(dir: &std::path::Path, name: &str) -> String {
    dir.join(name).display().to_string()
}

#[test]
fn compose_and_reply_attach_files_through_the_worker() {
    let m = mock(default_handler);
    let files = attachment_files("worker");
    let (pdf, notes) = (path_arg(&files, "report.pdf"), path_arg(&files, "notes.txt"));
    let o = cloudmail(&m, &["compose", "--to", "a@b.com", "--subject", "Files", "-m", "Attached", "--attach", &pdf, "--attach", &notes], None);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    assert_eq!(json_out(&o)["summary"], "Sent \"Files\" to a@b.com with 2 attachments");
    let sent = requests(&m).into_iter().find(|r| r.path == "/api/send").unwrap();
    assert_eq!(
        sent.body["attachments"],
        json!([
            { "filename": "report.pdf", "mime_type": "application/pdf", "content": "JVBERgD/EA==" },
            { "filename": "notes.txt", "mime_type": "text/plain", "content": "Zmlyc3Q=" },
        ])
    );

    // A dry run lists the files instead of carrying them.
    let v = json_out(&cloudmail(&m, &["compose", "--to", "a@b.com", "--subject", "Files", "-m", "x", "--attach", &pdf, "--dry-run"], None));
    assert_eq!(v["data"]["request"]["attachments"], json!([{ "filename": "report.pdf", "mime_type": "application/pdf", "size": 7 }]));
    assert_eq!(v["summary"], "Would send \"Files\" to a@b.com with 1 attachment (dry run)");

    let o = cloudmail(&m, &["reply", "t_1", "--no-quote", "-m", "Here", "--attach", &notes], None);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let sent = requests(&m).into_iter().rfind(|r| r.path == "/api/send").unwrap();
    assert_eq!(sent.body["reply_to_message_id"], "m_1");
    assert_eq!(sent.body["attachments"][0]["filename"], "notes.txt");

    // Without --attach the request is as it always was.
    let o = cloudmail(&m, &["compose", "--to", "a@b.com", "--subject", "Plain", "-m", "x"], None);
    assert!(o.status.success());
    assert!(requests(&m).into_iter().rfind(|r| r.path == "/api/send").unwrap().body.get("attachments").is_none());
    assert_eq!(requests(&m).iter().filter(|r| r.path == "/api/send").count(), 3);
}

#[test]
fn bad_attachments_fail_before_anything_is_sent() {
    let m = mock(default_handler);
    let files = attachment_files("bad");
    let missing = path_arg(&files, "nope.pdf");
    let o = cloudmail(&m, &["compose", "--to", "a@b.com", "--subject", "x", "-m", "x", "--attach", &missing], None);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(json_out(&o)["error"]["message"], format!("can't attach {missing}: no such file"));
    let o = cloudmail(&m, &["reply", "t_1", "-m", "x", "--attach", &files.display().to_string()], None);
    assert_eq!(o.status.code(), Some(2));
    assert!(json_out(&o)["error"]["message"].as_str().unwrap().ends_with("it's a directory"));

    // Over the worker's limit: refused here, with the limit in the message, even on a dry run.
    let big = files.join("big.bin");
    std::fs::write(&big, vec![0u8; 3700 * 1024]).unwrap();
    for extra in [&[][..], &["--dry-run"]] {
        let mut args = vec!["compose", "--to", "a@b.com", "--subject", "x", "-m", "x", "--attach", big.to_str().unwrap()];
        args.extend(extra);
        let o = cloudmail(&m, &args, None);
        assert_eq!(o.status.code(), Some(2), "{extra:?}");
        let v = json_out(&o);
        assert_eq!(v["error"]["code"], "bad_request");
        assert_eq!(v["error"]["message"], "the attachments are too large to send through Cloudmail: 3.6 MB in all, and it takes at most 3.5 MB");
    }
    assert!(!requests(&m).iter().any(|r| r.path == "/api/send"));

    // The worker's own refusal (413) is a bad request too.
    let m = mock(|req| if req.path == "/api/send" { (413, br#"{"error":"the attachments are too large"}"#.to_vec(), "application/json") } else { default_handler(req) });
    let o = cloudmail(&m, &["compose", "--to", "a@b.com", "--subject", "x", "-m", "x", "--attach", &path_arg(&files, "notes.txt")], None);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(json_out(&o)["error"]["message"], "the attachments are too large");
}

#[test]
fn hey_sends_attachments_by_path_from_a_private_directory() {
    let m = mock(default_handler);
    let h = hey_home("attach");
    let files = attachment_files("hey");
    let _ = std::fs::remove_file(h.home.join("hey.log.attached"));
    let o = h.run(&m, &["compose", "--from", "me@hey.example", "--to", "a@b.com", "--subject", "Hi", "-m", "Hello", "--attach", &path_arg(&files, "report.pdf"), "--attach", &path_arg(&files, "notes.txt"), "--attach", &path_arg(&files, "other/notes.txt")], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let call = h.calls().into_iter().find(|c| c.starts_with("compose ")).unwrap();
    assert!(call.starts_with("compose --to a@b.com --subject Hi --from me@hey.example --message-html <div>Hello</div> --attach "), "{call}");
    let paths: Vec<&str> = call.split(" --attach ").skip(1).collect();
    assert_eq!(paths.len(), 3);
    assert!(paths[0].ends_with("/0/report.pdf") && paths[1].ends_with("/1/notes.txt") && paths[2].ends_with("/2/notes.txt"), "{paths:?}");
    assert!(paths.iter().all(|p| !std::path::Path::new(p).exists()), "removed once hey has run");
    let attached = std::fs::read_to_string(h.home.join("hey.log.attached")).unwrap();
    assert_eq!(attached, "report.pdf\t700\t600\tJVBERgD/EA==\nnotes.txt\t700\t600\tZmlyc3Q=\nnotes.txt\t700\t600\tc2Vjb25k\n");
    assert!(!requests(&m).iter().any(|r| r.path == "/api/send"));

    let o = h.run(&m, &["reply", "hey:9001:7001", "-m", "On it", "--attach", &path_arg(&files, "notes.txt")], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let call = h.calls().into_iter().find(|c| c.starts_with("reply 9001 --replace-recipients")).unwrap();
    assert!(call.contains(" --attach ") && call.ends_with("/0/notes.txt"), "{call}");
}

#[test]
fn gmail_sends_attachments_as_an_uploaded_multipart_message() {
    let m = mock(default_handler);
    let h = gmail_home("attach", GMAIL, true);
    let _ = std::fs::remove_file(h.home.join("gws.log.upload"));
    let files = attachment_files("gmail");
    let o = h.run(&m, &["reply", "gmail:t-a1", "--no-quote", "-m", "Both attached", "--attach", &path_arg(&files, "report.pdf"), "--attach", &path_arg(&files, "notes.txt")], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let call = h.calls().into_iter().find(|c| c.contains("messages send")).unwrap();
    assert!(call.contains(r#"--json {"threadId":"t-a1"} --upload .outgoing-"#) && call.ends_with("/0/message.eml --upload-content-type message/rfc822"), "{call}");
    assert!(!call.contains("raw"), "the message is uploaded, not passed as an argument");
    assert_eq!(std::fs::read_to_string(h.home.join("gws.log.upload")).unwrap(), "700 600\n");
    assert!(std::fs::read_dir(h.gws_dir()).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with(".outgoing")), "removed after sending");

    let sent = h.sent();
    let (head, body) = sent.split_once("\r\n\r\n").unwrap();
    assert!(head.contains("In-Reply-To: <trip-1@example.net>\r\nReferences: <trip-0@example.net> <trip-1@example.net>\r\n"), "{head}");
    let boundary = head.split("multipart/mixed; boundary=\"").nth(1).expect(head).split('"').next().unwrap();
    let parts: Vec<&str> = body.split(&format!("--{boundary}")).collect();
    assert_eq!(parts.len(), 5, "{body}");
    assert!(parts[1].starts_with("\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\nQm90aCBhdHRhY2hlZA==\r\n"), "{}", parts[1]);
    assert_eq!(parts[2], "\r\nContent-Type: application/pdf; name=\"report.pdf\"\r\nContent-Disposition: attachment; filename=\"report.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERgD/EA==\r\n");
    assert_eq!(parts[3], "\r\nContent-Type: text/plain; name=\"notes.txt\"\r\nContent-Disposition: attachment; filename=\"notes.txt\"\r\nContent-Transfer-Encoding: base64\r\n\r\nZmlyc3Q=\r\n");
    assert_eq!(parts[4], "--\r\n");
    assert!(!requests(&m).iter().any(|r| r.path == "/api/send"), "a Gmail reply never goes through the worker");

    // A body far past what one argument could carry goes through too.
    std::fs::write(files.join("long.txt"), "x".repeat(300 * 1024)).unwrap();
    let o = h.run(&m, &["compose", "--from", "me@gmail.example", "--to", "a@b.com", "--subject", "Long", "--message-file", &path_arg(&files, "long.txt")], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    assert!(h.sent().len() > 400 * 1024);
}

// ---------- iCloud Mail through icloud-session, and the keyring (tests/support) ----------

use support::{FMessage, FakeMail, Keyring, SessionShared};

/// A home on a private bus with a keyring, icloud-session and iCloud Mail's web services.
struct IcloudHome {
    home: PathBuf,
    bus: support::Bus,
    keyring: Keyring,
    session: SessionShared,
    mail: FakeMail,
}

const T0: i64 = 1_790_000_000_000;
const HOUR: i64 = 3_600_000;

/// Bob's lunch conversation (his message, your reply in Sent, his answer with a PDF), a newsletter
/// from a sender nobody decided on, mail from a blocked sender, and an archived receipt.
fn icloud_messages() -> Vec<FMessage> {
    let bob = "Bob Lee <bob@example.com>";
    let mut reply = FMessage::new("11", "Sent Messages", "t-lunch", "Ann Example <ann@icloud.com>", "Re: Lunch?", T0 + HOUR);
    reply.to = vec![bob.into()];
    let mut answer = FMessage::new("12", "INBOX", "t-lunch", bob, "Re: Lunch?", T0 + 2 * HOUR);
    answer.seen = false;
    answer.html = Some("<p>See the <b>menu</b>.</p>".into());
    answer.attachment = Some(("3".into(), "menu.pdf".into(), "application/pdf".into(), b"%PDF-1.4 fake".to_vec()));
    answer.references = "<10.t-lunch@example.com> <11.t-lunch@example.com>".into();
    let mut news = FMessage::new("20", "INBOX", "t-news", "News <news@example.org>", "Weekly news", T0 + 3 * HOUR);
    news.seen = false;
    vec![
        FMessage::new("10", "INBOX", "t-lunch", bob, "Lunch?", T0),
        reply,
        answer,
        news,
        FMessage::new("30", "INBOX", "t-spam", "Spam <spam@bad.example>", "Win big", T0 + 4 * HOUR),
        FMessage::new("50", "Archive", "t-receipt", "Shop <shop@example.com>", "Your receipt", T0 - HOUR),
    ]
}

fn icloud_home(tag: &str, linked: bool, signed_in: bool) -> IcloudHome {
    let home = temp_home(&format!("icloud-{tag}"));
    let _ = std::fs::remove_dir_all(home.join("config"));
    std::fs::create_dir_all(home.join("config/cloudmail")).unwrap();
    std::fs::write(home.join("config/cloudmail/config.toml"), if linked { "[accounts.icloud]\n" } else { "" }).unwrap();
    let mut bus = support::Bus::start(&home.join("bus"));
    let keyring = support::keyring(&mut bus, &[]);
    let mail = support::icloud_mail(icloud_messages());
    let session = support::icloud_session(&mut bus, &mail.url, signed_in);
    IcloudHome { home, bus, keyring, session, mail }
}

/// Your worker's Screener: Bob and the shop approved, the spammer blocked, the newsletter undecided.
fn screening(decisions: Arc<Mutex<HashMap<String, String>>>) -> impl Fn(&Req) -> (u16, Vec<u8>, &'static str) + Send + Sync + 'static {
    {
        let mut d = decisions.lock().unwrap();
        for (e, s) in [("bob@example.com", "approved"), ("shop@example.com", "approved"), ("spam@bad.example", "blocked")] {
            d.insert(e.into(), s.into());
        }
    }
    move |req: &Req| {
        let p = req.path.as_str();
        match (req.method.as_str(), p) {
            ("POST", "/api/senders/lookup") => {
                let d = decisions.lock().unwrap();
                let found: Vec<Value> = req.body["emails"].as_array().into_iter().flatten().filter_map(|e| e.as_str()).filter_map(|e| d.get(e).map(|s| json!({ "email": e, "status": s }))).collect();
                ok(json!({ "senders": found }))
            }
            ("POST", "/api/senders/batch") => {
                let mut d = decisions.lock().unwrap();
                let status = req.body["status"].as_str().unwrap().to_string();
                for s in req.body["senders"].as_array().into_iter().flatten() {
                    let e = s["email"].as_str().unwrap().to_ascii_lowercase();
                    if !(req.body["only_undecided"] == json!(true) && d.contains_key(&e)) {
                        d.insert(e, status.clone());
                    }
                }
                ok(json!({ "ok": true, "changed": 1 }))
            }
            ("POST", _) if p.starts_with("/api/senders/") => {
                let e = urlencoding::decode(&p["/api/senders/".len()..]).unwrap().into_owned();
                decisions.lock().unwrap().insert(e, req.body["status"].as_str().unwrap().into());
                ok(json!({ "ok": true, "moved": 0 }))
            }
            _ => default_handler(req),
        }
    }
}

impl IcloudHome {
    fn run(&self, m: &Mock, args: &[&str]) -> Output {
        self.run_env(m, args, &[])
    }

    fn run_env(&self, m: &Mock, args: &[&str], env: &[(&str, Option<&str>)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cloudmail"));
        cmd.args(args)
            .env("DBUS_SESSION_BUS_ADDRESS", &self.bus.address)
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("HOME", &self.home)
            .env("CLOUDMAIL_API_URL", &m.url)
            .env("CLOUDMAIL_API_TOKEN", "test-token")
            .current_dir(&self.home)
            .stdin(Stdio::null());
        for (k, v) in env {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            };
        }
        cmd.output().unwrap()
    }

    fn json(&self, m: &Mock, args: &[&str]) -> Value {
        let o = self.run(m, args);
        assert!(o.status.success(), "{args:?}: {}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        json_out(&o)
    }

    fn calls(&self) -> Vec<String> {
        self.session.lock().unwrap().calls.clone()
    }
}

fn id_of(v: &Value, subject: &str) -> String {
    v["data"].as_array().unwrap().iter().find(|t| t["subject"] == subject).unwrap_or_else(|| panic!("no {subject} in {v}"))["id"].as_str().unwrap().to_string()
}

fn subjects(v: &Value) -> Vec<String> {
    v["data"].as_array().unwrap().iter().map(|t| t["subject"].as_str().unwrap().to_string()).collect()
}

#[test]
fn icloud_links_through_icloud_session() {
    let decisions = Arc::new(Mutex::new(HashMap::new()));
    let m = mock(screening(decisions.clone()));
    let h = icloud_home("link", false, false);
    let cfg = h.home.join("config/cloudmail/config.toml");

    // No icloud-session on the bus at all.
    let mut lonely = support::Bus::start(&h.home.join("lonely-bus"));
    let _ = support::keyring(&mut lonely, &[]);
    let o = h.run_env(&m, &["account", "add", "icloud"], &[("DBUS_SESSION_BUS_ADDRESS", Some(&lonely.address))]);
    assert_eq!(o.status.code(), Some(1), "{}", String::from_utf8_lossy(&o.stdout));
    let v = json_out(&o);
    assert_eq!(v["error"]["code"], "not_installed");
    assert!(v["error"]["hint"].as_str().unwrap().contains("icloud-for-omarchy"), "{v}");

    let o = h.run(&m, &["account", "add", "icloud"]);
    assert_eq!(o.status.code(), Some(3), "signed out and no terminal: nothing opens");
    assert_eq!(json_out(&o)["error"]["code"], "not_logged_in");
    assert!(!h.calls().contains(&"SignIn".to_string()));

    let v = h.json(&m, &["account", "add", "icloud", "--login"]);
    assert!(h.calls().contains(&"SignIn".to_string()), "icloud-session's own sign-in window");
    assert_eq!(v["data"]["addresses"], json!(["ann@icloud.com", "hi@ann.dev"]), "the addresses Mail sends from");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap().trim(), "[accounts.icloud]", "nothing else to keep");
    // Its correspondents (Inbox senders, recipients of sent mail) are screened in, a no kept.
    let batch = requests(&m).into_iter().find(|r| r.path == "/api/senders/batch").unwrap();
    assert_eq!(batch.body["only_undecided"], true);
    let emails: Vec<&str> = batch.body["senders"].as_array().unwrap().iter().map(|s| s["email"].as_str().unwrap()).collect();
    assert!(emails.contains(&"news@example.org") && emails.contains(&"bob@example.com") && !emails.iter().any(|e| e.ends_with("icloud.com")), "{emails:?}");
    assert_eq!(decisions.lock().unwrap()["spam@bad.example"], "blocked");

    let v = h.json(&m, &["account", "list"]);
    assert_eq!((v["data"]["accounts"][0]["label"].as_str(), v["data"]["accounts"][0]["ok"].as_bool()), (Some("iCloud Mail"), Some(true)), "{v}");
    h.json(&m, &["account", "remove", "icloud"]);
    assert!(!h.calls().contains(&"SignOut".to_string()), "icloud-session stays signed in for the other apps");
    assert!(h.keyring.lock().unwrap().is_empty(), "no secret of cloudmail's own");
}

#[test]
fn icloud_mail_is_screened_by_your_worker() {
    let decisions = Arc::new(Mutex::new(HashMap::new()));
    let m = mock(screening(decisions.clone()));
    let h = icloud_home("screen", true, true);
    let v = h.json(&m, &["inbox"]);
    assert_eq!(subjects(&v), ["Lunch?", "Subject t_1", "Subject t_2"], "the newsletter waits, the spammer is out");
    let lunch = &v["data"][0];
    assert_eq!((lunch["account"].as_str(), lunch["folder"].as_str(), lunch["unread"].as_bool(), lunch["has_attachments"].as_bool()), (Some("icloud"), Some("inbox"), Some(true), Some(true)));
    assert_eq!(lunch["from"]["email"], "bob@example.com");
    let search = h.mail.log("/mailws2/v1/thread/search");
    let r = &search[0];
    assert_eq!(r.cookie, support::COOKIE);
    assert_eq!((r.query["dsid"].as_str(), r.query["clientBuildNumber"].as_str()), ("1234", "2636Hotfix65"));
    assert_eq!(r.body["sessionHeaders"], json!({ "folder": "INBOX", "modseq": null, "threadmodseq": null, "condstore": 1, "qresync": 1, "threadmode": 1 }));
    assert_eq!(r.body["responseType"], "THREAD_DIGEST");
    assert!(h.mail.log("/thread/flag").is_empty() && !h.mail.seen("12"), "listing leaves mail unread");

    let v = h.json(&m, &["screener"]);
    let waiting: Vec<&Value> = v["data"].as_array().unwrap().iter().filter(|s| s["account"] == "icloud").collect();
    assert_eq!(waiting.len(), 1, "{v}");
    assert_eq!((waiting[0]["email"].as_str(), waiting[0]["last_subject"].as_str()), (Some("news@example.org"), Some("Weekly news")));
    let v = h.json(&m, &["threads", "list", "--folder", "screener"]);
    assert!(v["data"].as_array().unwrap().iter().any(|t| t["subject"] == "Weekly news" && t["folder"] == "screener"), "{v}");
    let v = h.json(&m, &["search", "win"]);
    assert!(!subjects(&v).contains(&"Win big".to_string()), "a blocked sender stays out of search");
    let v = h.json(&m, &["archive"]);
    assert!(subjects(&v).contains(&"Your receipt".to_string()), "{v}");

    // A no: decided once in the worker, and the sender's Inbox threads archived in iCloud.
    h.json(&m, &["screener", "block", "news@example.org"]);
    assert_eq!(decisions.lock().unwrap()["news@example.org"], "blocked");
    let mv = h.mail.log("/thread/move");
    assert_eq!(mv.len(), 1);
    assert_eq!((mv[0].body["destFolder"].as_str(), mv[0].body["threadIds"][0].as_str()), (Some("Archive"), Some("t-news")));
    assert_eq!(h.mail.folder_of("20"), "Archive");
}

#[test]
fn icloud_threads_read_and_reply_like_the_web_app() {
    let decisions = Arc::new(Mutex::new(HashMap::new()));
    let m = mock(screening(decisions.clone()));
    let h = icloud_home("read", true, true);
    let id = id_of(&h.json(&m, &["inbox"]), "Lunch?");
    let v = h.json(&m, &["thread", "read", &id, "--html"]);
    let msgs = v["data"]["messages"].as_array().unwrap();
    assert_eq!(msgs.iter().map(|m| m["outgoing"].as_bool().unwrap()).collect::<Vec<_>>(), [false, true, false], "your reply from Sent Messages in between");
    assert_eq!(msgs[2]["html"], "<p>See the <b>menu</b>.</p>");
    let att = &msgs[2]["attachments"][0];
    assert_eq!((att["filename"].as_str(), att["mime_type"].as_str(), att["size"].as_i64()), (Some("menu.pdf"), Some("application/pdf"), Some(13)));
    assert!(h.mail.log("/message/get").iter().all(|r| r.body["dontMarkAsRead"] == true), "reading never marks read");
    assert!(!h.mail.seen("12"), "`thread read` leaves it unread");

    let o = h.run(&m, &["attachment", "save", att["id"].as_str().unwrap(), "-o", "-"]);
    assert_eq!(o.stdout, b"%PDF-1.4 fake", "{}", String::from_utf8_lossy(&o.stderr));
    let o = h.run(&m, &["raw", msgs[0]["id"].as_str().unwrap()]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("Text of Lunch?"), "{}", String::from_utf8_lossy(&o.stderr));

    let v = h.json(&m, &["reply", &id, "--no-quote", "-m", "Friday it is"]);
    assert_eq!(v["data"]["thread_id"], id.as_str());
    let st = h.mail.state.lock().unwrap();
    let d = &st.drafts[0];
    assert_eq!(d["from"], "Ann Example <ann@icloud.com>", "named by icloud-session's FullName, as Mail names you");
    assert_eq!(d["to"], json!(["Bob Lee <bob@example.com>"]));
    assert_eq!(d["headerInReplyTo"], "<12.t-lunch@example.com>");
    assert_eq!(d["headerReferences"], "<10.t-lunch@example.com> <11.t-lunch@example.com> <12.t-lunch@example.com>");
    assert_eq!((d["subject"].as_str(), d["textBody"].as_str()), (Some("Re: Lunch?"), Some("Friday it is")));
    assert_eq!(st.sent.len(), 1, "sent by draft/send of Drafts/<uid>");
    drop(st);
    let batch = requests(&m).into_iter().find(|r| r.path == "/api/senders/batch").expect("whoever you write to is screened in");
    assert_eq!((batch.body["status"].as_str(), batch.body["only_undecided"].as_bool()), (Some("approved"), Some(false)));

    let file = h.home.join("plan.txt");
    std::fs::write(&file, "the plan").unwrap();
    h.json(&m, &["compose", "--from", "hi@ann.dev", "--to", "carol@z.com", "--bcc", "dave@w.com", "--subject", "Plan", "-m", "Attached.", "--attach", file.to_str().unwrap()]);
    let st = h.mail.state.lock().unwrap();
    let (q, bytes) = &st.uploads[0];
    assert_eq!((q["X-name"].as_str(), q["X-type"].as_str(), bytes.as_slice()), ("plan.txt", "text/plain", &b"the plan"[..]));
    let d = &st.drafts[1];
    assert_eq!(d["from"], "Ann at Home <hi@ann.dev>");
    assert_eq!((d["bcc"].clone(), d["attachments"][0]["guid"].clone()), (json!(["dave@w.com"]), json!("cachedpart:1")));
    assert_eq!(st.sent.len(), 2);
}

#[test]
fn icloud_moves_and_marks() {
    let m = mock(screening(Arc::new(Mutex::new(HashMap::new()))));
    let h = icloud_home("moves", true, true);
    let id = id_of(&h.json(&m, &["inbox"]), "Lunch?");
    h.json(&m, &["thread", "markread", &id]);
    let flag = h.mail.log("/thread/flag");
    assert_eq!((flag[0].body["method"].as_str(), flag[0].body["sessionHeaders"]["folder"].as_str()), (Some("ADD"), Some("INBOX")));
    assert!(h.mail.seen("12"));
    h.json(&m, &["thread", "unread", &id]);
    assert!(!h.mail.seen("12"));
    h.json(&m, &["thread", "archive", &id]);
    assert_eq!((h.mail.folder_of("10"), h.mail.folder_of("12"), h.mail.folder_of("11")), ("Archive".into(), "Archive".into(), "Sent Messages".into()), "your reply stays in Sent");
    let archived = id_of(&h.json(&m, &["archive"]), "Lunch?");
    h.json(&m, &["thread", "unarchive", &archived]);
    assert_eq!(h.mail.folder_of("12"), "INBOX");
    assert_eq!(h.run(&m, &["thread", "delete", &id, "--yes"]).status.code(), Some(2), "deleting stays a worker-only action");
}

#[test]
fn a_failing_icloud_never_breaks_your_own_mail() {
    let m = mock(screening(Arc::new(Mutex::new(HashMap::new()))));
    let h = icloud_home("failing", true, true);
    let lunch = id_of(&h.json(&m, &["inbox"]), "Lunch?");
    let warning = |h: &IcloudHome| {
        let o = h.run(&m, &["inbox"]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
        let v = json_out(&o);
        assert_eq!(ids(&v), ["t_1", "t_2"]);
        v["meta"]["warnings"][0].clone()
    };
    h.mail.state.lock().unwrap().rotate_cookie = true;
    h.json(&m, &["inbox"]);
    assert!(h.session.lock().unwrap().merged.iter().any(|c| c.starts_with("X-APPLE-WEBAUTH-TOKEN=rotated")), "rotated cookies go back to icloud-session");

    h.session.lock().unwrap().keyring_locked = true;
    let w = warning(&h);
    assert_eq!(w["code"], "account_unavailable");
    assert!(w["message"].as_str().unwrap().contains("keyring"), "{w}");
    h.session.lock().unwrap().keyring_locked = false;

    h.mail.state.lock().unwrap().garbage = true;
    let w = warning(&h);
    assert!(w["message"].as_str().unwrap().contains("doesn't understand"), "{w}");
    h.mail.state.lock().unwrap().garbage = false;

    // Apple says the session ended, and icloud-session confirms it.
    {
        h.mail.state.lock().unwrap().status = Some(421);
        h.session.lock().unwrap().still_signed_in = false;
    }
    let w = warning(&h);
    assert_eq!(w["code"], "account_unauthorized");
    assert!(w["message"].as_str().unwrap().contains("cloudmail account login icloud"), "{w}");
    assert!(h.calls().contains(&"ReportSignInRequired".to_string()));
    let o = h.run(&m, &["thread", "read", &lunch]);
    assert_eq!(o.status.code(), Some(3), "{}", String::from_utf8_lossy(&o.stdout));
}

#[test]
fn secrets_live_in_the_keyring() {
    let m = mock(default_handler);
    let h = icloud_home("keyring", false, true);
    let cfg = h.home.join("config/cloudmail/config.toml");
    std::fs::write(&cfg, format!("api_url = \"{}\"\napi_token = \"test-token\"\n", m.url)).unwrap();
    let no_env = [("CLOUDMAIL_API_URL", None), ("CLOUDMAIL_API_TOKEN", None)];
    let o = h.run_env(&m, &["inbox"], &no_env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    assert_eq!(support::secret(&h.keyring, "api_token").as_deref(), Some("test-token"), "an older config's token moves to the keyring");
    assert!(!std::fs::read_to_string(&cfg).unwrap().contains("api_token"));
    assert!(h.run_env(&m, &["inbox"], &no_env).status.success(), "and is read from there");

    let o = h.run_env(&m, &["config", "set", "api-token", "rotated"], &no_env);
    assert!(o.status.success());
    assert_eq!(support::secret(&h.keyring, "api_token").as_deref(), Some("rotated"));
    assert!(!std::fs::read_to_string(&cfg).unwrap().contains("rotated"));

    // No Secret Service: a clear error, never a file.
    let lonely = support::Bus::start(&h.home.join("lonely-bus"));
    let o = h.run_env(&m, &["inbox"], &[no_env[0], no_env[1], ("DBUS_SESSION_BUS_ADDRESS", Some(&lonely.address))]);
    assert_eq!(o.status.code(), Some(3));
    let v = json_out(&o);
    assert_eq!(v["error"]["code"], "not_configured");
    assert!(v["error"]["message"].as_str().unwrap().contains("keyring (Secret Service) isn't available"), "{v}");
}
