//! `cloudmail setup` and `mailbox add --route`: drive the Cloudflare CLI (cf) to deploy a worker and
//! connect addresses.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use cloudmail_api::{Client, Config, MailboxUpdate, config};

use crate::cli::{RouteArgs, SetupArgs};
use crate::output::{self, CliError, CliResult, Response, crumb, exit, prompt};

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

/// The first JSON value in a command's output (cf may print notices around it, and they can
/// contain brackets too, as in "▲ [WARNING] …").
pub fn first_json(output: &str) -> Option<Value> {
    output.match_indices(['[', '{']).find_map(|(i, _)| serde_json::Deserializer::from_str(&output[i..]).into_iter::<Value>().next()?.ok())
}

/// What `cloudmail setup` records about an install in `install.json`, which cloudflare.config.ts reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Install {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    pub database: Database,
    pub bucket: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Database {
    pub name: String,
    pub id: String,
}

pub const INSTALL_FILE: &str = "install.json";
/// The config setup generated before it used cf (worker name, account, database and bucket).
pub const LEGACY_FILE: &str = "wrangler.jsonc";

pub fn render_install(install: &Install) -> String {
    let mut text = serde_json::to_string_pretty(install).expect("an install serializes");
    text.push('\n');
    text
}

/// Names an earlier setup recorded: `install.json`, else the wrangler.jsonc of a setup before cf.
#[derive(Debug, Default, PartialEq)]
pub struct Recorded {
    pub file: Option<&'static str>,
    pub text: Option<String>,
    pub worker: Option<String>,
    pub database: Option<String>,
    pub bucket: Option<String>,
    pub account: Option<String>,
}

pub fn recorded(dir: &Path) -> Recorded {
    if let Ok(text) = std::fs::read_to_string(dir.join(INSTALL_FILE))
        && let Ok(i) = serde_json::from_str::<Install>(&text)
    {
        return Recorded {
            file: Some(INSTALL_FILE),
            worker: Some(i.name),
            database: Some(i.database.name),
            bucket: Some(i.bucket),
            account: i.account_id,
            text: Some(text),
        };
    }
    match std::fs::read_to_string(dir.join(LEGACY_FILE)) {
        Ok(text) => Recorded {
            file: Some(LEGACY_FILE),
            worker: jsonc_field(&text, "name"),
            database: jsonc_field(&text, "database_name"),
            bucket: jsonc_field(&text, "bucket_name"),
            account: jsonc_field(&text, "account_id"),
            text: Some(text),
        },
        Err(_) => Recorded::default(),
    }
}

/// The workers.dev URL printed by `cf deploy`.
pub fn parse_deploy_url(output: &str) -> Option<String> {
    strip_ansi(output)
        .split_whitespace()
        .find(|w| w.starts_with("https://") && w.contains(".workers.dev") && !w.contains('<'))
        .map(|w| w.trim_end_matches(['.', ',', ')']).to_string())
}

/// The worker name in a workers.dev URL (`https://<worker>.<subdomain>.workers.dev`).
pub fn worker_name_from_url(url: &str) -> Option<String> {
    let host = url.strip_prefix("https://")?.split(['/', ':']).next()?;
    let labels: Vec<&str> = host.strip_suffix(".workers.dev")?.split('.').collect();
    match labels.as_slice() {
        [worker, _subdomain] if !worker.is_empty() => Some(worker.to_string()),
        _ => None,
    }
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

/// The id of the D1 database called `name` in `cf d1 list`.
pub fn parse_d1_list(output: &str, name: &str) -> Option<String> {
    let list = first_json(output)?;
    list.as_array()?.iter().find(|d| d["name"] == name).and_then(|d| d["uuid"].as_str().map(str::to_string))
}

/// Bucket names in `cf r2 buckets list`.
pub fn parse_r2_buckets(output: &str) -> Vec<String> {
    let list = first_json(output).unwrap_or_default();
    list["buckets"].as_array().into_iter().flatten().filter_map(|b| b["name"].as_str().map(str::to_string)).collect()
}

/// Whether `cf email-routing settings get` says Email Routing is on.
pub fn routing_enabled(output: &str) -> bool {
    first_json(output).is_some_and(|v| v["enabled"] == true)
}

/// Whether `cf email-sending subdomains list` has sending on for `domain` itself.
pub fn sending_enabled(output: &str, domain: &str) -> bool {
    first_json(output)
        .and_then(|v| v.as_array().cloned())
        .into_iter()
        .flatten()
        .any(|s| s["enabled"] == true && s["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(domain)))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rule {
    pub id: String,
    /// `to:<address>`
    pub matcher: String,
    /// `worker:<name>`, `forward:<address>`, `drop:`…
    pub action: String,
}

/// Rules in `cf email-routing rules list-account`, flattened to their first matcher and action.
pub fn parse_rules(output: &str) -> Vec<Rule> {
    let text = |v: &Value| match v {
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(","),
        v => v.as_str().unwrap_or_default().to_string(),
    };
    first_json(output)
        .and_then(|v| v.as_array().cloned())
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let (m, a) = (&r["matchers"][0], &r["actions"][0]);
            Some(Rule {
                id: r["id"].as_str()?.to_string(),
                matcher: format!("{}:{}", text(&m["field"]), text(&m["value"])),
                action: format!("{}:{}", text(&a["type"]), text(&a["value"])),
            })
        })
        .collect()
}

/// (name, id) of each account in `cf auth whoami`, or in `cf accounts list`'s plain array.
pub fn parse_accounts(output: &Value) -> Vec<(String, String)> {
    output
        .as_array()
        .or_else(|| output["accounts"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|a| Some((a["name"].as_str()?.to_string(), a["id"].as_str()?.to_string())))
        .collect()
}

/// Destination addresses in `cf email-routing addresses list`, with whether each is verified.
pub fn parse_destinations(output: &str) -> Vec<(String, bool)> {
    first_json(output)
        .and_then(|v| v.as_array().cloned())
        .into_iter()
        .flatten()
        .filter_map(|d| Some((d["email"].as_str()?.to_ascii_lowercase(), d["verified"].as_str().is_some_and(|v| !v.is_empty()))))
        .collect()
}

/// A domain's public MX hosts (DNS over HTTPS); None when the lookup failed three times.
fn mx_hosts(domain: &str) -> Option<Vec<String>> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(5))).build().into();
    let lookup = || -> Option<Value> {
        agent
            .get("https://cloudflare-dns.com/dns-query")
            .query("name", domain)
            .query("type", "MX")
            .header("accept", "application/dns-json")
            .call()
            .ok()?
            .body_mut()
            .read_json()
            .ok()
    };
    // One setup looks up every domain, and a single slow answer shouldn't read as mail elsewhere.
    (0..3).find_map(|_| lookup()).map(|body| parse_mx_answer(&body))
}

