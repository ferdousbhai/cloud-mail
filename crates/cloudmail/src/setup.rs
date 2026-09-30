//! `cloudmail setup` and `mailbox add --route`: drive wrangler to deploy a worker and connect addresses.

use serde::Serialize;
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use cloudmail_api::{Client, Config, MailboxUpdate, config};

use crate::cli::{RouteArgs, SetupArgs};
use crate::output::{CliError, CliResult, Response, crumb, exit};

// ---------- parsing helpers (pure, unit-tested) ----------

pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Fills in wrangler.template.jsonc; without an account the `account_id` line is dropped.
pub fn render_template(template: &str, worker: &str, account: Option<&str>, database: &str, database_id: &str, bucket: &str) -> String {
    let template = match account {
        Some(id) => template.replace("__ACCOUNT_ID__", id),
        None => template.split_inclusive('\n').filter(|l| !l.contains("__ACCOUNT_ID__")).collect(),
    };
    template
        .replace("__WORKER_NAME__", worker)
        .replace("__DATABASE_NAME__", database)
        .replace("__DATABASE_ID__", database_id)
        .replace("__BUCKET_NAME__", bucket)
}

/// The workers.dev URL printed by `wrangler deploy`.
pub fn parse_deploy_url(output: &str) -> Option<String> {
    strip_ansi(output)
        .split_whitespace()
        .find(|w| w.starts_with("https://") && w.contains(".workers.dev"))
        .map(|w| w.trim_end_matches(['.', ',', ')']).to_string())
}

/// The first string value of `"key": "…"` in a wrangler.jsonc.
pub fn jsonc_field(jsonc: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let mut rest = jsonc;
    while let Some(i) = rest.find(&needle) {
        rest = &rest[i + needle.len()..];
        let Some(after) = rest.trim_start().strip_prefix(':') else { continue };
        let after = after.trim_start().strip_prefix('"')?;
        return after.split('"').next().map(str::to_string);
    }
    None
}

/// The worker `"name"` of a wrangler.jsonc.
pub fn parse_worker_name(jsonc: &str) -> Option<String> {
    jsonc_field(jsonc, "name")
}

pub fn parse_d1_list(json_text: &str, name: &str) -> Option<String> {
    // Wrangler may print warnings around the JSON, and they contain brackets too
    // ("▲ [WARNING] …"), so take the first `[` that starts a whole JSON array.
    let list = json_text.match_indices('[').find_map(|(i, _)| {
        match serde_json::Deserializer::from_str(&json_text[i..]).into_iter::<Value>().next() {
            Some(Ok(Value::Array(items))) => Some(items),
            _ => None,
        }
    })?;
    list.iter().find(|d| d["name"] == name).and_then(|d| d["uuid"].as_str().map(str::to_string))
}

pub fn parse_r2_buckets(output: &str) -> Vec<String> {
    strip_ansi(output)
        .lines()
        .filter_map(|l| l.trim().strip_prefix("name:").map(|n| n.trim().to_string()))
        .collect()
}

/// Rows of a wrangler box-drawing table, as trimmed cells (header row included).
pub fn parse_table(output: &str) -> Vec<Vec<String>> {
    strip_ansi(output)
        .lines()
        .filter(|l| l.trim_start().starts_with('│'))
        .map(|l| l.split('│').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect())
        .collect()
}

/// Domains listed with enabled = yes in `wrangler email sending list` / `email routing list`.
pub fn enabled_zones(output: &str) -> Vec<String> {
    parse_table(output)
        .into_iter()
        .filter(|row| row.len() >= 3 && row.iter().any(|c| c == "yes"))
        .map(|row| row[0].clone())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rule {
    pub id: String,
    pub matcher: String,
    pub action: String,
}

pub fn parse_rules(output: &str) -> Vec<Rule> {
    let mut rules = Vec::new();
    let mut current: Option<Rule> = None;
    for line in strip_ansi(output).lines() {
        let l = line.trim();
        if let Some(id) = l.strip_prefix("Rule:") {
            if let Some(r) = current.take() {
                rules.push(r);
            }
            current = Some(Rule { id: id.trim().into(), matcher: String::new(), action: String::new() });
        } else if let Some(r) = current.as_mut() {
            if let Some(m) = l.strip_prefix("Matchers:") {
                r.matcher = m.trim().into();
            } else if let Some(a) = l.strip_prefix("Actions:") {
                r.action = a.trim().into();
            }
        }
    }
    rules.extend(current);
    rules
}

/// (name, id) of each account in `wrangler whoami`'s table.
pub fn parse_accounts(output: &str) -> Vec<(String, String)> {
    parse_table(output)
        .into_iter()
        .filter(|row| row.len() == 2 && row[1].len() == 32 && row[1].chars().all(|c| c.is_ascii_hexdigit()))
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect()
}

/// Destination addresses in `wrangler email routing addresses list`, with whether each is verified.
pub fn parse_destinations(output: &str) -> Vec<(String, bool)> {
    parse_table(output)
        .into_iter()
        .filter(|row| row.len() >= 3 && row[1].contains('@'))
        .map(|row| (row[1].to_ascii_lowercase(), row.len() >= 4))
        .collect()
}

/// A domain's public MX hosts (DNS over HTTPS); None when the lookup itself failed.
fn mx_hosts(domain: &str) -> Option<Vec<String>> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(5))).build().into();
    let url = format!("https://cloudflare-dns.com/dns-query?name={domain}&type=MX");
    let body: Value = agent.get(&url).header("accept", "application/dns-json").call().ok()?.body_mut().read_json().ok()?;
    Some(parse_mx_answer(&body))
}

