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

// ---------- linked iCloud Mail account, through tests/fake_icloud ----------

mod fake_icloud;
use fake_icloud::{Fake, Mode};

/// A home whose config links iCloud Mail (with its password saved), served by a fake iCloud.
struct IcloudHome {
    home: PathBuf,
    fake: Fake,
}

const ICLOUD: &str = "poll_seconds = 60\n\n[accounts.icloud]\nemail = \"me@icloud.com\"\naliases = [\"me@example.com\"]\n";

fn icloud_home(tag: &str, linked: bool, archive: bool) -> IcloudHome {
    let home = temp_home(&format!("icloud-{tag}"));
    let _ = std::fs::remove_dir_all(home.join("config"));
    let cfg = home.join("config").join("cloudmail");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("config.toml"), if linked { ICLOUD } else { "poll_seconds = 30\n" }).unwrap();
    if linked {
        std::fs::create_dir_all(cfg.join("icloud")).unwrap();
        std::fs::write(cfg.join("icloud").join("icloud"), format!("{}\n", fake_icloud::PASSWORD)).unwrap();
    }
    let fake = Fake::start(&home, archive);
    seed(&fake);
    IcloudHome { home, fake }
}

/// A conversation with Ann (her message, your reply in Sent, her answer with an attachment), a
/// copy of the worker's t_1, a newsletter, and an archived receipt.
fn seed(f: &Fake) {
    let mut st = f.state.lock().unwrap();
    st.add("INBOX", &["\\Seen"], "01-Oct-2026 10:00:00 +0000", "From: Ann Example <ann@example.com>\nTo: me@icloud.com\nSubject: Lunch?\nDate: Thu, 1 Oct 2026 10:00:00 +0000\nMessage-ID: <l1@example.com>\nContent-Type: text/plain; charset=utf-8\n\nWant lunch on Friday?\n");
    st.add(
        "INBOX",
        &[],
        "01-Oct-2026 12:00:00 +0000",
        "From: Ann Example <ann@example.com>\nTo: Me <me@icloud.com>\nSubject: Re: Lunch?\nMessage-ID: <l3@example.com>\nIn-Reply-To: <l2@icloud.com>\nReferences: <l1@example.com> <l2@icloud.com>\nContent-Type: multipart/mixed; boundary=\"b1\"\n\n--b1\nContent-Type: multipart/alternative; boundary=\"b2\"\n\n--b2\nContent-Type: text/plain; charset=utf-8\n\nGreat, see the menu.\n--b2\nContent-Type: text/html; charset=utf-8\n\n<p>Great, see the <b>menu</b>.</p>\n--b2--\n--b1\nContent-Type: application/pdf; name=\"menu.pdf\"\nContent-Disposition: attachment; filename=\"menu.pdf\"\nContent-Transfer-Encoding: base64\n\nJVBERi0xLjQgZmFrZQ==\n--b1--\n",
    );
    st.add("INBOX", &["\\Seen"], "02-Oct-2026 08:00:00 +0000", "From: Joe <joe@x.com>\nTo: me@icloud.com\nSubject: Subject t_1\nMessage-ID: <a@x>\n\nForwarded copy of the worker's mail.\n");
    st.add("INBOX", &[], "03-Oct-2026 07:00:00 +0000", "From: News <news@example.org>\nTo: me@icloud.com\nSubject: Caf\u{e9} news\nMessage-ID: <n1@example.org>\nContent-Type: text/html; charset=utf-8\n\n<h1>This week</h1><p>New caf\u{e9} opened.</p>\n");
    st.add("Sent Messages", &["\\Seen"], "01-Oct-2026 11:00:00 +0000", "From: me@icloud.com\nTo: Ann Example <ann@example.com>\nSubject: Re: Lunch?\nMessage-ID: <l2@icloud.com>\nIn-Reply-To: <l1@example.com>\nReferences: <l1@example.com>\n\nYes! Where?\n");
    if st.mailbox("Archive").is_some() {
        st.add("Archive", &["\\Seen"], "15-Sep-2026 09:00:00 +0000", "From: Shop <shop@example.com>\nTo: me@icloud.com\nSubject: Your receipt\nMessage-ID: <r1@shop>\n\nThanks for your order.\n");
    }
}