/// MX hosts in priority order (most preferred first).
pub fn parse_mx_answer(body: &Value) -> Vec<String> {
    let mut mx: Vec<(u16, String)> = body["Answer"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["type"] == 15)
        .filter_map(|a| {
            let (priority, host) = a["data"].as_str()?.split_once(' ')?;
            Some((priority.parse().ok()?, host.trim().trim_end_matches('.').to_ascii_lowercase()))
        })
        .collect();
    mx.sort();
    mx.into_iter().map(|(_, host)| host).collect()
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

// ---------- running cf ----------

#[derive(Debug, Serialize)]
pub struct Step {
    pub step: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A command line for display, quoting arguments the shell would split or expand.
fn shell_line(program: &[String], args: &[&str]) -> String {
    let mut parts = program.to_vec();
    parts.extend(args.iter().map(|a| if a.contains([' ', '"', '[', '{', '*', '$']) { format!("'{a}'") } else { a.to_string() }));
    parts.join(" ")
}

pub struct Cf {
    program: Vec<String>,
    dir: PathBuf,
    account: Option<String>,
    pub dry_run: bool,
    /// Someone is at a terminal: questions may be asked and steps are shown as they happen.
    pub interactive: bool,
    pub steps: Vec<Step>,
}

impl Cf {
    pub fn new(program: &str, dir: PathBuf, dry_run: bool, interactive: bool) -> Self {
        let program = program.split_whitespace().map(str::to_string).collect();
        Self { program, dir, account: None, dry_run, interactive, steps: Vec::new() }
    }

    /// Runs every later cf command against this Cloudflare account.
    pub fn use_account(&mut self, id: &str) {
        self.account = Some(id.into());
    }

    /// Asks a yes/no question at the terminal; None when nobody is there to answer.
    pub fn ask(&self, question: &str) -> Option<bool> {
        if !self.interactive || self.dry_run {
            return None;
        }
        output::ask_yes(question).ok()
    }

    fn render(&self, args: &[&str]) -> String {
        shell_line(&self.program, args)
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

    /// Does a step that changes something, or records it as planned during --dry-run.
    pub fn perform<T: Default>(
        &mut self,
        step: &str,
        command: Option<String>,
        detail: Option<String>,
        action: impl FnOnce(&Self) -> CliResult<T>,
    ) -> CliResult<T> {
        if self.dry_run {
            self.record(step, "planned", command, detail);
            return Ok(T::default());
        }
        if self.interactive {
            eprint!("[    ...] {step}\r");
            let _ = std::io::stderr().flush();
        }
        let out = action(self)?;
        self.record(step, "done", command, detail);
        Ok(out)
    }

    /// Runs a read-only cf command (also during --dry-run, where its result only informs the plan).
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
        self.perform(step, Some(self.render(args)), None, |w| w.exec(&w.program, args, stdin))
    }

    pub fn run_program(&mut self, step: &str, program: &[&str]) -> CliResult<()> {
        let (bin, args) = program.split_first().expect("a program to run");
        self.perform(step, Some(program.join(" ")), None, |w| w.exec(&[bin.to_string()], args, None).map(drop))
    }

    fn command(&self, program: &[String], args: &[&str]) -> CliResult<Command> {
        let (bin, pre) = program.split_first().ok_or_else(|| CliError::usage("empty --cf command"))?;
        let mut cmd = Command::new(bin);
        cmd.args(pre).args(args).current_dir(&self.dir);
        if let Some(id) = &self.account {
            cmd.env("CLOUDFLARE_ACCOUNT_ID", id);
        }
        Ok(cmd)
    }

    /// Runs cf attached to the terminal (for `cf auth login`, which opens a browser).
    pub fn attached(&self, args: &[&str]) -> CliResult<()> {
        let status = self
            .command(&self.program, args)?
            .status()
            .map_err(|e| CliError::generic(format!("could not run `{}`: {e}", self.program[0])).hint("install Node.js (npm)"))?;
        if status.success() { Ok(()) } else { Err(CliError::generic(format!("`{}` failed", self.render(args)))) }
    }

    fn exec(&self, program: &[String], args: &[&str], stdin: Option<&str>) -> CliResult<String> {
        let mut child = self
            .command(program, args)?
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| CliError::generic(format!("could not run `{}`: {e}", program[0])).hint("install Node.js (npx) or pass --cf \"bunx cf\""))?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(input.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        if !out.status.success() {
            let clean = strip_ansi(&text);
            let tail: Vec<&str> = clean.lines().filter(|l| !l.trim().is_empty()).collect();
            let tail = tail[tail.len().saturating_sub(6)..].join("\n");
            return Err(CliError::generic(format!("`{}` failed:\n{tail}", shell_line(program, args))));
        }
        Ok(text)
    }
}

/// Where packages install the worker source (read-only).
const PACKAGED_WORKER_DIRS: &[&str] = &["/usr/share/cloudmail/worker", "/usr/local/share/cloudmail/worker"];

fn is_worker_dir(dir: &Path) -> bool {
    dir.join("cloudflare.config.ts").exists()
}

/// `--worker-dir`, else `./worker` in a clone, else a writable copy of the packaged worker in
/// `~/.local/share/cloudmail/worker` (refreshed on each run, keeping the generated install.json and node_modules).
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

#[derive(Serialize)]
pub struct RouteOutcome {
    pub address: String,
    pub status: String,
    pub detail: String,
}

/// Asks for the go-ahead on a change that moves existing mail: `--yes`, else the person at the terminal.
fn consent(w: &Cf, yes: bool, question: &str) -> bool {
    yes || w.ask(question).unwrap_or(false)
}

fn blocked(w: &mut Cf, address: &str, detail: String) -> RouteOutcome {
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
    w: &mut Cf,
    address: &str,
    worker: &str,
    yes: bool,
    sending_zones: &mut Vec<String>,
    routing_zones: &mut Vec<String>,
) -> CliResult<RouteOutcome> {
    let domain = domain_of(address).ok_or_else(|| CliError::usage(format!("not an email address: {address}")))?.to_string();

    let routing_on = |w: &Cf| w.query(&["email-routing", "settings", "get", "-z", &domain]).is_ok_and(|o| routing_enabled(&o));
    if !routing_zones.contains(&domain) && !routing_on(w) {
        // Enabling Email Routing replaces the domain's MX records: free to do when nothing receives
        // mail there yet, a question when something else does.
        let mx = if yes { None } else { mx_hosts(&domain) };
        let elsewhere: Vec<String> = mx.iter().flatten().filter(|h| !h.ends_with(".mx.cloudflare.net")).cloned().collect();
        let safe = mx.is_some() && elsewhere.is_empty();
        if !safe {
            // A failed lookup says so: "receives mail at somewhere" reads as mail elsewhere.
            let current = if mx.is_none() {
                format!("{domain} may receive mail elsewhere (its MX lookup failed; run setup again to retry)")
            } else {
                format!("{domain} receives mail at {}", elsewhere.join(", "))
            };
            let question = format!("{current}. Move all of {domain}'s mail to Cloudflare (replaces its MX records)?");
            if !consent(w, yes, &question) {
                return Ok(blocked(w, address, format!("{current}; moving it to Cloudflare replaces its MX records")));
            }
        }
        w.mutate(&format!("enable Email Routing for {domain}"), &["email-routing", "enable", "-z", &domain], None)?;
    }
    routing_zones.push(domain.clone());

    let sending_on = |w: &Cf| w.query(&["email-sending", "subdomains", "list", "-z", &domain]).is_ok_and(|o| sending_enabled(&o, &domain));
    if sending_zones.contains(&domain) || sending_on(w) {
        w.record(&format!("sending for {domain}"), "exists", None, None);
    } else {
        w.mutate(&format!("enable sending for {domain}"), &["email-sending", "subdomains", "create", "-z", &domain, "--name", &domain], None)?;
    }
    sending_zones.push(domain.clone());

    let rules = parse_rules(&w.probe(&["email-routing", "rules", "list-account", "-z", &domain, "--per-page", "50"])?);
    let target = format!("worker:{worker}");
    let matcher = format!("to:{address}");
    let step = format!("route {address}");
    let outcome = |status: &str, detail: String| RouteOutcome { address: address.into(), status: status.into(), detail };
    let (verb, id, status, detail) = match rules.iter().find(|r| r.matcher.eq_ignore_ascii_case(&matcher)) {
        Some(r) if r.action == target => {
            w.record(&step, "exists", None, Some(target.clone()));
            return Ok(outcome("exists", target));
        }
        Some(r) if !consent(w, yes, &format!("{address} is routed to {}. Send it to Cloudmail instead?", r.action)) => {
            return Ok(blocked(w, address, format!("an existing rule sends {address} to {}", r.action)));
        }
        Some(r) => ("update", Some(r.id.as_str()), "updated", format!("was {}", r.action)),
        None => ("create", None, "created", target),
    };
    let matchers = json!([{ "type": "literal", "field": "to", "value": address }]).to_string();
    let actions = json!([{ "type": "worker", "value": [worker] }]).to_string();
    let mut args = vec!["email-routing", "rules", verb];
    args.extend(id);
    args.extend(["-z", &domain, "--matchers", &matchers, "--actions", &actions, "--enabled"]);
    w.mutate(&step, &args, None)?;
    Ok(outcome(if w.dry_run { "planned" } else { status }, detail))
}

pub fn route_for_mailbox(address: &str, args: &RouteArgs, interactive: bool) -> CliResult<Value> {
    let dir = resolve_worker_dir(args.worker_dir.as_deref())?;
    let recorded = recorded(&dir);
    // A worker set up from a clone keeps its install.json there, not in the packaged copy, so
    // fall back to the worker this CLI is configured to talk to.
    let worker = match &args.worker_name {
        Some(n) => n.clone(),
        None => recorded
            .worker
            .clone()
            .or_else(|| config::load().ok().and_then(|c| worker_name_from_url(&c.api_url)))
            .ok_or_else(|| CliError::usage("couldn't tell which worker to route to").hint("run `cloudmail setup` first"))?,
    };
    let mut w = Cf::new(&args.cf, dir, false, interactive);
    if let Some(account) = &recorded.account {
        w.use_account(account);
    }
    let outcome = route_address(&mut w, address, &worker, args.yes, &mut Vec::new(), &mut Vec::new())?;
    Ok(json!({ "status": outcome.status, "detail": outcome.detail, "worker": worker, "steps": w.steps }))
}

// ---------- setup ----------

/// Makes sure cf can reach Cloudflare, logging in through the browser when someone is there to do
/// it, and returns `cf auth whoami`'s answer.
fn login(w: &mut Cf, cf: &str) -> CliResult<Value> {
    let whoami = |w: &Cf| w.query(&["auth", "whoami"]).ok().and_then(|o| first_json(&o)).filter(|v| v["authenticated"] == true && v["tokenValid"] != false);
    if let Some(out) = whoami(w) {
        w.record("Cloudflare login", "ok", None, None);
        return Ok(out);
    }
    if w.dry_run {
        w.record("Cloudflare login", "planned", Some(format!("{cf} auth login")), Some("opens your browser".into()));
        return Ok(Value::Null);
    }
    if w.interactive {
        eprintln!("Log in to Cloudflare in the browser window that opens (create a free account there if you need one).");
        w.attached(&["auth", "login"])?;
        if let Some(out) = whoami(w) {
            w.record("Cloudflare login", "done", None, None);
            return Ok(out);
        }
    }
    Err(CliError::new("not_logged_in", exit::AUTH, "not logged in to Cloudflare").hint(format!(
        "run `{cf} auth login`, or set CLOUDFLARE_API_TOKEN to a token with the permissions listed in the README (https://github.com/ferdousbhai/cloud-mail#get-started)"
    )))
}

/// The Cloudflare account to use: --account / CLOUDFLARE_ACCOUNT_ID, the one setup used before,
/// the only one the login can see, or the person's pick.
fn choose_account(w: &Cf, flag: Option<&str>, previous: Option<String>, whoami: &Value) -> CliResult<Option<String>> {
    if let Some(id) = flag.map(str::to_string).or(previous) {
        return Ok(Some(id));
    }
    let mut accounts = parse_accounts(whoami);
    // A browser (OAuth) login leaves whoami's account list empty; the accounts API still answers.
    if accounts.is_empty() {
        accounts = w.query(&["accounts", "list"]).ok().and_then(|o| first_json(&o)).map(|v| parse_accounts(&v)).unwrap_or_default();
    }
    match accounts.len() {
        // A token that can't list accounts leaves cf unable to pick one either.
        0 if !w.dry_run => Err(CliError::usage("couldn't tell which Cloudflare account to use")
            .hint("pass --account <ID> (the Account ID on your Cloudflare dashboard's home page), or set CLOUDFLARE_ACCOUNT_ID")),
        0 => Ok(None),
        1 => Ok(Some(accounts[0].1.clone())),
        _ if w.interactive && !w.dry_run => {
            eprintln!("Your Cloudflare login can use several accounts:");
            for (i, (name, id)) in accounts.iter().enumerate() {
                eprintln!("  {}. {name} ({id})", i + 1);
            }
            let pick = prompt(&format!("Which one holds your domains? [1-{}]", accounts.len()))?;
            let i = pick.parse::<usize>().ok().filter(|i| (1..=accounts.len()).contains(i)).ok_or_else(|| CliError::cancelled("no account chosen"))?;
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
            return Err(CliError::cancelled("cancelled"));
        }
        specs.push(addr);
    }
    let mailboxes = specs.iter().map(|m| parse_mailbox_spec(m)).collect::<CliResult<Vec<_>>>()?;

    let dir = resolve_worker_dir(args.worker_dir.as_deref())?;
    let install_path = dir.join(INSTALL_FILE);
    // What an earlier setup recorded wins unless --force: reuse its worker, database, bucket and account.
    let previous = recorded(&dir);
    let keep = previous.file.is_some() && !args.force;
    let previous = if keep { previous } else { Recorded::default() };
    let worker_name = previous.worker.clone().unwrap_or_else(|| args.name.clone());
    let db_name = previous.database.clone().unwrap_or_else(|| args.name.clone());
    let bucket_name = previous.bucket.clone().unwrap_or_else(|| args.name.clone());
    let mut w = Cf::new(&args.cf, dir.clone(), args.dry_run, interactive);

    // 1. dependencies
    if deps_current(&dir) {
        w.record("install worker dependencies", "exists", None, None);
    } else {
        w.run_program("install worker dependencies", &["npm", "install", "--no-audit", "--no-fund"])?;
    }

    // 2. login and account
    let whoami = login(&mut w, &args.cf)?;
    let account = choose_account(&w, args.account.as_deref(), previous.account.clone(), &whoami)?;
    if let Some(id) = &account {
        w.use_account(id);
        w.record("Cloudflare account", "ok", None, Some(id.clone()));
    }

    // 3. D1 + R2
    let find_db = |w: &Cf| w.query(&["d1", "list", "--name", &db_name]).map(|o| parse_d1_list(&o, &db_name));
    let database_id = match find_db(&w).ok().flatten() {
        Some(id) => {
            w.record(&format!("D1 database {db_name}"), "exists", None, Some(id.clone()));
            id
        }
        None => {
            w.mutate(&format!("create D1 database {db_name}"), &["d1", "create", "--name", &db_name], None)?;
            if args.dry_run {
                "<new-database-id>".into()
            } else {
                find_db(&w)?.ok_or_else(|| CliError::generic("created the D1 database but could not find its id"))?
            }
        }
    };
    let buckets = w.query(&["r2", "buckets", "list", "--name-contains", &bucket_name]).map(|o| parse_r2_buckets(&o)).unwrap_or_default();
    if buckets.contains(&bucket_name) {
        w.record(&format!("R2 bucket {bucket_name}"), "exists", None, None);
    } else {
        w.mutate(&format!("create R2 bucket {bucket_name}"), &["r2", "buckets", "create", "--name", &bucket_name], None)?;
    }

    // 4. install.json, which cloudflare.config.ts reads
    let install = Install {
        name: worker_name.clone(),
        account_id: account.clone(),
        database: Database { name: db_name.clone(), id: database_id.clone() },
        bucket: bucket_name.clone(),
    };
    let rendered = render_install(&install);
    let write_install = |_: &Cf| Ok(std::fs::write(&install_path, &rendered)?);
    match previous.file {
        Some(INSTALL_FILE) => {
            let status = if previous.text.as_deref() == Some(rendered.as_str()) { "exists" } else { "kept" };
            w.record("write install.json", status, None, Some(format!("using the existing file (worker \"{worker_name}\"); --force regenerates it")));
        }
        // A setup from before cf left wrangler.jsonc; its names carry over.
        Some(legacy) => w.perform("write install.json", None, Some(format!("from {legacy} (worker \"{worker_name}\")")), write_install)?,
        None => w.perform("write install.json", None, Some(install_path.display().to_string()), write_install)?,
    }

    // 5. schema + deploy
    w.mutate("apply database migrations", &["d1", "migrations", "apply", &database_id], None)?;
    // A new Cloudflare account has no workers.dev subdomain yet, and cf asks for one. With someone
    // at the terminal, let cf ask (it then deploys), and deploy again to read the URL.
    let deploy_out = match w.mutate("deploy worker", &["deploy"], None) {
        Err(e) if e.message.contains("workers.dev subdomain") && w.interactive => {
            eprintln!("\nYour Cloudflare account needs a workers.dev subdomain: the address your mail service runs at.");
            w.attached(&["deploy"])?;
            w.mutate("deploy worker", &["deploy"], None)?
        }
        Err(e) if e.message.contains("workers.dev subdomain") => {
            return Err(e.hint("pick a workers.dev subdomain at https://dash.cloudflare.com/?to=/:account/workers/onboarding, then run the same command again"));
        }
        other => other?,
    };
    let url = if args.dry_run {
        format!("https://{worker_name}.<your-subdomain>.workers.dev")
    } else {
        parse_deploy_url(&deploy_out).ok_or_else(|| CliError::generic("deployed, but could not find the workers.dev URL in cf's output"))?
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
    // The token is in the keyring (or, from an older version, still in the file).
    let reused = match existing_cfg.as_ref().filter(|c| !args.force && c.api_url.as_deref().is_some_and(same_worker)) {
        Some(c) => match c.api_token.clone() {
            Some(t) => Some(t),
            None => config::keyring_token()?,
        },
        None => None,
    };
    let token = if let Some(token) = reused {
        w.record("API token", "exists", None, Some("keyring".into()));
        token
    } else if !first_time && !args.force {
        if !args.dry_run {
            return Err(CliError::generic(format!("{} already points at another worker", cfg_path.display()))
                .hint("re-run with --force to rotate the token and overwrite the config"));
        }
        w.record("API token", "blocked", None, Some(format!("{} points at another worker; setup would stop here without --force", cfg_path.display())));
        String::new()
    } else {
        let token = if args.dry_run { "<generated>".to_string() } else { random_token()? };
        let secret = json!({ "name": "API_TOKEN", "type": "secret_text", "text": token }).to_string();
        w.mutate(
            "set API_TOKEN secret",
            &["workers", "secrets", "update", "API_TOKEN", "--worker", &worker_name, "--body", "@/dev/stdin"],
            Some(&secret),
        )?;
        let file = config::FileConfig {
            api_url: Some(url.clone()),
            api_token: None,
            poll_seconds: Some(config::DEFAULT_POLL_SECONDS),
            // Linked accounts survive a token rotation.
            accounts: existing_cfg.as_ref().map(|c| c.accounts.clone()).unwrap_or_default(),
        };
        w.perform("keep the API token in the keyring", None, Some("Secret Service".into()), |_| {
            cloudmail_api::keyring::set(cloudmail_api::keyring::API_TOKEN, "Cloudmail API token", &token)?;
            Ok(())
        })?;
        w.perform("write config", None, Some(cfg_path.display().to_string()), |_| {
            config::save(&file)?;
            Ok(())
        })?;
        token
    };

    // 7. a new workers.dev hostname and secret take a few seconds to go live
    let client = Client::new(&Config { api_url: url.clone(), api_token: token, poll_seconds: config::DEFAULT_POLL_SECONDS, accounts: Default::default() });
    w.perform("wait for the worker", None, Some(url.clone()), |_| wait_until_live(&client))?;

    // 8. mailboxes and routes
    let mut routes = Vec::new();
    let (mut sending, mut routing) = (Vec::new(), Vec::new());
    for (addr, screen) in &mailboxes {
        let kind = if *screen { "screened" } else { "direct" };
        w.perform(&format!("mailbox {addr}"), None, Some(kind.into()), |_| {
            client.put_mailbox(addr, &MailboxUpdate { screen: Some(*screen), ..Default::default() })?;
            Ok(())
        })?;
        routes.push(route_address(&mut w, addr, &worker_name, args.yes, &mut sending, &mut routing)?);
    }

    // 9. forwarding: Email Routing only forwards to verified destinations, and Cloudflare verifies
    // one by emailing it a link
    if let Some(fwd) = args.forward_to.as_deref().map(str::to_ascii_lowercase).filter(|f| !f.is_empty()) {
        let known = parse_destinations(&w.probe(&["email-routing", "addresses", "list", "--per-page", "50"])?).into_iter().find(|(a, _)| *a == fwd);
        let pending = format!("Cloudflare emailed {fwd} a verification link; forwarding starts once it's clicked");
        match known {
            Some((_, true)) => {}
            Some((_, false)) => w.record(&format!("verify {fwd}"), "pending", None, Some(pending)),
            None => {
                w.mutate(&format!("add forwarding destination {fwd}"), &["email-routing", "addresses", "create", "--email", &fwd], None)?;
                w.record(&format!("verify {fwd}"), "pending", None, Some(pending));
            }
        }
        w.perform("forward a copy", None, Some(fwd.clone()), |_| {
            client.update_settings(&json!({ "forward_to": fwd }))?;
            Ok(())
        })?;
    }

    let needs_attention = |s: &&Step| matches!(s.status.as_str(), "blocked" | "pending");
    let mut summary = if args.dry_run {
        format!("Planned {} steps (dry run, nothing changed)", w.steps.iter().filter(|s| s.status == "planned").count())
    } else {
        format!("Cloudmail is running at {url}")
    };
    match w.steps.iter().filter(needs_attention).count() {
        0 => {}
        1 => summary.push_str("; 1 step needs attention"),
        n => summary.push_str(&format!("; {n} steps need attention")),
    }
    // At a terminal the steps were shown as they ran; repeat only what still needs the person.
    let show_all = !interactive || args.dry_run;
    let mut human = String::new();
    for s in w.steps.iter().filter(|s| show_all || needs_attention(s)) {
        human.push_str(&format!("[{:>7}] {}\n", s.status, s.step));
        if let Some(c) = &s.command {
            human.push_str(&format!("          $ {c}\n"));
        }
        if let Some(d) = &s.detail {
            human.push_str(&format!("          {d}\n"));
        }
    }
    if !human.is_empty() {
        human.push('\n');
    }
    human.push_str(&summary);
    if !args.dry_run {
        human.push_str("\nOpen Cloudmail from your app launcher, or run `cloudmail inbox`.");
    }
    Ok(Response::new(
        json!({ "dry_run": args.dry_run, "worker": worker_name, "account": account, "url": url, "steps": w.steps, "routes": routes }),
        summary,
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
    fn json_survives_notices_around_it() {
        let out = "\u{1b}[33m▲ [WARNING]\u{1b}[0m update available\n[{\"uuid\":\"abc\",\"name\":\"cloudmail\"}]\n▲ [WARNING] trailing note\n";
        assert_eq!(parse_d1_list(out, "cloudmail").as_deref(), Some("abc"));
        assert_eq!(parse_d1_list(out, "other"), None);
        assert_eq!(first_json("no json [here]"), None);
    }

    #[test]
    fn packaged_worker_is_copied_somewhere_writable_and_refreshed() {
        let tmp = std::env::temp_dir().join(format!("cloudmail-pkg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let packaged = tmp.join("usr/share/cloudmail/worker");
        std::fs::create_dir_all(packaged.join("src")).unwrap();
        std::fs::write(packaged.join("cloudflare.config.ts"), "export default {}").unwrap();
        std::fs::write(packaged.join("src/index.ts"), "v1").unwrap();
        let data = tmp.join("data");

        let dir = resolve_worker_dir_in(None, &tmp.join("no-clone"), &[&packaged], &data).unwrap();
        assert_eq!(dir, data.join("cloudmail/worker"));
        assert_eq!(std::fs::read_to_string(dir.join("src/index.ts")).unwrap(), "v1");

        // A later package version replaces the source but keeps what setup generated.
        std::fs::write(dir.join(INSTALL_FILE), "generated").unwrap();
        std::fs::write(packaged.join("src/index.ts"), "v2").unwrap();
        let dir = resolve_worker_dir_in(None, &tmp.join("no-clone"), &[&packaged], &data).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("src/index.ts")).unwrap(), "v2");
        assert_eq!(std::fs::read_to_string(dir.join(INSTALL_FILE)).unwrap(), "generated");

        // A clone's worker/ wins over the package; nothing at all is a usage error.
        std::fs::create_dir_all(tmp.join("clone/worker")).unwrap();
        std::fs::write(tmp.join("clone/worker/cloudflare.config.ts"), "export default {}").unwrap();
        assert_eq!(resolve_worker_dir_in(None, &tmp.join("clone/worker"), &[&packaged], &data).unwrap(), tmp.join("clone/worker"));
        assert!(resolve_worker_dir_in(None, &tmp.join("no-clone"), &[], &data).is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    const RULES: &str = r#"[
      {"id": "6b4a6ac1f8d643999a3a6111c0583125", "name": "", "matchers": [{"type": "literal", "field": "to", "value": "hi@example.com"}],
       "actions": [{"type": "forward", "value": ["me@elsewhere.com"]}], "enabled": true, "priority": 0},
      {"id": "06bae9a8c2794e41a8fd99679a84f310", "name": "", "matchers": [{"type": "literal", "field": "to", "value": "hello@example.com"}],
       "actions": [{"type": "worker", "value": ["cloudmail"]}], "enabled": true, "priority": 0}
    ]"#;

    #[test]
    fn parses_rules() {
        let r = parse_rules(RULES);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].matcher, "to:hi@example.com");
        assert_eq!(r[0].action, "forward:me@elsewhere.com");
        assert_eq!(r[1].id, "06bae9a8c2794e41a8fd99679a84f310");
        assert_eq!(r[1].action, "worker:cloudmail");
        assert!(parse_rules("").is_empty());
    }

    #[test]
    fn reads_worker_name_from_workers_dev_url() {
        assert_eq!(worker_name_from_url("https://cloud-mail.ferdousbd.workers.dev").as_deref(), Some("cloud-mail"));
        assert_eq!(worker_name_from_url("https://cm.sub.workers.dev/api").as_deref(), Some("cm"));
        assert_eq!(worker_name_from_url("https://mail.example.com"), None);
        assert_eq!(worker_name_from_url("https://sub.workers.dev"), None);
        assert_eq!(worker_name_from_url("http://cm.sub.workers.dev"), None);
    }

    #[test]
    fn renders_install() {
        let mut install = Install {
            name: "cloudmail".into(),
            account_id: None,
            database: Database { name: "cloudmail-db".into(), id: "abc-123".into() },
            bucket: "cloudmail-bucket".into(),
        };
        let text = render_install(&install);
        assert_eq!(text, "{\n  \"name\": \"cloudmail\",\n  \"database\": {\n    \"name\": \"cloudmail-db\",\n    \"id\": \"abc-123\"\n  },\n  \"bucket\": \"cloudmail-bucket\"\n}\n");
        install.account_id = Some("0af9".into());
        let text = render_install(&install);
        assert!(text.contains("\"accountId\": \"0af9\""));
        assert_eq!(serde_json::from_str::<Install>(&text).unwrap(), install);
    }

    #[test]
    fn reads_what_an_earlier_setup_recorded() {
        let dir = std::env::temp_dir().join(format!("cloudmail-recorded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(recorded(&dir), Recorded::default());

        // A wrangler.jsonc from a setup before cf.
        let legacy = "{\n  \"name\": \"cloud-mail\",\n  \"account_id\": \"0af9\",\n  \"d1_databases\": [{ \"binding\": \"DB\", \"database_name\": \"cloud-mail-db\", \"database_id\": \"x\" }],\n  \"r2_buckets\": [{ \"binding\": \"BUCKET\", \"bucket_name\": \"mail\" }]\n  // Secret: API_TOKEN\n}";
        std::fs::write(dir.join(LEGACY_FILE), legacy).unwrap();
        let r = recorded(&dir);
        assert_eq!(r.file, Some(LEGACY_FILE));
        assert_eq!((r.worker.as_deref(), r.database.as_deref(), r.bucket.as_deref(), r.account.as_deref()), (Some("cloud-mail"), Some("cloud-mail-db"), Some("mail"), Some("0af9")));

        // install.json wins over it.
        let install = Install { name: "cm".into(), account_id: None, database: Database { name: "cm-db".into(), id: "u".into() }, bucket: "cm-r2".into() };
        std::fs::write(dir.join(INSTALL_FILE), render_install(&install)).unwrap();
        let r = recorded(&dir);
        assert_eq!(r.file, Some(INSTALL_FILE));
        assert_eq!((r.worker.as_deref(), r.database.as_deref(), r.bucket.as_deref(), r.account), (Some("cm"), Some("cm-db"), Some("cm-r2"), None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_deploy_output() {
        let out = "│  Uploaded cloudmail (4.39 sec)\n│  Deployed cloudmail triggers (2.15 sec)\n│    https://cloudmail.example-sub.workers.dev\n│  Current Version ID: x";
        assert_eq!(parse_deploy_url(out).as_deref(), Some("https://cloudmail.example-sub.workers.dev"));
        assert_eq!(parse_deploy_url("It will be accessible at https://<subdomain>.workers.dev"), None);
        assert_eq!(parse_deploy_url("no url here"), None);
    }

    #[test]
    fn reads_jsonc_fields() {
        let j = "{\n  \"name\": \"cloud-mail\",\n  \"d1_databases\": [{ \"binding\": \"DB\", \"database_name\": \"cloud-mail-db\" }],\n  \"r2_buckets\": [{ \"binding\": \"BUCKET\", \"bucket_name\": \"mail\" }]\n}";
        assert_eq!(jsonc_field(j, "database_name").as_deref(), Some("cloud-mail-db"));
        assert_eq!(jsonc_field(j, "bucket_name").as_deref(), Some("mail"));
        assert_eq!(jsonc_field(j, "missing"), None);
        let j = "{\n  \"$schema\": \"x\",\n  \"name\": \"cloud-mail\",\n  \"main\": \"src/index.ts\",";
        assert_eq!(jsonc_field(j, "name").as_deref(), Some("cloud-mail"));
    }

    #[test]
    fn parses_accounts_destinations_and_mx() {
        let whoami = json!({"authenticated": true, "accounts": [{"id": "0123456789abcdef0123456789abcdef", "name": "Me's Account"}, {"id": "11111111111111111111111111111111", "name": "Team"}]});
        assert_eq!(parse_accounts(&whoami), vec![("Me's Account".into(), "0123456789abcdef0123456789abcdef".into()), ("Team".into(), "11111111111111111111111111111111".into())]);
        assert!(parse_accounts(&json!({"authenticated": false})).is_empty());
        let list = json!([{"id": "0123456789abcdef0123456789abcdef", "name": "Me's Account", "type": "standard"}]);
        assert_eq!(parse_accounts(&list), vec![("Me's Account".into(), "0123456789abcdef0123456789abcdef".into())]);
        assert!(parse_accounts(&json!({"authenticated": true, "accounts": []})).is_empty());
        let dests = r#"[{"id": "b51d", "email": "Me@Hey.com", "verified": "2025-12-26T05:50:19Z", "status": "verified"},
                        {"id": "87be", "email": "new@gmail.com", "verified": null, "status": "pending"}]"#;
        assert_eq!(parse_destinations(dests), vec![("me@hey.com".into(), true), ("new@gmail.com".into(), false)]);
        let mx = json!({"Answer": [{"type": 15, "data": "20 route2.mx.cloudflare.net."}, {"type": 5, "data": "x."}, {"type": 15, "data": "10 ASPMX.L.Google.com."}]});
        assert_eq!(parse_mx_answer(&mx), vec!["aspmx.l.google.com", "route2.mx.cloudflare.net"]);
        assert!(parse_mx_answer(&json!({"Status": 3})).is_empty());
    }

    #[test]
    fn parses_lists() {
        let d1 = "[{\"uuid\":\"u1\",\"name\":\"cloudmail-old\"},{\"uuid\":\"u2\",\"name\":\"cloudmail\"}]";
        assert_eq!(parse_d1_list(d1, "cloudmail").as_deref(), Some("u2"));
        assert_eq!(parse_d1_list(d1, "nope"), None);
        let r2 = "{\"buckets\": [{\"name\": \"cloud-mail\", \"creation_date\": \"2026\"}, {\"name\": \"cloudmail\"}]}";
        assert_eq!(parse_r2_buckets(r2), vec!["cloud-mail", "cloudmail"]);
        assert!(parse_r2_buckets("").is_empty());
        assert!(routing_enabled("{\"name\": \"example.com\", \"enabled\": true, \"status\": \"ready\"}"));
        assert!(!routing_enabled("{\"name\": \"example.com\", \"enabled\": false}"));
        let sending = "[{\"name\": \"example.com\", \"enabled\": true}, {\"name\": \"news.other.com\", \"enabled\": true}, {\"name\": \"off.com\", \"enabled\": false}]";
        assert!(sending_enabled(sending, "example.com"));
        assert!(!sending_enabled(sending, "other.com"));
        assert!(!sending_enabled(sending, "off.com"));
    }

    #[test]
    fn parses_mailbox_specs() {
        assert_eq!(parse_mailbox_spec("Hi@Example.com").unwrap(), ("hi@example.com".into(), true));
        assert_eq!(parse_mailbox_spec("support@example.com:direct").unwrap(), ("support@example.com".into(), false));
        assert!(parse_mailbox_spec("nope").is_err());
        assert!(parse_mailbox_spec("a@b.com:weird").is_err());
    }

    fn dry_run_args(dir: &Path, force: bool) -> SetupArgs {
        SetupArgs {
            worker_dir: Some(dir.to_path_buf()),
            name: "cloudmail".into(),
            mailboxes: vec!["hi@example.com".into()],
            mailbox_flags: vec!["support@example.com:direct".into()],
            yes: true,
            account: None,
            forward_to: Some("me@elsewhere.com".into()),
            force,
            dry_run: true,
            // A fake cf that fails every command proves nothing mutating runs in --dry-run.
            cf: "false".into(),
        }
    }

    fn worker_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cloudmail-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("node_modules/.package-lock.json"), "{}").unwrap();
        std::fs::write(dir.join("cloudflare.config.ts"), "export default {}").unwrap();
        dir
    }

    #[test]
    fn dry_run_setup_plans_without_running() {
        let dir = worker_dir("setup-test");
        let resp = run(&dry_run_args(&dir, true), false).unwrap();
        let steps = resp.data["steps"].as_array().unwrap();
        let planned: Vec<&str> = steps.iter().filter(|s| s["status"] == "planned").filter_map(|s| s["command"].as_str()).collect();
        assert!(planned.contains(&"false d1 create --name cloudmail"));
        assert!(planned.contains(&"false r2 buckets create --name cloudmail"));
        assert!(planned.contains(&"false d1 migrations apply <new-database-id>"));
        assert!(planned.contains(&"false deploy"));
        assert!(planned.iter().any(|c| c.starts_with("false email-routing rules create -z example.com") && c.contains(r#""value":"hi@example.com""#)));
        assert!(planned.contains(&"false email-sending subdomains create -z example.com --name example.com"));
        assert!(planned.contains(&"false email-routing enable -z example.com"));
        assert!(planned.contains(&"false email-routing addresses create --email me@elsewhere.com"));
        assert!(planned.contains(&"false auth login"));
        assert!(steps.iter().any(|s| s["step"] == "write install.json" && s["status"] == "planned"));
        assert!(!dir.join(INSTALL_FILE).exists());
        assert_eq!(resp.data["dry_run"], true);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn dry_run_carries_over_a_wrangler_jsonc_from_before_cf() {
        let dir = worker_dir("setup-legacy");
        std::fs::write(dir.join(LEGACY_FILE), "{ \"name\": \"cloud-mail\", \"database_name\": \"cm-db\", \"bucket_name\": \"cm-r2\" }").unwrap();
        let resp = run(&dry_run_args(&dir, false), false).unwrap();
        let steps = resp.data["steps"].as_array().unwrap();
        let planned: Vec<&str> = steps.iter().filter(|s| s["status"] == "planned").filter_map(|s| s["command"].as_str()).collect();
        assert_eq!(resp.data["worker"], "cloud-mail");
        assert!(planned.contains(&"false d1 create --name cm-db"));
        assert!(planned.contains(&"false r2 buckets create --name cm-r2"));
        let write = steps.iter().find(|s| s["step"] == "write install.json").unwrap();
        assert_eq!(write["status"], "planned");
        assert!(write["detail"].as_str().unwrap().starts_with("from wrangler.jsonc"));
        assert!(!dir.join(INSTALL_FILE).exists());
        std::fs::remove_dir_all(dir).ok();
    }
}