pub fn parse_mx_answer(body: &Value) -> Vec<String> {
    body["Answer"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["type"] == 15)
        .filter_map(|a| a["data"].as_str()?.split_whitespace().nth(1).map(|h| h.trim_end_matches('.').to_ascii_lowercase()))
        .collect()
}

pub fn domain_of(email: &str) -> Option<&str> {
    email.rsplit_once('@').map(|(_, d)| d).filter(|d| d.contains('.'))
}

/// Parses `address` or `address:direct` / `address:screen`.
pub fn parse_mailbox_spec(spec: &str) -> CliResult<(String, bool)> {
    let (addr, mode) = match spec.rsplit_once(':') {
        Some((a, m)) if !m.contains('@') => (a, Some(m)),
        _ => (spec, None),
    };
    let addr = addr.trim().to_ascii_lowercase();
    if domain_of(&addr).is_none() || addr.starts_with('@') {
        return Err(CliError::usage(format!("not an email address: {spec}")));
    }
    let screen = match mode {
        None | Some("screen") | Some("screened") => true,
        Some("direct") => false,
        Some(other) => return Err(CliError::usage(format!("unknown mailbox mode `{other}` (use :direct or :screen)"))),
    };
    Ok((addr, screen))
}

fn random_token() -> CliResult<String> {
    let mut buf = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .map_err(|e| CliError::generic(format!("could not read /dev/urandom: {e}")))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

// ---------- running wrangler ----------

#[derive(Debug, Serialize)]
pub struct Step {
    pub step: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

pub struct Wrangler {
    program: Vec<String>,
    dir: PathBuf,
    envs: Vec<(String, String)>,
    pub dry_run: bool,
    /// Someone is at a terminal: questions may be asked and steps are shown as they happen.
    pub interactive: bool,
    pub steps: Vec<Step>,
}

impl Wrangler {
    pub fn new(program: &str, dir: PathBuf, dry_run: bool, interactive: bool) -> Self {
        let program = program.split_whitespace().map(str::to_string).collect();
        Self { program, dir, envs: Vec::new(), dry_run, interactive, steps: Vec::new() }
    }

    /// Runs every later wrangler command against this Cloudflare account.
    pub fn use_account(&mut self, id: &str) {
        self.envs.retain(|(k, _)| k != "CLOUDFLARE_ACCOUNT_ID");
        self.envs.push(("CLOUDFLARE_ACCOUNT_ID".into(), id.into()));
    }

    /// Asks a yes/no question at the terminal; None when nobody is there to answer.
    pub fn ask(&self, question: &str) -> Option<bool> {
        if !self.interactive || self.dry_run {
            return None;
        }
        eprint!("{question} [y/N] ");
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok()?;
        Some(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
    }

    fn announce(&self, step: &str) {
        if self.interactive {
            eprint!("[    ...] {step}\r");
            let _ = std::io::stderr().flush();
        }
    }

    fn render(&self, args: &[&str]) -> String {
        let mut parts = self.program.clone();
        parts.extend(args.iter().map(|a| if a.contains(' ') { format!("'{a}'") } else { a.to_string() }));
        parts.join(" ")
    }

    pub fn record(&mut self, step: &str, status: &str, command: Option<String>, detail: Option<String>) {
        if self.interactive {
            eprintln!("[{status:>7}] {step}\x1b[K");
            if let Some(d) = detail.as_ref().filter(|_| matches!(status, "blocked" | "skipped" | "pending")) {
                eprintln!("          {d}");
            }
        }
        self.steps.push(Step { step: step.into(), status: status.into(), command, detail });
    }

    /// Runs a read-only wrangler command (also during --dry-run, where its result only informs the plan).
    pub fn query(&self, args: &[&str]) -> CliResult<String> {
        self.exec(&self.program, args, None)
    }

    /// Like `query`, but during --dry-run a failure reads as empty output so the plan can continue.
    pub fn probe(&self, args: &[&str]) -> CliResult<String> {
        match self.query(args) {
            Err(_) if self.dry_run => Ok(String::new()),
            other => other,
        }
    }

    /// Runs a mutating command, or records it as planned during --dry-run.
    pub fn mutate(&mut self, step: &str, args: &[&str], stdin: Option<&str>) -> CliResult<String> {
        let command = self.render(args);
        if self.dry_run {
            self.record(step, "planned", Some(command), None);
            return Ok(String::new());
        }
        self.announce(step);
        let out = self.exec(&self.program, args, stdin)?;
        self.record(step, "done", Some(command), None);
        Ok(out)
    }

    pub fn run_program(&mut self, step: &str, program: &[&str]) -> CliResult<()> {
        let prog: Vec<String> = program.iter().map(|s| s.to_string()).collect();
        let command = prog.join(" ");
        if self.dry_run {
            self.record(step, "planned", Some(command), None);
            return Ok(());
        }
        self.announce(step);
        self.exec(&prog[..1], &program[1..], None)?;
        self.record(step, "done", Some(command), None);
        Ok(())
    }

    /// Runs wrangler attached to the terminal (for `wrangler login`, which opens a browser).
    pub fn attached(&self, args: &[&str]) -> CliResult<()> {
        let (bin, pre) = self.program.split_first().ok_or_else(|| CliError::usage("empty --wrangler command"))?;
        let status = Command::new(bin)
            .args(pre)
            .args(args)
            .current_dir(&self.dir)
            .envs(self.envs.iter().map(|(k, v)| (k, v)))
            .status()
            .map_err(|e| CliError::generic(format!("could not run `{bin}`: {e}")).hint("install Node.js (npm)"))?;
        if status.success() { Ok(()) } else { Err(CliError::generic(format!("`{}` failed", self.render(args)))) }
    }

    fn exec(&self, program: &[String], args: &[&str], stdin: Option<&str>) -> CliResult<String> {
        let (bin, pre) = program.split_first().ok_or_else(|| CliError::usage("empty --wrangler command"))?;
        let mut cmd = Command::new(bin);
        cmd.args(pre).args(args).current_dir(&self.dir).envs(self.envs.iter().map(|(k, v)| (k, v))).stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        let mut child = cmd
            .spawn()
            .map_err(|e| CliError::generic(format!("could not run `{bin}`: {e}")).hint("install Node.js (npx) or pass --wrangler \"bunx wrangler\""))?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(input.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        if !out.status.success() {
            let clean = strip_ansi(&text);
            let tail: Vec<&str> = clean.lines().filter(|l| !l.trim().is_empty()).collect();
            let tail = tail[tail.len().saturating_sub(6)..].join("\n");
            return Err(CliError::generic(format!("`{}` failed:\n{tail}", self.render(args))));
        }
        Ok(text)
    }
}

/// Where packages install the worker source (read-only).
const PACKAGED_WORKER_DIRS: &[&str] = &["/usr/share/cloudmail/worker", "/usr/local/share/cloudmail/worker"];

fn is_worker_dir(dir: &Path) -> bool {
    dir.join("wrangler.template.jsonc").exists() || dir.join("wrangler.jsonc").exists()
}

/// `--worker-dir`, else `./worker` in a clone, else a writable copy of the packaged worker in
/// `~/.local/share/cloudmail/worker` (refreshed on each run, keeping the generated wrangler.jsonc and node_modules).
pub fn resolve_worker_dir(flag: Option<&Path>) -> CliResult<PathBuf> {
    let data = dirs::data_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/share"));
    let packaged: Vec<&Path> = PACKAGED_WORKER_DIRS.iter().map(Path::new).collect();
    resolve_worker_dir_in(flag, Path::new("worker"), &packaged, &data)
}

fn resolve_worker_dir_in(flag: Option<&Path>, local: &Path, packaged: &[&Path], data: &Path) -> CliResult<PathBuf> {
    if let Some(dir) = flag {
        if is_worker_dir(dir) {
            return Ok(dir.to_path_buf());
        }
        return Err(CliError::usage(format!("{} is not a cloudmail worker directory", dir.display()))
            .hint("pass the worker/ directory of a cloudmail checkout"));
    }
    if is_worker_dir(local) {
        return Ok(local.to_path_buf());
    }
    if let Some(source) = packaged.iter().find(|d| is_worker_dir(d)) {
        let dest = data.join("cloudmail").join("worker");
        copy_tree(source, &dest)?;
        return Ok(dest);
    }
    Err(CliError::usage("couldn't find the cloudmail worker source")
        .hint("install the cloudmail package, run from a clone (git clone https://github.com/ferdousbhai/cloud-mail), or pass --worker-dir"))
}

/// Whether node_modules was installed after package.json last changed (a package upgrade
/// refreshes package.json in the copied worker, so its dependencies must follow).
fn deps_current(dir: &Path) -> bool {
    let modified = |p: PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    match (modified(dir.join("node_modules/.package-lock.json")), modified(dir.join("package.json"))) {
        (Some(installed), Some(manifest)) => installed >= manifest,
        (Some(_), None) => true,
        _ => false,
    }
}

fn copy_tree(from: &Path, to: &Path) -> CliResult<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if std::fs::read(&target).ok() != Some(std::fs::read(entry.path())?) {
            // Unchanged files keep their timestamps, so an upgrade is what triggers a reinstall.
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn wait_until_live(client: &Client) -> CliResult<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        match client.counts() {
            Ok(_) => return Ok(()),
            Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_secs(3)),
            Err(e) => {
                return Err(CliError::generic(format!("the worker was deployed but isn't answering yet: {e}"))
                    .hint("wait a minute, then run `cloudmail status`; re-running setup is safe"));
            }
        }
    }
}

// ---------- routing one address ----------

pub struct RouteOutcome {
    pub address: String,
    pub status: String,
    pub detail: String,
}

/// Asks for the go-ahead on a change that moves existing mail: `--yes`, else the person at the terminal.
fn consent(w: &Wrangler, yes: bool, question: &str) -> bool {
    yes || w.ask(question).unwrap_or(false)
}

fn blocked(w: &mut Wrangler, address: &str, detail: String) -> RouteOutcome {
    let detail = match (w.interactive, w.dry_run) {
        (true, false) => detail,
        (true, true) => format!("{detail}; setup will ask first"),
        _ => format!("{detail}; re-run with --yes to do it"),
    };
    w.record(&format!("route {address}"), "blocked", None, Some(detail.clone()));
    RouteOutcome { address: address.into(), status: "blocked".into(), detail }
}

/// Makes sure the address's domain receives mail through Email Routing and can send, and that the
/// address's routing rule points at the worker. Anything that would take mail away from somewhere
/// else needs `yes` or a yes at the terminal.
pub fn route_address(
    w: &mut Wrangler,
    address: &str,
    worker: &str,
    yes: bool,
    sending_zones: &mut Option<Vec<String>>,
    routing_zones: &mut Option<Vec<String>>,
) -> CliResult<RouteOutcome> {
    let domain = domain_of(address).ok_or_else(|| CliError::usage(format!("not an email address: {address}")))?.to_string();

    if routing_zones.is_none() {
        *routing_zones = Some(enabled_zones(&w.probe(&["email", "routing", "list"])?));
    }
    if !routing_zones.as_ref().is_some_and(|z| z.contains(&domain)) {
        // Enabling Email Routing replaces the domain's MX records: free to do when nothing receives
        // mail there yet, a question when something else does.
        let mx = if yes { None } else { mx_hosts(&domain) };
        let elsewhere: Vec<String> = mx.clone().unwrap_or_default().into_iter().filter(|h| !h.ends_with(".mx.cloudflare.net")).collect();
        let safe = mx.is_some() && elsewhere.is_empty();
        if !safe {
            let current = if elsewhere.is_empty() { "somewhere we couldn't look up".to_string() } else { elsewhere.join(", ") };
            let question = format!("{domain} receives mail at {current}. Move all of {domain}'s mail to Cloudflare (replaces its MX records)?");
            if !consent(w, yes, &question) {
                return Ok(blocked(w, address, format!("{domain} receives mail at {current}; moving it to Cloudflare replaces its MX records")));
            }
        }
        w.mutate(&format!("enable Email Routing for {domain}"), &["email", "routing", "enable", &domain], None)?;
        routing_zones.get_or_insert_with(Vec::new).push(domain.clone());
    }

    if sending_zones.is_none() {
        *sending_zones = Some(enabled_zones(&w.probe(&["email", "sending", "list"])?));
    }
    if sending_zones.as_ref().is_some_and(|z| z.contains(&domain)) {
        w.record(&format!("sending for {domain}"), "exists", None, None);
    } else {
        w.mutate(&format!("enable sending for {domain}"), &["email", "sending", "enable", &domain], None)?;
        sending_zones.get_or_insert_with(Vec::new).push(domain.clone());
    }

    let rules = parse_rules(&w.probe(&["email", "routing", "rules", "list", &domain])?);
    let target = format!("worker:{worker}");
    let matcher = format!("to:{address}");
    let step = format!("route {address}");
    let set_args = |verb: &str, id: Option<&str>| -> Vec<String> {
        let mut a = vec!["email".to_string(), "routing".into(), "rules".into(), verb.into(), domain.clone()];
        a.extend(id.map(str::to_string));
        a.extend(
            ["--match-type", "literal", "--match-field", "to", "--match-value", address, "--action-type", "worker", "--action-value", worker]
                .iter()
                .map(|s| s.to_string()),
        );
        a
    };
    let outcome = |status: &str, detail: String| RouteOutcome { address: address.into(), status: status.into(), detail };
    match rules.iter().find(|r| r.matcher.eq_ignore_ascii_case(&matcher)) {
        Some(r) if r.action == target => {
            w.record(&step, "exists", None, Some(target.clone()));
            Ok(outcome("exists", target))
        }
        Some(r) if !consent(w, yes, &format!("{address} is routed to {}. Send it to Cloudmail instead?", r.action)) => {
            Ok(blocked(w, address, format!("an existing rule sends {address} to {}", r.action)))
        }
        Some(r) => {
            let args = set_args("update", Some(&r.id));
            w.mutate(&step, &args.iter().map(String::as_str).collect::<Vec<_>>(), None)?;
            Ok(outcome(if w.dry_run { "planned" } else { "updated" }, format!("was {}", r.action)))
        }
        None => {
            let args = set_args("create", None);
            w.mutate(&step, &args.iter().map(String::as_str).collect::<Vec<_>>(), None)?;
            Ok(outcome(if w.dry_run { "planned" } else { "created" }, target))
        }
    }
}

pub fn route_for_mailbox(address: &str, args: &RouteArgs, interactive: bool) -> CliResult<Value> {
    let dir = resolve_worker_dir(args.worker_dir.as_deref())?;
    let generated = std::fs::read_to_string(dir.join("wrangler.jsonc")).ok();
    let worker = match &args.worker_name {
        Some(n) => n.clone(),
        None => generated
            .as_deref()
            .and_then(parse_worker_name)
            .ok_or_else(|| CliError::usage("could not read the worker name from wrangler.jsonc").hint("run `cloudmail setup` first"))?,
    };
    let mut w = Wrangler::new(&args.wrangler, dir, false, interactive);
    if let Some(account) = generated.as_deref().and_then(|t| jsonc_field(t, "account_id")) {
        w.use_account(&account);
    }
    let outcome = route_address(&mut w, address, &worker, args.yes, &mut None, &mut None)?;
    Ok(json!({ "status": outcome.status, "detail": outcome.detail, "worker": worker, "steps": w.steps }))
}

// ---------- setup ----------

fn plural_steps(n: usize) -> String {
    if n == 1 { "1 step".into() } else { format!("{n} steps") }
}

/// Reads a line from the terminal after a prompt.
fn prompt(question: &str) -> CliResult<String> {
    eprint!("{question} ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().to_string())
}

/// Makes sure wrangler can reach Cloudflare, logging in through the browser when someone is there
/// to do it, and returns `wrangler whoami`'s output.
fn login(w: &mut Wrangler, wrangler: &str) -> CliResult<String> {
    let whoami = |w: &Wrangler| w.query(&["whoami"]).ok().filter(|o| strip_ansi(o).contains("You are logged in"));
    if let Some(out) = whoami(w) {
        w.record("Cloudflare login", "ok", None, None);
        return Ok(out);
    }
    if w.dry_run {
        w.record("Cloudflare login", "planned", Some(format!("{wrangler} login")), Some("opens your browser".into()));
        return Ok(String::new());
    }
    if w.interactive {
        eprintln!("Log in to Cloudflare in the browser window that opens (create a free account there if you need one).");
        w.attached(&["login"])?;
        if let Some(out) = whoami(w) {
            w.record("Cloudflare login", "done", None, None);
            return Ok(out);
        }
    }
    Err(CliError::new("not_logged_in", exit::AUTH, "not logged in to Cloudflare").hint(format!(
        "run `{wrangler} login`, or set CLOUDFLARE_API_TOKEN to a token that can edit Workers Scripts, D1, Workers R2 Storage, Email Routing Rules, Email Sending and Zone Settings"
    )))
}

/// The Cloudflare account to use: --account / CLOUDFLARE_ACCOUNT_ID, the one setup used before,
/// the only one the login can see, or the person's pick.
fn choose_account(w: &Wrangler, flag: Option<&str>, previous: Option<String>, whoami: &str) -> CliResult<Option<String>> {
    if let Some(id) = flag.map(str::to_string).or(previous) {
        return Ok(Some(id));
    }
    let accounts = parse_accounts(whoami);
    match accounts.len() {
        0 => Ok(None),
        1 => Ok(Some(accounts[0].1.clone())),
        _ if w.interactive && !w.dry_run => {
            eprintln!("Your Cloudflare login can use several accounts:");
            for (i, (name, id)) in accounts.iter().enumerate() {
                eprintln!("  {}. {name} ({id})", i + 1);
            }
            let pick = prompt(&format!("Which one holds your domains? [1-{}]", accounts.len()))?;
            let i = pick.parse::<usize>().ok().filter(|i| (1..=accounts.len()).contains(i)).ok_or_else(|| CliError::new("cancelled", exit::GENERIC, "no account chosen"))?;
            Ok(Some(accounts[i - 1].1.clone()))
        }
        _ => {
            let list = accounts.iter().map(|(n, id)| format!("{id} ({n})")).collect::<Vec<_>>().join(", ");
            Err(CliError::usage(format!("your Cloudflare login can use several accounts: {list}")).hint("pass --account <ID> for the one that holds your domains"))
        }
    }
}

pub fn run(args: &SetupArgs, interactive: bool) -> CliResult {
    let mut specs: Vec<String> = args.mailboxes.iter().chain(&args.mailbox_flags).cloned().collect();
    let existing_cfg = config::read_file(&config::path()).ok().flatten();
    let first_time = existing_cfg.as_ref().and_then(|c| c.api_url.as_ref()).is_none();
    if specs.is_empty() && first_time {
        if !interactive || args.dry_run {
            return Err(CliError::usage("which address should receive mail?").hint("cloudmail setup you@yourdomain.com"));
        }
        let addr = prompt("Email address to set up (e.g. you@yourdomain.com):")?;
        if addr.is_empty() {
            return Err(CliError::new("cancelled", exit::GENERIC, "cancelled"));
        }
        specs.push(addr);
    }
    let mailboxes = specs.iter().map(|m| parse_mailbox_spec(m)).collect::<CliResult<Vec<_>>>()?;

    let dir = resolve_worker_dir(args.worker_dir.as_deref())?;
    let template_path = dir.join("wrangler.template.jsonc");
    let template = std::fs::read_to_string(&template_path)
        .map_err(|e| CliError::generic(format!("could not read {}: {e}", template_path.display())))?;
    let config_path = dir.join("wrangler.jsonc");
    let current = std::fs::read_to_string(&config_path).ok();
    // An existing wrangler.jsonc wins unless --force: reuse its worker, database, bucket and account.
    let keep = current.is_some() && !args.force;
    let field = |k: &str| current.as_deref().filter(|_| keep).and_then(|c| jsonc_field(c, k));
    let worker_name = field("name").unwrap_or_else(|| args.name.clone());
    let db_name = field("database_name").unwrap_or_else(|| args.name.clone());
    let bucket_name = field("bucket_name").unwrap_or_else(|| args.name.clone());
    let mut w = Wrangler::new(&args.wrangler, dir.clone(), args.dry_run, interactive);

    // 1. dependencies
    if deps_current(&dir) {
        w.record("install worker dependencies", "exists", None, None);
    } else {
        w.run_program("install worker dependencies", &["npm", "install", "--no-audit", "--no-fund"])?;
    }

    // 2. login and account
    let whoami = login(&mut w, &args.wrangler)?;
    let account = choose_account(&w, args.account.as_deref(), field("account_id"), &whoami)?;
    if let Some(id) = &account {
        w.use_account(id);
        w.record("Cloudflare account", "ok", None, Some(id.clone()));
    }

    // 3. D1 + R2
    let existing_db = w.query(&["d1", "list", "--json"]).ok().and_then(|o| parse_d1_list(&o, &db_name));
    let database_id = match existing_db {
        Some(id) => {
            w.record(&format!("D1 database {db_name}"), "exists", None, Some(id.clone()));
            id
        }
        None => {
            w.mutate(&format!("create D1 database {db_name}"), &["d1", "create", &db_name], None)?;
            if args.dry_run {
                "<new-database-id>".into()
            } else {
                parse_d1_list(&w.query(&["d1", "list", "--json"])?, &db_name)
                    .ok_or_else(|| CliError::generic("created the D1 database but could not find its id"))?
            }
        }
    };
    let buckets = w.query(&["r2", "bucket", "list"]).map(|o| parse_r2_buckets(&o)).unwrap_or_default();
    if buckets.contains(&bucket_name) {
        w.record(&format!("R2 bucket {bucket_name}"), "exists", None, None);
    } else {
        w.mutate(&format!("create R2 bucket {bucket_name}"), &["r2", "bucket", "create", &bucket_name], None)?;
    }

    // 4. wrangler.jsonc
    let rendered = render_template(&template, &worker_name, account.as_deref(), &db_name, &database_id, &bucket_name);
    if keep {
        let status = if current.as_deref() == Some(rendered.as_str()) { "exists" } else { "kept" };
        w.record("write wrangler.jsonc", status, None, Some(format!("using the existing file (worker \"{worker_name}\"); --force regenerates it")));
    } else if args.dry_run {
        w.record("write wrangler.jsonc", "planned", None, Some(config_path.display().to_string()));
    } else {
        std::fs::write(&config_path, &rendered)?;
        w.record("write wrangler.jsonc", "done", None, Some(config_path.display().to_string()));
    }

    // 5. schema + deploy
    w.mutate("apply database migrations", &["d1", "migrations", "apply", &db_name, "--remote"], None)?;
    let deploy_out = w.mutate("deploy worker", &["deploy"], None).map_err(|e| {
        if e.message.contains("workers.dev subdomain") {
            e.hint("pick a workers.dev subdomain at https://dash.cloudflare.com/?to=/:account/workers/onboarding, then run the same command again")
        } else {
            e
        }
    })?;
    let url = if args.dry_run {
        format!("https://{worker_name}.<your-subdomain>.workers.dev")
    } else {
        parse_deploy_url(&deploy_out).ok_or_else(|| CliError::generic("deployed, but could not find the workers.dev URL in wrangler's output"))?
    };

    // 6. token + local config
    let cfg_path = config::path();
    let same_worker = |configured: &str| {
        if args.dry_run {
            configured.starts_with(&format!("https://{worker_name}.")) && configured.ends_with(".workers.dev")
        } else {
            configured == url
        }
    };
    let reuse = !args.force
        && existing_cfg.as_ref().is_some_and(|c| c.api_url.as_deref().is_some_and(same_worker) && c.api_token.is_some());
    let token = if reuse {
        w.record("API token", "exists", None, Some(cfg_path.display().to_string()));
        existing_cfg.as_ref().and_then(|c| c.api_token.clone()).unwrap_or_default()
    } else if !first_time && !args.force && args.dry_run {
        w.record("API token", "blocked", None, Some(format!("{} points at another worker; setup would stop here without --force", cfg_path.display())));
        String::new()
    } else if !first_time && !args.force {
        return Err(CliError::generic(format!("{} already points at another worker", cfg_path.display()))
            .hint("re-run with --force to rotate the token and overwrite the config"));
    } else {
        let token = if args.dry_run { "<generated>".to_string() } else { random_token()? };
        w.mutate("set API_TOKEN secret", &["secret", "put", "API_TOKEN"], Some(&token))?;
        if args.dry_run {
            w.record("write config", "planned", None, Some(cfg_path.display().to_string()));
        } else {
            let file = config::FileConfig {
                api_url: Some(url.clone()),
                api_token: Some(token.clone()),
                poll_seconds: Some(config::DEFAULT_POLL_SECONDS),
            };
            config::save(&file)?;
            w.record("write config", "done", None, Some(cfg_path.display().to_string()));
        }
        token
    };

    // 7. a new workers.dev hostname and secret take a few seconds to go live
    let client = Client::new(&Config { api_url: url.clone(), api_token: token, poll_seconds: 60 });
    if args.dry_run {
        w.record("wait for the worker", "planned", None, Some(url.clone()));
    } else {
        w.announce("wait for the worker");
        wait_until_live(&client)?;
        w.record("wait for the worker", "done", None, Some(url.clone()));
    }

    // 8. mailboxes and routes
    let mut routes = Vec::new();
    let (mut sending, mut routing) = (None, None);
    for (addr, screen) in &mailboxes {
        let kind = if *screen { "screened" } else { "direct" };
        if args.dry_run {
            w.record(&format!("mailbox {addr}"), "planned", None, Some(kind.into()));
        } else {
            client.put_mailbox(addr, &MailboxUpdate { screen: Some(*screen), ..Default::default() })?;
            w.record(&format!("mailbox {addr}"), "done", None, Some(kind.into()));
        }
        let r = route_address(&mut w, addr, &worker_name, args.yes, &mut sending, &mut routing)?;
        routes.push(json!({ "address": r.address, "status": r.status, "detail": r.detail }));
    }

    // 9. forwarding: Email Routing only forwards to verified destinations, and Cloudflare verifies
    // one by emailing it a link
    if let Some(fwd) = args.forward_to.as_deref().map(str::to_ascii_lowercase).filter(|f| !f.is_empty()) {
        let known = parse_destinations(&w.probe(&["email", "routing", "addresses", "list"])?).into_iter().find(|(a, _)| *a == fwd);
        let pending = format!("Cloudflare emailed {fwd} a verification link; forwarding starts once it's clicked");
        match known {
            Some((_, true)) => {}
            Some((_, false)) => w.record(&format!("verify {fwd}"), "pending", None, Some(pending)),
            None => {
                w.mutate(&format!("add forwarding destination {fwd}"), &["email", "routing", "addresses", "create", &fwd], None)?;
                w.record(&format!("verify {fwd}"), "pending", None, Some(pending));
            }
        }
        if args.dry_run {
            w.record("forward a copy", "planned", None, Some(fwd.clone()));
        } else {
            client.update_settings(&json!({ "forward_to": fwd }))?;
            w.record("forward a copy", "done", None, Some(fwd.clone()));
        }
    }

    let attention: Vec<&Step> = w.steps.iter().filter(|s| matches!(s.status.as_str(), "blocked" | "pending")).collect();
    let mut summary = if args.dry_run {
        format!("Planned {} steps (dry run, nothing changed)", w.steps.iter().filter(|s| s.status == "planned").count())
    } else {
        format!("Cloudmail is running at {url}")
    };
    if !attention.is_empty() {
        summary.push_str(&format!("; {} need attention", plural_steps(attention.len())));
    }
    let details = |steps: &[&Step]| {
        steps
            .iter()
            .map(|s| {
                let mut line = format!("[{:>7}] {}", s.status, s.step);
                if let Some(c) = &s.command {
                    line.push_str(&format!("\n          $ {c}"));
                }
                if let Some(d) = &s.detail {
                    line.push_str(&format!("\n          {d}"));
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    // At a terminal the steps were shown as they ran; repeat only what still needs the person.
    let mut human = if interactive && !args.dry_run { details(&attention) } else { details(&w.steps.iter().collect::<Vec<_>>()) };
    if !human.is_empty() {
        human.push_str("\n\n");
    }
    human.push_str(&summary);
    if !args.dry_run {
        human.push_str("\nOpen Cloudmail from your app launcher, or run `cloudmail inbox`.");
    }
    Ok(Response::new(
        json!({ "dry_run": args.dry_run, "worker": worker_name, "account": account, "url": url, "steps": w.steps, "routes": routes }),
        summary.clone(),
    )
    .human(human)
    .crumbs(vec![
        crumb("status", "cloudmail status", "Check the worker and counts"),
        crumb("mailbox", "cloudmail mailbox add <address> --route", "Add another address"),
        crumb("watch", "cloudmail watch", "Wait for mail to arrive"),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependencies_reinstall_when_package_json_is_newer() {
        let dir = std::env::temp_dir().join(format!("cloudmail-deps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        assert!(!deps_current(&dir), "no install record yet");
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("node_modules/.package-lock.json"), "{}").unwrap();
        assert!(deps_current(&dir));
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("package.json"), "{\"v\":2}").unwrap();
        assert!(!deps_current(&dir), "an upgraded package.json needs a reinstall");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn d1_list_survives_wrangler_warnings_around_the_json() {
        let out = "\u{1b}[33m▲ [WARNING]\u{1b}[0m update available\n[{\"uuid\":\"abc\",\"name\":\"cloudmail\"}]\n▲ [WARNING] trailing note\n";
        assert_eq!(parse_d1_list(out, "cloudmail").as_deref(), Some("abc"));
        assert_eq!(parse_d1_list(out, "other"), None);
    }

    #[test]
    fn packaged_worker_is_copied_somewhere_writable_and_refreshed() {
        let tmp = std::env::temp_dir().join(format!("cloudmail-pkg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let packaged = tmp.join("usr/share/cloudmail/worker");
        std::fs::create_dir_all(packaged.join("src")).unwrap();
        std::fs::write(packaged.join("wrangler.template.jsonc"), "{}").unwrap();
        std::fs::write(packaged.join("src/index.ts"), "v1").unwrap();
        let data = tmp.join("data");

        let dir = resolve_worker_dir_in(None, &tmp.join("no-clone"), &[&packaged], &data).unwrap();
        assert_eq!(dir, data.join("cloudmail/worker"));
        assert_eq!(std::fs::read_to_string(dir.join("src/index.ts")).unwrap(), "v1");

        // A later package version replaces the source but keeps what setup generated.
        std::fs::write(dir.join("wrangler.jsonc"), "generated").unwrap();
        std::fs::write(packaged.join("src/index.ts"), "v2").unwrap();
        let dir = resolve_worker_dir_in(None, &tmp.join("no-clone"), &[&packaged], &data).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("src/index.ts")).unwrap(), "v2");
        assert_eq!(std::fs::read_to_string(dir.join("wrangler.jsonc")).unwrap(), "generated");

        // A clone's worker/ wins over the package; nothing at all is a usage error.
        std::fs::create_dir_all(tmp.join("clone/worker")).unwrap();
        std::fs::write(tmp.join("clone/worker/wrangler.template.jsonc"), "{}").unwrap();
        assert_eq!(resolve_worker_dir_in(None, &tmp.join("clone/worker"), &[&packaged], &data).unwrap(), tmp.join("clone/worker"));
        assert!(resolve_worker_dir_in(None, &tmp.join("no-clone"), &[], &data).is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    const RULES: &str = "\n \u{1b}[33m▲ WARNING\u{1b}[0m open beta\n\nRule: 6b4a6ac1f8d643999a3a6111c0583125\n  Name:     Rule created at 2025\n  Enabled:  true\n  Matchers: to:hi@example.com\n  Actions:  forward:me@elsewhere.com\n  Priority: 0\n\nRule: 06bae9a8c2794e41a8fd99679a84f310\n  Name:     (none)\n  Enabled:  true\n  Matchers: to:hello@example.com\n  Actions:  worker:cloudmail\n  Priority: 0\n\n\nCatch-all rule: disabled, action: drop\n";

    #[test]
    fn parses_rules() {
        let r = parse_rules(RULES);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].matcher, "to:hi@example.com");
        assert_eq!(r[0].action, "forward:me@elsewhere.com");
        assert_eq!(r[1].id, "06bae9a8c2794e41a8fd99679a84f310");
        assert_eq!(r[1].action, "worker:cloudmail");
    }

    #[test]
    fn renders_template() {
        let t = "{ \"name\": \"__WORKER_NAME__\", \"database_name\": \"__DATABASE_NAME__\", \"database_id\": \"__DATABASE_ID__\", \"bucket_name\": \"__BUCKET_NAME__\" }";
        let r = render_template(t, "cloudmail", None, "cloudmail-db", "abc-123", "cloudmail-bucket");
        assert_eq!(r, "{ \"name\": \"cloudmail\", \"database_name\": \"cloudmail-db\", \"database_id\": \"abc-123\", \"bucket_name\": \"cloudmail-bucket\" }");
        assert!(!r.contains("__"));
        let t = "{\n  \"name\": \"__WORKER_NAME__\",\n  \"account_id\": \"__ACCOUNT_ID__\",\n  \"main\": \"src/index.ts\"\n}";
        assert_eq!(render_template(t, "cm", Some("0af9"), "", "", ""), "{\n  \"name\": \"cm\",\n  \"account_id\": \"0af9\",\n  \"main\": \"src/index.ts\"\n}");
        assert_eq!(render_template(t, "cm", None, "", "", ""), "{\n  \"name\": \"cm\",\n  \"main\": \"src/index.ts\"\n}");
    }

    #[test]
    fn parses_deploy_output() {
        let out = "Uploaded cloudmail (4.39 sec)\nDeployed cloudmail triggers (2.15 sec)\n  https://cloudmail.example-sub.workers.dev\nCurrent Version ID: x";
        assert_eq!(parse_deploy_url(out).as_deref(), Some("https://cloudmail.example-sub.workers.dev"));
        assert_eq!(parse_deploy_url("no url here"), None);
    }

    #[test]
    fn reads_jsonc_fields() {
        let j = "{\n  \"name\": \"cloud-mail\",\n  \"d1_databases\": [{ \"binding\": \"DB\", \"database_name\": \"cloud-mail-db\" }],\n  \"r2_buckets\": [{ \"binding\": \"BUCKET\", \"bucket_name\": \"mail\" }]\n}";
        assert_eq!(jsonc_field(j, "database_name").as_deref(), Some("cloud-mail-db"));
        assert_eq!(jsonc_field(j, "bucket_name").as_deref(), Some("mail"));
        assert_eq!(jsonc_field(j, "missing"), None);
    }

    #[test]
    fn parses_worker_name() {
        let j = "{\n  \"$schema\": \"x\",\n  \"name\": \"cloud-mail\",\n  \"main\": \"src/index.ts\",";
        assert_eq!(parse_worker_name(j).as_deref(), Some("cloud-mail"));
    }

    #[test]
    fn parses_accounts_destinations_and_mx() {
        let whoami = "┌──┐\n│ Account Name │ Account ID │\n├──┤\n│ Me's Account │ 0123456789abcdef0123456789abcdef │\n│ Team │ 11111111111111111111111111111111 │\n└──┘\n";
        assert_eq!(parse_accounts(whoami), vec![("Me's Account".into(), "0123456789abcdef0123456789abcdef".into()), ("Team".into(), "11111111111111111111111111111111".into())]);
        let dests = "│ id │ email │ verified │ created │\n│ b51d │ Me@Hey.com │ 2025-12-26T05:50:19Z │ 2025-12-26T04:33:43Z │\n│ 87be │ new@gmail.com │  │ 2026-01-01T00:00:00Z │\n";
        assert_eq!(parse_destinations(dests), vec![("me@hey.com".into(), true), ("new@gmail.com".into(), false)]);
        let mx = json!({"Answer": [{"type": 15, "data": "10 ASPMX.L.Google.com."}, {"type": 5, "data": "x."}, {"type": 15, "data": "20 route2.mx.cloudflare.net."}]});
        assert_eq!(parse_mx_answer(&mx), vec!["aspmx.l.google.com", "route2.mx.cloudflare.net"]);
        assert!(parse_mx_answer(&json!({"Status": 3})).is_empty());
    }

    #[test]
    fn parses_lists() {
        let d1 = "[{\"uuid\":\"u1\",\"name\":\"other\"},{\"uuid\":\"u2\",\"name\":\"cloudmail\"}]";
        assert_eq!(parse_d1_list(d1, "cloudmail").as_deref(), Some("u2"));
        assert_eq!(parse_d1_list(d1, "nope"), None);
        let r2 = "Listing buckets...\nname:           cloud-mail\ncreation_date:  2026\n\nname:           cloudmail\n";
        assert_eq!(parse_r2_buckets(r2), vec!["cloud-mail", "cloudmail"]);
        let table = "┌──┐\n│ zone │ name │ enabled │ tag │\n├──┤\n│ example.com │ example.com │ yes │ c0 │\n├──┤\n│ other.com │ other.com │ no │ c1 │\n";
        assert_eq!(enabled_zones(table), vec!["example.com"]);
    }

    #[test]
    fn parses_mailbox_specs() {
        assert_eq!(parse_mailbox_spec("Hi@Example.com").unwrap(), ("hi@example.com".into(), true));
        assert_eq!(parse_mailbox_spec("support@example.com:direct").unwrap(), ("support@example.com".into(), false));
        assert!(parse_mailbox_spec("nope").is_err());
        assert!(parse_mailbox_spec("a@b.com:weird").is_err());
    }

    #[test]
    fn dry_run_setup_plans_without_running() {
        let dir = std::env::temp_dir().join(format!("cloudmail-setup-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("node_modules/.package-lock.json"), "{}").unwrap();
        std::fs::write(dir.join("wrangler.template.jsonc"), "{ \"name\": \"__WORKER_NAME__\" }").unwrap();
        // A fake wrangler that fails every command proves nothing mutating runs in --dry-run.
        let args = SetupArgs {
            worker_dir: Some(dir.clone()),
            name: "cloudmail".into(),
            mailboxes: vec!["hi@example.com".into()],
            mailbox_flags: vec!["support@example.com:direct".into()],
            yes: true,
            account: None,
            forward_to: Some("me@elsewhere.com".into()),
            force: true,
            dry_run: true,
            wrangler: "false".into(),
        };
        let resp = run(&args, false).unwrap();
        let steps = resp.data["steps"].as_array().unwrap();
        let planned: Vec<&str> = steps.iter().filter(|s| s["status"] == "planned").filter_map(|s| s["command"].as_str()).collect();
        assert!(planned.contains(&"false d1 create cloudmail"));
        assert!(planned.contains(&"false deploy"));
        assert!(planned.iter().any(|c| c.contains("routing rules create example.com") && c.contains("--match-value hi@example.com")));
        assert!(planned.iter().any(|c| c.contains("email sending enable example.com")));
        assert!(planned.iter().any(|c| c.contains("email routing enable example.com")));
        assert!(planned.contains(&"false email routing addresses create me@elsewhere.com"));
        assert!(planned.contains(&"false login"));
        assert!(!dir.join("wrangler.jsonc").exists());
        assert_eq!(resp.data["dry_run"], true);
        std::fs::remove_dir_all(dir).ok();
    }
}