impl IcloudHome {
    fn run(&self, m: &Mock, args: &[&str], stdin: Option<&str>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cloudmail"));
        cmd.args(args)
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("HOME", &self.home)
            .env("CLOUDMAIL_API_URL", &m.url)
            .env("CLOUDMAIL_API_TOKEN", "test-token")
            .env("CLOUDMAIL_BROWSER", "true")
            .current_dir(&self.home)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in self.fake.env() {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        if let Some(s) = stdin {
            // A command that fails before reading its stdin closes it early.
            let _ = child.stdin.take().unwrap().write_all(s.as_bytes());
        }
        child.wait_with_output().unwrap()
    }

    fn json(&self, m: &Mock, args: &[&str]) -> Value {
        let o = self.run(m, args, None);
        assert!(o.status.success(), "{args:?}: {}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        json_out(&o)
    }

    fn password_file(&self) -> PathBuf {
        self.home.join("config/cloudmail/icloud/icloud")
    }

    /// The thread ID of the listed thread with this subject.
    fn id_of(&self, v: &Value, subject: &str) -> String {
        v["data"].as_array().unwrap().iter().find(|t| t["subject"] == subject).unwrap_or_else(|| panic!("no {subject} in {v}"))["id"].as_str().unwrap().to_string()
    }
}

#[test]
fn icloud_account_add_list_login_remove() {
    let m = mock(default_handler);
    let h = icloud_home("accounts", false, true);
    let cfg = h.home.join("config/cloudmail/config.toml");
    let pw = format!("{}\n", fake_icloud::PASSWORD);

    let o = h.run(&m, &["account", "add", "icloud", "--password-stdin"], Some(&pw));
    assert_eq!(o.status.code(), Some(2), "no address and no terminal to ask at");
    let o = h.run(&m, &["account", "add", "icloud", "--email", "me@example.com", "--password-stdin"], Some(&pw));
    assert_eq!(o.status.code(), Some(2));
    assert!(json_out(&o)["error"]["hint"].as_str().unwrap().contains("--alias"), "custom domains are aliases");
    let o = h.run(&m, &["account", "add", "icloud", "--email", "me@icloud.com"], None);
    assert_eq!(o.status.code(), Some(3), "no terminal to ask for the password at");
    let v = json_out(&o);
    assert_eq!(v["error"]["code"], "not_logged_in");
    assert!(v["error"]["hint"].as_str().unwrap().contains("--password-stdin"), "{v}");
    let o = h.run(&m, &["account", "add", "icloud", "--email", "me@icloud.com", "--password-stdin"], Some("wrong-pass-word-xxxx\n"));
    assert_eq!(o.status.code(), Some(3));
    assert_eq!(json_out(&o)["error"]["code"], "account_unauthorized");
    assert!(!h.password_file().exists(), "a refused password isn't kept");
    assert!(!std::fs::read_to_string(&cfg).unwrap().contains("accounts"));
    let o = h.run(&m, &["account", "add", "icloud", "--email", "me@icloud.com", "--command", "x", "--password-stdin"], Some(&pw));
    assert_eq!(o.status.code(), Some(2), "--command is for CLI-backed providers");
    let o = h.run(&m, &["account", "add", "gmail", "--email", "me@icloud.com"], None);
    assert_eq!(o.status.code(), Some(2), "--email is for iCloud");

    let o = h.run(&m, &["account", "add", "icloud", "--email", "Me@iCloud.com", "--alias", "me@example.com", "--password-stdin"], Some(&pw));
    assert!(o.status.success(), "{}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    let v = json_out(&o);
    assert_eq!(v["data"]["addresses"], json!(["me@icloud.com", "me@example.com"]));
    assert!(v["summary"].as_str().unwrap_or_default().contains("no Screener") || v.to_string().contains("no Screener"), "{v}");
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(text.contains("[accounts.icloud]") && text.contains("email = \"me@icloud.com\"") && text.contains("me@example.com"), "{text}");
    assert!(!text.contains(fake_icloud::PASSWORD), "the password isn't in config.toml");
    assert_eq!(std::fs::read_to_string(h.password_file()).unwrap().trim(), fake_icloud::PASSWORD);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(h.password_file()).unwrap().permissions().mode() & 0o777, 0o600);
    }
    // iCloud's documented user name (the address's name part) is tried first.
    assert_eq!(h.fake.state.lock().unwrap().logins.last().map(String::as_str), Some("me"));

    let v = h.json(&m, &["account", "list"]);
    let a = &v["data"]["accounts"][0];
    assert_eq!((a["name"].as_str(), a["provider"].as_str(), a["label"].as_str(), a["ok"].as_bool()), (Some("icloud"), Some("icloud"), Some("iCloud Mail"), Some(true)), "{v}");

    let o = h.run(&m, &["account", "login", "icloud", "--password-stdin"], Some(&pw));
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let o = h.run(&m, &["account", "login", "icloud"], None);
    assert_eq!(o.status.code(), Some(3), "a new password needs a terminal or stdin");

    let o = h.run(&m, &["account", "remove", "icloud"], None);
    assert!(o.status.success());
    assert!(!h.password_file().exists(), "unlinking forgets the password");
    assert!(!std::fs::read_to_string(&cfg).unwrap().contains("accounts"));
}

#[test]
fn icloud_inbox_threads_by_conversation_and_hides_copies() {
    let m = mock(default_handler);
    let h = icloud_home("inbox", true, true);
    let v = h.json(&m, &["inbox"]);
    let subjects: Vec<&str> = v["data"].as_array().unwrap().iter().map(|t| t["subject"].as_str().unwrap()).collect();
    // Newest first; Joe's copy of t_1 (same Message-ID) is hidden; the worker's own mail is older.
    assert_eq!(subjects, ["Café news", "Lunch?", "Subject t_1", "Subject t_2"], "{v}");
    assert_eq!(v["meta"]["duplicates_hidden"], 1);
    let lunch = v["data"].as_array().unwrap().iter().find(|t| t["subject"] == "Lunch?").unwrap();
    assert!(lunch["id"].as_str().unwrap().starts_with("icloud:t"), "{lunch}");
    assert_eq!(lunch["account"], "icloud");
    assert_eq!(lunch["folder"], "inbox");
    assert_eq!(lunch["message_count"], 2, "both of Ann's messages; your reply is in Sent");
    assert_eq!(lunch["unread"], true);
    assert_eq!(lunch["has_attachments"], true);
    assert_eq!(lunch["from"]["name"], "Ann Example");
    assert_eq!(lunch["to_address"], "me@icloud.com");
    assert_eq!(lunch["snippet"], "Great, see the menu.");
    assert_eq!(lunch["last_at"], 1790856000000_i64);
    let log = h.fake.log();
    assert!(log.iter().any(|c| c == "EXAMINE \"INBOX\""), "{log:?}");
    assert!(!log.iter().any(|c| c.starts_with("SELECT") || c.contains("STORE") || c.contains("MOVE")), "listing changes nothing: {log:?}");
    assert!(log.iter().any(|c| c.starts_with("UID FETCH") && c.contains("BODY.PEEK[HEADER.FIELDS (") && c.contains("BODY.PEEK[TEXT]<0.2048>")), "{log:?}");

    let v = h.json(&m, &["archive"]);
    assert!(v["data"].as_array().unwrap().iter().any(|t| t["subject"] == "Your receipt" && t["folder"] == "archive"), "{v}");
    let v = h.json(&m, &["sent"]);
    let sent: Vec<&Value> = v["data"].as_array().unwrap().iter().filter(|t| t["account"] == "icloud").collect();
    assert_eq!(sent.len(), 1, "{v}");
    assert_eq!(sent[0]["subject"], "Lunch?", "a reply's thread is named without its Re:");
    assert_eq!(sent[0]["id"], lunch["id"], "the same conversation has the same ID wherever it's listed");
    let v = h.json(&m, &["threads", "list", "--folder", "screener"]);
    assert!(!v["data"].as_array().unwrap().iter().any(|t| t["account"] == "icloud"), "iCloud has no Screener");
    let v = h.json(&m, &["inbox", "--unread"]);
    let icloud: Vec<&str> = v["data"].as_array().unwrap().iter().filter(|t| t["account"] == "icloud").map(|t| t["subject"].as_str().unwrap()).collect();
    assert_eq!(icloud, ["Café news", "Lunch?"]);

    h.fake.clear_log();
    let v = h.json(&m, &["search", "café"]);
    assert!(v["data"].as_array().unwrap().iter().any(|t| t["subject"] == "Café news"), "{v}");
    assert!(h.fake.log().iter().any(|c| c.starts_with("UID SEARCH CHARSET UTF-8 TEXT {")), "non-ASCII searches go as UTF-8 literals: {:?}", h.fake.log());
    let v = h.json(&m, &["search", "lunch"]);
    let hits: Vec<&Value> = v["data"].as_array().unwrap().iter().filter(|t| t["account"] == "icloud").collect();
    assert_eq!(hits.len(), 1, "one conversation, found in the Inbox and Sent: {v}");
    assert_eq!(hits[0]["message_count"], 3);
}

#[test]
fn icloud_thread_read_attachment_raw_and_reply() {
    let m = mock(default_handler);
    let h = icloud_home("read", true, true);
    let inbox = h.json(&m, &["inbox"]);
    let id = h.id_of(&inbox, "Lunch?");
    h.fake.clear_log();
    let v = h.json(&m, &["thread", "read", &id]);
    let msgs = v["data"]["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3, "Ann's two messages and your reply from Sent: {v}");
    assert_eq!(msgs.iter().map(|m| m["outgoing"].as_bool().unwrap()).collect::<Vec<_>>(), [false, true, false]);
    assert_eq!(msgs[1]["from"]["email"], "me@icloud.com");
    assert_eq!(msgs[2]["message_id"], "<l3@example.com>");
    assert!(msgs[2]["text"].as_str().unwrap().contains("Great, see the menu."), "{}", msgs[2]);
    let att = &msgs[2]["attachments"][0];
    assert_eq!((att["filename"].as_str(), att["mime_type"].as_str(), att["size"].as_i64()), (Some("menu.pdf"), Some("application/pdf"), Some(13)), "{att}");
    assert!(!h.fake.log().iter().any(|c| c.contains("STORE") || c.starts_with("SELECT")), "reading leaves mail unread: {:?}", h.fake.log());
    assert!(h.fake.flags_of("INBOX", "<l3@example.com>").is_empty());

    let o = h.run(&m, &["attachment", "save", att["id"].as_str().unwrap(), "-o", "-"], None);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(o.stdout, b"%PDF-1.4 fake");
    let o = h.run(&m, &["raw", msgs[0]["id"].as_str().unwrap()], None);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("Want lunch on Friday?"));

    let o = h.run(&m, &["reply", &id, "--no-quote", "-m", "Friday it is"], None);
    assert!(o.status.success(), "{}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    let v = json_out(&o);
    assert_eq!(v["data"]["thread_id"], id, "{v}");
    assert!(v["data"]["warning"].is_null(), "{v}");
    let sent = h.fake.state.lock().unwrap().sent.clone();
    assert_eq!(sent.len(), 1);
    assert_eq!((sent[0].user.as_str(), sent[0].from.as_str(), sent[0].to.clone()), ("me@icloud.com", "me@icloud.com", vec!["ann@example.com".to_string()]));
    let head = sent[0].data.split("\r\n\r\n").next().unwrap().to_string();
    assert!(head.contains("In-Reply-To: <l3@example.com>") && head.contains("References: <l1@example.com> <l2@icloud.com> <l3@example.com>"), "{head}");
    assert!(head.contains("Subject: Re: Lunch?") && head.starts_with("Message-ID: <"), "{head}");
    // iCloud didn't file it in Sent Messages itself, so a copy was saved there.
    let copies = h.fake.raw_in("Sent Messages");
    assert_eq!(copies.len(), 2, "{copies:?}");
    assert!(copies[1].contains("Friday it is") || copies[1].contains("RnJpZGF5IGl0IGlz"), "{}", copies[1]);
    assert_eq!(h.fake.flags_of("Sent Messages", "In-Reply-To: <l3@example.com>"), ["\\Seen"]);
    // The reply joins the conversation.
    let v = h.json(&m, &["thread", "read", &id]);
    assert_eq!(v["data"]["messages"].as_array().unwrap().len(), 4);
}

#[test]
fn icloud_compose_keeps_bcc_out_of_the_message_and_never_files_twice() {
    let m = mock(default_handler);
    let h = icloud_home("compose", true, true);
    h.fake.set_mode(Mode::AutoSent);
    let o = h.run(&m, &["compose", "--from", "me@example.com", "--to", "Bob <bob@b.com>", "--bcc", "secret@c.com", "--subject", "Hi", "-m", "Hello"], None);
    assert!(o.status.success(), "{}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    let v = json_out(&o);
    assert!(v["data"]["thread_id"].as_str().unwrap().starts_with("icloud:t"), "{v}");
    let sent = h.fake.state.lock().unwrap().sent.clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].from, "me@example.com", "an alias sends through iCloud");
    assert_eq!(sent[0].to, ["bob@b.com", "secret@c.com"]);
    assert!(!sent[0].data.contains("Bcc:") && !sent[0].data.contains("secret@c.com"), "{}", sent[0].data);
    assert!(sent[0].data.contains("From: me@example.com\r\nTo: Bob <bob@b.com>\r\n"), "{}", sent[0].data);
    assert_eq!(h.fake.raw_in("Sent Messages").len(), 2, "iCloud filed it itself; no second copy");
    assert!(!h.fake.log().iter().any(|c| c.starts_with("APPEND")));
    assert!(!requests(&m).iter().any(|r| r.path == "/api/send"), "never through the worker");

    let o = h.run(&m, &["compose", "--from", "other@icloud.com", "--to", "bob@b.com", "--subject", "Hi", "-m", "Hello"], None);
    assert!(o.status.success(), "an address no account owns goes through your worker, as before");
    assert!(requests(&m).iter().any(|r| r.path == "/api/send"));
}

#[test]
fn icloud_moves_and_marks() {
    let m = mock(default_handler);
    let h = icloud_home("moves", true, false);
    let id = h.id_of(&h.json(&m, &["inbox"]), "Lunch?");
    assert!(h.run(&m, &["thread", "archive", &id], None).status.success());
    assert!(h.fake.log().iter().any(|c| c == "CREATE \"Archive\""), "no Archive yet, so one is made: {:?}", h.fake.log());
    assert_eq!(h.fake.uids("INBOX").len(), 2, "both of Ann's messages left the Inbox");
    assert_eq!(h.fake.raw_in("Archive").len(), 2);
    assert_eq!(h.fake.raw_in("Sent Messages").len(), 1, "your reply stays in Sent");
    let v = h.json(&m, &["archive"]);
    assert!(v["data"].as_array().unwrap().iter().any(|t| t["id"] == id.as_str()), "the same ID after archiving: {v}");

    assert!(h.run(&m, &["thread", "markread", &id], None).status.success());
    assert!(h.fake.flags_of("Archive", "<l3@example.com>").contains(&"\\Seen".to_string()));
    assert!(h.run(&m, &["thread", "unread", &id], None).status.success());
    assert!(h.fake.flags_of("Archive", "<l3@example.com>").is_empty(), "the latest message you received is unread again");
    assert!(h.fake.flags_of("Archive", "<l1@example.com>").contains(&"\\Seen".to_string()));

    assert!(h.run(&m, &["thread", "unarchive", &id], None).status.success());
    assert_eq!(h.fake.uids("INBOX").len(), 4);
    assert!(h.fake.raw_in("Archive").is_empty());
    let o = h.run(&m, &["thread", "delete", &id, "--yes"], None);
    assert_eq!(o.status.code(), Some(2), "deleting stays a worker-only action");
}

#[test]
fn a_failing_icloud_never_breaks_your_own_mail() {
    let m = mock(default_handler);
    let h = icloud_home("isolation", true, true);
    let check = |h: &IcloudHome, code: &str, says: &str| {
        let o = h.run(&m, &["inbox"], None);
        assert!(o.status.success(), "{code}: {}", String::from_utf8_lossy(&o.stdout));
        let v = json_out(&o);
        assert_eq!(ids(&v), ["t_1", "t_2"], "{code}");
        assert_eq!(v["meta"]["warnings"][0]["account"], "icloud", "{v}");
        assert_eq!(v["meta"]["warnings"][0]["code"], code, "{v}");
        assert!(v["meta"]["warnings"][0]["message"].as_str().unwrap().contains(says), "{v}");
    };
    h.fake.set_mode(Mode::BadPassword);
    check(&h, "account_unauthorized", "run `cloudmail account login icloud`");
    let o = h.run(&m, &["thread", "read", "icloud:tYUB4"], None);
    assert_eq!(o.status.code(), Some(3), "{}", String::from_utf8_lossy(&o.stdout));
    let o = h.run(&m, &["compose", "--from", "me@icloud.com", "--to", "a@b.com", "--subject", "Hi", "-m", "x"], None);
    assert_eq!(o.status.code(), Some(3), "SMTP refuses the password too: {}", String::from_utf8_lossy(&o.stdout));
    h.fake.set_mode(Mode::Garbage);
    check(&h, "account_unavailable", "unexpected");
    h.fake.set_mode(Mode::Ok);
    std::fs::remove_file(h.password_file()).unwrap();
    check(&h, "account_unauthorized", "no app-specific password is saved");

    // Offline: nothing listens where iCloud should be.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    std::fs::write(h.password_file(), fake_icloud::PASSWORD).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cloudmail"));
    cmd.args(["inbox"])
        .env("XDG_CONFIG_HOME", h.home.join("config"))
        .env("HOME", &h.home)
        .env("CLOUDMAIL_API_URL", &m.url)
        .env("CLOUDMAIL_API_TOKEN", "test-token")
        .env("CLOUDMAIL_ICLOUD_IMAP", format!("localhost:{closed}"))
        .env("CLOUDMAIL_ICLOUD_CA", &h.fake.ca)
        .stdin(Stdio::null());
    let o = cmd.output().unwrap();
    assert!(o.status.success());
    let v = json_out(&o);
    assert_eq!(ids(&v), ["t_1", "t_2"]);
    assert_eq!(v["meta"]["warnings"][0]["code"], "account_unavailable", "{v}");
    assert!(v["meta"]["warnings"][0]["message"].as_str().unwrap().contains("offline?"), "{v}");
}
