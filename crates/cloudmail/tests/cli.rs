//! End-to-end tests: the real `cloudmail` binary against an in-process mock worker.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

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
    assert!(v["error"]["message"].as_str().unwrap().contains("hey auth login"));
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
    assert!(!calls.iter().any(|c| c.contains("modify") || c.contains("send")), "listing changes nothing");
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
    let before = h.calls().len();
    let v = json_out(&h.run(&m, &["threads", "list", "--folder", "screener"], &[]));
    assert!(!ids(&v).iter().any(|i| i.starts_with("gmail:")), "{v}");
    assert_eq!(h.calls().len(), before, "Gmail has no Screener, so it isn't asked");
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
    assert!(v["meta"]["warnings"][0]["message"].as_str().unwrap().contains("run `cloudmail account add gmail` to sign in again"), "{v}");
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
    assert!(v["error"]["message"].as_str().unwrap().contains("cloudmail account add gmail"));
    assert_eq!(h.run(&m, &["thread", "read", "gmail:nope"], &[]).status.code(), Some(4));
    assert_eq!(h.run(&m, &["thread", "read", "gmail:t-a1"], &[("FAKE_GWS_MODE", "offline")]).status.code(), Some(5));
}

#[test]
fn gmail_account_add_list_remove() {
    let m = mock(default_handler);
    let h = gmail_home("accounts", "poll_seconds = 30\n", false);
    let cfg = h.home.join("config/cloudmail/config.toml");
    let client = [("CLOUDMAIL_GOOGLE_CLIENT_ID", "test-client.apps.googleusercontent.com"), ("CLOUDMAIL_GOOGLE_CLIENT_SECRET", "test-secret")];

    let o = h.run(&m, &["account", "add", "gmail", "--login"], &[]);
    assert_eq!(o.status.code(), Some(3), "{}", String::from_utf8_lossy(&o.stdout));
    let v = json_out(&o);
    assert_eq!(v["error"]["code"], "not_configured");
    assert!(v["error"]["message"].as_str().unwrap().contains("Google sign-in isn't configured in this build"), "{v}");

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
    assert!(v["summary"].as_str().unwrap().contains("no Screener"));
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

    let v = json_out(&h.run(&m, &["account", "list"], &[]));
    assert_eq!(v["data"]["accounts"][0]["name"], "gmail");
    assert_eq!(v["data"]["accounts"][0]["ok"], true);
    assert_eq!(v["data"]["accounts"][0]["addresses"][0], "me@gmail.example");
    let v = json_out(&h.run(&m, &["status"], &[]));
    assert_eq!(v["data"]["accounts"][0]["label"], "Gmail");

    // Another Gmail account under its own name, with its own client, directory and ID prefix.
    let o = h.run(&m, &["account", "add", "gmail", "--name", "work", "--client-id", "own-id", "--client-secret", "own-secret", "--login"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(text.contains("[accounts.work]") && text.contains("provider = \"gmail\"") && text.contains("client_id = \"own-id\""), "{text}");
    assert!(h.home.join("config/cloudmail/gws/work/credentials.enc").is_file());
    let v = json_out(&h.run(&m, &["inbox"], &[]));
    assert!(ids(&v).contains(&"work:t-a1".to_string()) && ids(&v).contains(&"gmail:t-a1".to_string()), "{v}");

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
