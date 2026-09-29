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

pub fn render_template(template: &str, worker: &str, database: &str, database_id: &str, bucket: &str) -> String {
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
    let start = json_text.find('[')?;
    let v: Value = serde_json::from_str(&json_text[start..]).ok()?;
    v.as_array()?
        .iter()
        .find(|d| d["name"] == name)
        .and_then(|d| d["uuid"].as_str().map(str::to_string))
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
    pub dry_run: bool,
    pub steps: Vec<Step>,
}

impl Wrangler {
    pub fn new(program: &str, dir: PathBuf, dry_run: bool) -> Self {
        Self { program: program.split_whitespace().map(str::to_string).collect(), dir, dry_run, steps: Vec::new() }
    }

    fn render(&self, args: &[&str]) -> String {
        let mut parts = self.program.clone();
        parts.extend(args.iter().map(|a| if a.contains(' ') { format!("'{a}'") } else { a.to_string() }));
        parts.join(" ")
    }

    pub fn record(&mut self, step: &str, status: &str, command: Option<String>, detail: Option<String>) {
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
        self.exec(&prog[..1], &program[1..], None)?;
        self.record(step, "done", Some(command), None);
        Ok(())
    }

    fn exec(&self, program: &[String], args: &[&str], stdin: Option<&str>) -> CliResult<String> {
        let (bin, pre) = program.split_first().ok_or_else(|| CliError::usage("empty --wrangler command"))?;
        let mut cmd = Command::new(bin);
        cmd.args(pre).args(args).current_dir(&self.dir).stdout(Stdio::piped()).stderr(Stdio::piped());
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
    if let Some(dir) = flag {
        if is_worker_dir(dir) {
            return Ok(dir.to_path_buf());
        }
        return Err(CliError::usage(format!("{} is not a cloudmail worker directory", dir.display()))
            .hint("pass the worker/ directory of a cloudmail checkout"));
    }
    let local = PathBuf::from("worker");
    if is_worker_dir(&local) {
        return Ok(local);
    }
    if let Some(packaged) = PACKAGED_WORKER_DIRS.iter().map(Path::new).find(|d| is_worker_dir(d)) {
        let data = dirs::data_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/share"));
        let dest = data.join("cloudmail").join("worker");
        copy_tree(packaged, &dest)?;
        return Ok(dest);
    }
    Err(CliError::usage("couldn't find the cloudmail worker source")
        .hint("install the cloudmail package, run from a clone (git clone https://github.com/ferdousbhai/cloud-mail), or pass --worker-dir"))
}

fn copy_tree(from: &Path, to: &Path) -> CliResult<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
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

/// Makes sure sending is enabled for the address's domain and its routing rule points at the worker.
pub fn route_address(
    w: &mut Wrangler,
    address: &str,
    worker: &str,
    take_over: bool,
    enable_routing: bool,
    sending_zones: &mut Option<Vec<String>>,
    routing_zones: &mut Option<Vec<String>>,
) -> CliResult<RouteOutcome> {
    let domain = domain_of(address).ok_or_else(|| CliError::usage(format!("not an email address: {address}")))?.to_string();

    if routing_zones.is_none() {
        *routing_zones = Some(enabled_zones(&w.probe(&["email", "routing", "list"])?));
    }
    if !routing_zones.as_ref().is_some_and(|z| z.contains(&domain)) {
        if !enable_routing {
            let detail = format!(
                "Email Routing is not enabled for {domain}; enabling it replaces the domain's MX records. Re-run with --enable-routing if that's intended, or enable it in the Cloudflare dashboard"
            );
            w.record(&format!("route {address}"), "blocked", None, Some(detail.clone()));
            return Ok(RouteOutcome { address: address.into(), status: "blocked".into(), detail });
        }
        w.mutate(&format!("enable routing for {domain}"), &["email", "routing", "enable", &domain], None)?;
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
        Some(r) if !take_over => {
            let detail = format!("existing rule {} sends it to {}; left alone (use --take-over-route(s) to replace)", r.id, r.action);
            w.record(&step, "skipped", None, Some(detail.clone()));
            Ok(outcome("skipped", detail))
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

pub fn route_for_mailbox(address: &str, args: &RouteArgs) -> CliResult<Value> {
    let dir = resolve_worker_dir(args.worker_dir.as_deref())?;
    let worker = match &args.worker_name {
        Some(n) => n.clone(),
        None => std::fs::read_to_string(dir.join("wrangler.jsonc"))
            .ok()
            .and_then(|t| parse_worker_name(&t))
            .ok_or_else(|| CliError::usage("could not read the worker name from wrangler.jsonc").hint("pass --worker-name"))?,
    };
    let mut w = Wrangler::new(&args.wrangler, dir, false);
    let outcome = route_address(&mut w, address, &worker, args.take_over_route, false, &mut None, &mut None)?;
    Ok(json!({ "status": outcome.status, "detail": outcome.detail, "worker": worker, "steps": w.steps }))
}

// ---------- setup ----------

fn plural_steps(n: usize) -> String {
    if n == 1 { "1 step".into() } else { format!("{n} steps") }
}

pub fn run(args: &SetupArgs) -> CliResult {
    let dir = resolve_worker_dir(args.worker_dir.as_deref())?;
    let template_path = dir.join("wrangler.template.jsonc");
    let template = std::fs::read_to_string(&template_path)
        .map_err(|e| CliError::generic(format!("could not read {}: {e}", template_path.display())))?;
    let mailboxes = args.mailboxes.iter().map(|m| parse_mailbox_spec(m)).collect::<CliResult<Vec<_>>>()?;
    let config_path = dir.join("wrangler.jsonc");
    let current = std::fs::read_to_string(&config_path).ok();
    // An existing wrangler.jsonc wins unless --force: reuse its worker, database and bucket.
    let keep = current.is_some() && !args.force;
    let field = |k: &str| current.as_deref().filter(|_| keep).and_then(|c| jsonc_field(c, k));
    let worker_name = field("name").unwrap_or_else(|| args.name.clone());
    let db_name = field("database_name").unwrap_or_else(|| args.name.clone());
    let bucket_name = field("bucket_name").unwrap_or_else(|| args.name.clone());
    let mut w = Wrangler::new(&args.wrangler, dir.clone(), args.dry_run);

    // 1. dependencies
    if dir.join("node_modules").exists() {
        w.record("install worker dependencies", "exists", None, None);
    } else {
        w.run_program("install worker dependencies", &["npm", "install"])?;
    }

    // 2. login
    match w.query(&["whoami"]) {
        Ok(out) if strip_ansi(&out).contains("You are logged in") => w.record("wrangler login", "ok", None, None),
        Ok(_) | Err(_) if args.dry_run => w.record("wrangler login", "unknown", None, Some("run `npx wrangler login` if not logged in".into())),
        _ => {
            return Err(CliError::new("not_logged_in", exit::AUTH, "wrangler is not logged in to Cloudflare")
                .hint(format!("run `{} login` in {}", args.wrangler, dir.display())));
        }
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
    let rendered = render_template(&template, &worker_name, &db_name, &database_id, &bucket_name);
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
    let deploy_out = w.mutate("deploy worker", &["deploy"], None)?;
    let url = if args.dry_run {
        format!("https://{worker_name}.<your-subdomain>.workers.dev")
    } else {
        parse_deploy_url(&deploy_out).ok_or_else(|| CliError::generic("deployed, but could not find the workers.dev URL in wrangler's output"))?
    };

    // 6. token + local config
    let cfg_path = config::path();
    let existing_cfg = config::read_file(&cfg_path).ok().flatten();
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
    } else if existing_cfg.is_some() && !args.force && args.dry_run {
        w.record("API token", "blocked", None, Some(format!("{} points at another worker; setup would stop here without --force", cfg_path.display())));
        String::new()
    } else if existing_cfg.is_some() && !args.force {
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
        wait_until_live(&client)?;
        w.record("wait for the worker", "done", None, Some(url.clone()));
    }

    // 8. mailboxes and routes
    let mut routes = Vec::new();
    let (mut sending, mut routing) = (None, None);
    for (addr, screen) in &mailboxes {
        if args.dry_run {
            w.record(&format!("mailbox {addr}"), "planned", None, Some(if *screen { "screened" } else { "direct" }.into()));
        } else {
            client.put_mailbox(addr, &MailboxUpdate { screen: Some(*screen), ..Default::default() })?;
            w.record(&format!("mailbox {addr}"), "done", None, Some(if *screen { "screened" } else { "direct" }.into()));
        }
        let r = route_address(&mut w, addr, &worker_name, args.take_over_routes, args.enable_routing, &mut sending, &mut routing)?;
        routes.push(json!({ "address": r.address, "status": r.status, "detail": r.detail }));
    }
    if let Some(fwd) = &args.forward_to {
        if args.dry_run {
            w.record("forward_to", "planned", None, Some(fwd.clone()));
        } else {
            client.update_settings(&json!({ "forward_to": fwd }))?;
            w.record("forward_to", "done", None, Some(fwd.clone()));
        }
    }

    let blocked = w.steps.iter().filter(|s| s.status == "blocked").count();
    let mut summary = if args.dry_run {
        format!("Planned {} steps (dry run, nothing changed)", w.steps.iter().filter(|s| s.status == "planned").count())
    } else {
        format!("cloudmail is deployed at {url}")
    };
    if blocked > 0 {
        summary.push_str(&format!("; {} need attention", plural_steps(blocked)));
    }
    let human = w
        .steps
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
        .join("\n");
    Ok(Response::new(
        json!({ "dry_run": args.dry_run, "worker": worker_name, "url": url, "steps": w.steps, "routes": routes }),
        summary.clone(),
    )
    .human(format!("{human}\n\n{summary}"))
    .crumbs(vec![
        crumb("status", "cloudmail status", "Check the worker and counts"),
        crumb("mailbox", "cloudmail mailbox add <address> --route", "Add another address"),
        crumb("watch", "cloudmail watch", "Wait for mail to arrive"),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let r = render_template(t, "cloudmail", "cloudmail-db", "abc-123", "cloudmail-bucket");
        assert_eq!(r, "{ \"name\": \"cloudmail\", \"database_name\": \"cloudmail-db\", \"database_id\": \"abc-123\", \"bucket_name\": \"cloudmail-bucket\" }");
        assert!(!r.contains("__"));
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
        std::fs::write(dir.join("wrangler.template.jsonc"), "{ \"name\": \"__WORKER_NAME__\" }").unwrap();
        // A fake wrangler that fails every command proves nothing mutating runs in --dry-run.
        let args = SetupArgs {
            worker_dir: Some(dir.clone()),
            name: "cloudmail".into(),
            mailboxes: vec!["hi@example.com".into(), "support@example.com:direct".into()],
            take_over_routes: false,
            enable_routing: true,
            forward_to: Some("me@elsewhere.com".into()),
            force: true,
            dry_run: true,
            wrangler: "false".into(),
        };
        let resp = run(&args).unwrap();
        let steps = resp.data["steps"].as_array().unwrap();
        let planned: Vec<&str> = steps.iter().filter(|s| s["status"] == "planned").filter_map(|s| s["command"].as_str()).collect();
        assert!(planned.contains(&"false d1 create cloudmail"));
        assert!(planned.contains(&"false deploy"));
        assert!(planned.iter().any(|c| c.contains("routing rules create example.com") && c.contains("--match-value hi@example.com")));
        assert!(planned.iter().any(|c| c.contains("email sending enable example.com")));
        assert!(!dir.join("wrangler.jsonc").exists());
        assert_eq!(resp.data["dry_run"], true);
        std::fs::remove_dir_all(dir).ok();
    }
}
