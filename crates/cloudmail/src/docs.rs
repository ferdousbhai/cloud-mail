//! Self-documentation: examples attached to every command, the `commands` tree and `agent-guide`.

use clap::{ArgAction, Command};
use serde_json::{Value, json};

use crate::output::exit;

/// Examples per command path. A test checks that every command has one and every key is real.
pub const EXAMPLES: &[(&str, &[&str])] = &[
    ("status", &["cloudmail status", "cloudmail status --json"]),
    ("inbox", &["cloudmail inbox", "cloudmail inbox --unread --limit 10", "cloudmail inbox --ids-only"]),
    ("archive", &["cloudmail archive --limit 50"]),
    ("sent", &["cloudmail sent"]),
    ("blocked", &["cloudmail blocked"]),
    ("threads", &["cloudmail threads list --folder all --since 1790000000000"]),
    ("threads list", &["cloudmail threads list --folder screener", "cloudmail threads list --folder all --limit 100"]),
    ("search", &["cloudmail search invoice", "cloudmail search \"coffee next week\" --json"]),
    ("thread", &["cloudmail thread read t_ad7e6e0b172e481aaa3c"]),
    ("thread read", &["cloudmail thread read t_ad7e6e0b172e481aaa3c", "cloudmail thread read t_ad7e6e0b172e481aaa3c --mark-read --json"]),
    ("thread archive", &["cloudmail thread archive t_1 t_2", "cloudmail inbox --ids-only | xargs cloudmail thread archive"]),
    ("thread unarchive", &["cloudmail thread unarchive t_ad7e6e0b172e481aaa3c"]),
    ("thread unread", &["cloudmail thread unread t_ad7e6e0b172e481aaa3c"]),
    ("thread markread", &["cloudmail thread markread t_1 t_2"]),
    ("thread delete", &["cloudmail thread delete t_ad7e6e0b172e481aaa3c --yes"]),
    ("screener", &["cloudmail screener", "cloudmail screener --json"]),
    ("screener list", &["cloudmail screener list --ids-only"]),
    ("screener approve", &["cloudmail screener approve alice@example.com", "cloudmail screener approve a@x.com b@y.com"]),
    ("screener block", &["cloudmail screener block spam@bad.biz"]),
    ("senders", &["cloudmail senders", "cloudmail senders --status blocked"]),
    (
        "compose",
        &[
            "cloudmail compose --to alice@example.com --subject \"Lunch?\" -m \"Tuesday at noon?\"",
            "echo \"Report attached below\" | cloudmail compose --to bob@x.com --subject Report --from support@example.com",
            "cloudmail compose --to a@x.com --subject Test -m hi --dry-run",
            "cloudmail compose --to bob@x.com --subject Invoice -m \"Attached.\" --attach invoice.pdf --attach notes.txt",
        ],
    ),
    (
        "reply",
        &[
            "cloudmail reply t_ad7e6e0b172e481aaa3c -m \"Sounds good!\"",
            "cloudmail reply t_ad7e6e0b172e481aaa3c --all --message-file reply.txt",
            "cloudmail reply t_ad7e6e0b172e481aaa3c -m \"Thanks\" --dry-run --json",
            "cloudmail reply t_ad7e6e0b172e481aaa3c -m \"Here it is\" --attach report.pdf",
        ],
    ),
    ("attachment", &["cloudmail attachment list t_ad7e6e0b172e481aaa3c"]),
    ("attachment list", &["cloudmail attachment list t_ad7e6e0b172e481aaa3c", "cloudmail attachment list t_ad7e6e0b172e481aaa3c --ids-only"]),
    ("attachment save", &["cloudmail attachment save a_31d5cc4ab87b497b8b79", "cloudmail attachment save a_31d5cc4ab87b497b8b79 -o ~/Downloads/", "cloudmail attachment save a_31d5cc4ab87b497b8b79 -o - > menu.pdf"]),
    ("raw", &["cloudmail raw m_21ea6d84119b4163b19e > message.eml", "cloudmail raw m_21ea6d84119b4163b19e -o message.eml"]),
    ("watch", &["cloudmail watch", "cloudmail watch --folder screener --interval 60", "cloudmail watch --json | while read -r line; do echo \"$line\" | jq .thread.subject; done"]),
    ("mailbox", &["cloudmail mailbox list"]),
    ("mailbox list", &["cloudmail mailbox list", "cloudmail mailbox list --json"]),
    (
        "mailbox add",
        &[
            "cloudmail mailbox add hi@example.com --name \"Jane Doe\"",
            "cloudmail mailbox add support@example.com --name \"Example Support\" --direct --route",
        ],
    ),
    ("mailbox set", &["cloudmail mailbox set support@example.com --screen false", "cloudmail mailbox set hi@example.com --position 0"]),
    ("mailbox remove", &["cloudmail mailbox remove old@example.com --yes"]),
    ("settings", &["cloudmail settings get"]),
    ("settings get", &["cloudmail settings get --json"]),
    ("settings set", &["cloudmail settings set forward-to me@elsewhere.com", "cloudmail settings set forward-to \"\""]),
    ("config", &["cloudmail config show"]),
    ("config show", &["cloudmail config show", "cloudmail config show --show-token --json"]),
    ("config set", &["cloudmail config set api-url https://cloudmail.you.workers.dev", "cloudmail config set poll-seconds 30"]),
    ("config path", &["cloudmail config path"]),
    ("account", &["cloudmail account list"]),
    ("account list", &["cloudmail account list", "cloudmail account list --json"]),
    (
        "account add",
        &[
            "cloudmail account add gmail",
            "cloudmail account add gmail --name work",
            "cloudmail account add gmail --client-id 123-abc.apps.googleusercontent.com --client-secret GOCSPX-xyz",
            "cloudmail account add hey",
            "cloudmail account add hey --command ~/.local/bin/hey",
            "cloudmail account add hey --no-login --json",
            "cloudmail account add icloud --email you@icloud.com",
            "cloudmail account add icloud --email you@icloud.com --alias you@example.com",
            "printf '%s\\n' \"$APP_PASSWORD\" | cloudmail account add icloud --email you@icloud.com --password-stdin --json",
        ],
    ),
    ("account login", &["cloudmail account login gmail", "cloudmail account login hey", "cloudmail account login icloud"]),
    ("account remove", &["cloudmail account remove gmail", "cloudmail account remove hey", "cloudmail account remove icloud"]),
    (
        "setup",
        &[
            "cloudmail setup hi@example.com",
            "cloudmail setup hi@example.com support@example.com:direct --forward-to me@gmail.com",
            "cloudmail setup hi@example.com --dry-run",
            "cloudmail setup hi@example.com --yes --account 0123456789abcdef0123456789abcdef --json",
        ],
    ),
    ("commands", &["cloudmail commands", "cloudmail commands --json"]),
    ("agent-guide", &["cloudmail agent-guide"]),
];

pub fn examples_for(path: &str) -> Option<&'static [&'static str]> {
    EXAMPLES.iter().find(|(p, _)| *p == path).map(|(_, e)| *e)
}

/// "thread read" from "thread" and "read"; a top-level command from "" and its name.
fn sub_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() { name.to_string() } else { format!("{prefix} {name}") }
}

/// Adds an "Examples:" section to every command's long help.
pub fn with_examples(cmd: Command) -> Command {
    fn walk(mut cmd: Command, prefix: &str) -> Command {
        let names: Vec<String> = cmd.get_subcommands().map(|c| c.get_name().to_string()).collect();
        for name in names {
            let path = sub_path(prefix, &name);
            cmd = cmd.mut_subcommand(&name, |sub| {
                let sub = match examples_for(&path) {
                    Some(ex) => {
                        let text = ex.iter().map(|e| format!("  {e}")).collect::<Vec<_>>().join("\n");
                        sub.after_long_help(format!("Examples:\n{text}"))
                    }
                    None => sub,
                };
                walk(sub, &path)
            });
        }
        cmd
    }
    walk(cmd, "").after_long_help(
        "Output: human-readable on a terminal, a JSON envelope when piped (--json forces it).\n\
         Run `cloudmail commands` for every command with examples, `cloudmail agent-guide` for scripting.",
    )
}

fn arg_json(arg: &clap::Arg) -> Value {
    let takes_value = matches!(arg.get_action(), ArgAction::Set | ArgAction::Append);
    let possible: Vec<String> = arg.get_possible_values().iter().map(|v| v.get_name().to_string()).collect();
    let default: Vec<String> = arg.get_default_values().iter().map(|v| v.to_string_lossy().into_owned()).collect();
    json!({
        "name": arg.get_id().as_str(),
        "long": arg.get_long().map(|l| format!("--{l}")),
        "short": arg.get_short().map(|s| format!("-{s}")),
        "positional": arg.is_positional(),
        "required": arg.is_required_set(),
        "takes_value": takes_value,
        "multiple": matches!(arg.get_action(), ArgAction::Append) || arg.get_num_args().is_some_and(|n| n.max_values() > 1),
        "possible_values": if possible.is_empty() { Value::Null } else { json!(possible) },
        "default": if default.is_empty() { Value::Null } else { json!(default.join(",")) },
        "help": arg.get_help().map(|h| h.to_string()),
    })
}

fn visible_args(cmd: &Command) -> impl Iterator<Item = &clap::Arg> {
    cmd.get_arguments()
        .filter(|a| !a.is_hide_set() && !a.is_global_set() && !matches!(a.get_id().as_str(), "help" | "version"))
}

/// The command tree as JSON, leaves first-class, built from clap metadata.
pub fn commands_json(root: &Command) -> Value {
    fn walk(cmd: &Command, path: &str, out: &mut Vec<Value>) {
        for sub in cmd.get_subcommands() {
            let p = sub_path(path, sub.get_name());
            let usage = format!(
                "cloudmail {p}{}",
                visible_args(sub)
                    .map(|a| {
                        let name = if a.is_positional() {
                            format!("<{}>", a.get_id().as_str().to_uppercase())
                        } else {
                            format!("--{}", a.get_long().unwrap_or(a.get_id().as_str()))
                        };
                        if a.is_required_set() { format!(" {name}") } else { format!(" [{name}]") }
                    })
                    .collect::<String>()
            );
            out.push(json!({
                "command": format!("cloudmail {p}"),
                "usage": usage,
                "description": sub.get_about().map(|a| a.to_string()).unwrap_or_default(),
                "has_subcommands": sub.has_subcommands(),
                "args": visible_args(sub).map(arg_json).collect::<Vec<_>>(),
                "examples": examples_for(&p).unwrap_or_default(),
            }));
            walk(sub, &p, out);
        }
    }
    let mut commands = Vec::new();
    walk(root, "", &mut commands);
    json!({
        "global_flags": root.get_arguments().filter(|a| a.is_global_set()).map(arg_json).collect::<Vec<_>>(),
        "commands": commands,
        "exit_codes": exit::TABLE.iter().map(|(c, d)| json!({ "code": c, "meaning": d })).collect::<Vec<_>>(),
    })
}

pub fn commands_text(tree: &Value) -> String {
    let mut out = String::from("cloudmail commands (run `cloudmail <command> --help` for details)\n");
    for c in tree["commands"].as_array().into_iter().flatten() {
        out.push_str(&format!("\n{}\n    {}\n", c["usage"].as_str().unwrap_or_default(), c["description"].as_str().unwrap_or_default()));
        for e in c["examples"].as_array().into_iter().flatten() {
            out.push_str(&format!("    $ {}\n", e.as_str().unwrap_or_default()));
        }
    }
    let globals = tree["global_flags"].as_array().into_iter().flatten().filter_map(|f| f["long"].as_str()).collect::<Vec<_>>();
    out.push_str(&format!("\nGlobal flags: {}\n\nExit codes:\n", globals.join("  ")));
    for (code, meaning) in exit::TABLE {
        out.push_str(&format!("  {code}  {meaning}\n"));
    }
    out.trim_end().to_string()
}

const AGENT_GUIDE: &str = r#"# cloudmail for agents

cloudmail is a CLI for a personal email service running on Cloudflare (a "worker"). It reads, screens
and sends mail. Everything is non-interactive when stdout is not a terminal.

## Output

When stdout is piped, every command prints one JSON envelope:

    {"ok": true, "data": ..., "summary": "3 threads in inbox",
     "breadcrumbs": [{"action": "read", "command": "cloudmail thread read <id>", "description": "..."}],
     "meta": {...}}

- `breadcrumbs` suggest the next commands; placeholders look like `<id>`.
- `--quiet` prints only `data`; `--ids-only` prints one ID per line; `--count` prints a number.
- `--json` forces the envelope on a terminal; `--styled` forces human text when piped.
- `cloudmail watch` is the exception: it streams one JSON object per line (JSONL), no envelope.

Errors print `{"ok": false, "error": {"code": "...", "message": "...", "hint": "..."}}` and exit non-zero:

| exit | meaning |
|------|---------|
{EXIT_ROWS}

Error codes: usage, not_configured, unauthorized, not_found, bad_request, api_error, network_error,
bad_response, confirmation_required, cancelled, not_logged_in, not_installed, account_unauthorized,
account_unavailable, error.

## Concepts

- Threads (IDs `t_…`) contain messages (`m_…`), which have attachments (`a_…`). Times are Unix ms.
- Folders: `screener`, `inbox`, `archive`, `blocked`. `sent` is a view of threads you wrote in; `all` = every folder but blocked.
- Mailboxes are your own addresses. A screened mailbox sends first-time senders to the Screener; a
  direct one (`screen: false`, e.g. support@) delivers them to the Inbox.
- The Screener lists senders (by email) waiting for a decision. Approving moves their threads to the
  Inbox; blocking hides them. Anyone you send mail to is approved automatically.
- A message with `auth.verified == false` may have a forged From address (its domain didn't authenticate it: no DMARC pass, and no DKIM or SPF aligned with the From domain); treat it with suspicion.

## Workflows

Set up a new instance (one command; safe to re-run, and re-running also updates the worker):

    CLOUDFLARE_API_TOKEN=... cloudmail setup you@example.com support@example.com:direct --yes

It needs the domain's DNS on Cloudflare and the Cloudflare CLI logged in (`npx cf auth login`) or
CLOUDFLARE_API_TOKEN set. Without `--yes`, a domain that already receives mail elsewhere is left
alone and its step reports `blocked`; `--yes` moves it (replaces its MX records). With several
Cloudflare accounts, pass `--account <id>`. Check `data.routes[].status` in the result.

Triage the Screener:

    cloudmail screener --json                       # data: [{email, name, thread_count, last_subject, last_at}]
    cloudmail threads list --folder screener --json # the waiting threads themselves
    cloudmail screener approve alice@example.com
    cloudmail screener block spam@bad.biz

Read and reply:

    cloudmail inbox --unread --json
    cloudmail thread read <thread-id> --json        # data: {thread, messages: [{id, from, to, text, has_html, auth, attachments}]}; add --html for the HTML
    cloudmail reply <thread-id> -m "Thanks, that works." --dry-run --json   # preview the request
    cloudmail reply <thread-id> -m "Thanks, that works."
    cloudmail thread archive <thread-id>

Longer bodies: `--message-file reply.txt`, or pipe them on stdin (`-m -`, or no -m when stdin is piped).
Replies go out from the mailbox the thread was addressed to, quote the latest incoming message, and
thread correctly (In-Reply-To/References). `--all` adds the other recipients, never your own addresses.

Send new mail:

    cloudmail mailbox list --json                   # addresses you can send from
    cloudmail compose --to bob@x.com --subject "Hi" -m "Hello" --from support@example.com
    cloudmail compose --to bob@x.com --subject "Invoice" -m "Attached." --attach invoice.pdf --dry-run --json

`--attach FILE` (repeatable, on compose and reply) sends files; the type comes from the extension.
A missing or unreadable file is a usage error before anything is sent. Your worker sends at most
about 3.5 MiB of attachments (Cloudflare Email Service takes 5 MiB per message once encoded); HEY
and Gmail 25 MB; iCloud Mail 14 MiB (20 MB per message once encoded). Over that is a `bad_request` naming the limit. `--dry-run` lists the files as
`{filename, mime_type, size}`; a worker send's `message.attachments` lists them as stored.

Watch for new mail (JSONL, one object per new or updated thread):

    cloudmail watch --folder all --interval 30
    # {"event":"thread","thread":{"id":"t_…","folder":"screener","from":{…},"subject":"…",…}}

## Linked accounts (HEY, Gmail, iCloud Mail)

Optional. `cloudmail account add hey` links a HEY account through the official `hey` CLI (it must be
installed and signed in; on a terminal, add runs `hey auth login` for you, otherwise it fails with
`not_logged_in` and the hint). `cloudmail account add gmail` links Gmail through Google's Workspace CLI
`gws` (installed with `npm install -g @googleworkspace/cli`); on a terminal it opens one Google sign-in
in the browser (Gmail access only, kept in cloudmail's own gws directory, apart from any gws of yours),
otherwise it fails with `not_logged_in`; `--login` signs in without a terminal. A build without
cloudmail's Google client fails with `not_configured` until CLOUDMAIL_GOOGLE_CLIENT_ID/SECRET or
`--client-id/--client-secret` name one. `cloudmail account add icloud --email you@icloud.com` links
iCloud Mail over IMAP and SMTP with an app-specific password (made at account.apple.com: Sign-In and
Security → App-Specific Passwords): on a terminal it opens that page and asks for the password,
otherwise pass it on stdin with `--password-stdin` (else `not_logged_in`). It is checked by signing in,
then kept in `~/.config/cloudmail/icloud/<name>`, readable only by you. `--alias` adds an address you
also send from (Hide My Email, a custom domain). `cloudmail account list --json` shows each account and
whether it works.

Once linked, their mail appears next to yours with `"account": "hey"` / `"gmail"` / `"icloud"` on threads
and HEY's Screener senders (your worker's own mail has no `account` key). IDs are prefixed and go back
to their account:

    hey:<topic>:<box-item>   a thread in a box (read, reply, archive, markread/unread)
    hey:<topic>              a thread outside any box (read and reply only)
    hey:<topic>/<entry>      a message        hey:<id>   an attachment or a Screener sender
    gmail:<thread>           a Gmail thread   gmail:<thread>/<message>   a message (also for `raw`)
    gmail:<message>:<part>   a Gmail attachment
    icloud:t<root>           an iCloud Mail thread (grouped by Message-ID/References; <root> is the
                             first message's Message-ID in base64url, so it survives archiving)
    icloud:t<root>/<box>.<uidvalidity>.<uid>   a message (also for `raw`)
    icloud:<box>.<uidvalidity>.<uid>#<n>       an iCloud Mail attachment

Folders: `inbox` = your Inbox + HEY's Imbox + Gmail's and iCloud's Inboxes; `archive` = your Archive +
HEY's Paper Trail + Gmail threads out of the Inbox + iCloud's Archive mailbox (archiving a HEY thread
moves it to Paper Trail, a Gmail thread loses its Inbox label, an iCloud thread moves to Archive;
unarchive undoes each); `sent` = yours + Gmail's + iCloud's; `screener` = yours + HEY's (a sender
waiting in both shows once; Gmail and iCloud Mail have no Screener, their mail goes straight to the Inbox);
`blocked` is yours only. HEY's other boxes are extra folders: `threads list --folder
feed|paper-trail|set-aside|reply-later`. Search covers all of them (Gmail reads its own search syntax).

    cloudmail inbox --json                           # merged by time
    cloudmail thread read hey:9001:7001 --json
    cloudmail reply hey:9001:7001 -m "Thanks"        # sent by `hey reply`, from your HEY address
    cloudmail reply gmail:18c2f0a1b2 -m "Thanks"     # sent through Gmail, threaded, from your Gmail address
    cloudmail compose --from you@gmail.com --to a@b.com --subject Hi -m "Hello"   # sent through Gmail
    cloudmail screener approve alice@example.com     # decides her everywhere she waits
    cloudmail screener approve hey:5001              # only in HEY

If your worker forwards to a linked address (or Gmail forwards into your worker), the account's copy of
each message is hidden (`meta.duplicates_hidden`): Gmail and iCloud copies by Message-ID, HEY copies by sender,
subject and time. A linked account's failure never fails a command about your own mail: the rest is
returned and `meta.warnings` lists `{account, code, message}`. A command about a linked account's ID
fails with `account_unauthorized` (exit 3: `cloudmail account login <name>` signs it in again in the
browser, or for iCloud with a new app-specific password), `account_unavailable` (exit 5: its CLI
missing or failing, or Google or iCloud unreachable), or `not_found`. Linked accounts can't delete threads from here, HEY gives out no raw .eml,
and `watch` follows your worker only.

Destructive commands (`thread delete`, `mailbox remove`) need `--yes` when not on a terminal.
`cloudmail commands --json` lists every command, flag and example.
"#;

pub fn agent_guide() -> String {
    let rows = exit::TABLE
        .iter()
        .map(|(c, d)| format!("| {c} | {d} |"))
        .collect::<Vec<_>>()
        .join("\n");
    AGENT_GUIDE.replace("{EXIT_ROWS}", &rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn paths(cmd: &Command, prefix: &str, out: &mut Vec<String>) {
        for sub in cmd.get_subcommands() {
            let p = sub_path(prefix, sub.get_name());
            out.push(p.clone());
            paths(sub, &p, out);
        }
    }

    #[test]
    fn every_command_has_examples_and_no_stale_examples() {
        let mut all = Vec::new();
        paths(&crate::cli::Cli::command(), "", &mut all);
        for p in &all {
            assert!(examples_for(p).is_some(), "missing examples for `{p}`");
        }
        for (p, ex) in EXAMPLES {
            assert!(all.iter().any(|a| a == p), "examples for unknown command `{p}`");
            assert!(!ex.is_empty());
            for e in *ex {
                assert!(e.starts_with("cloudmail ") || e.contains("| cloudmail") || e.contains("| xargs cloudmail") || e.starts_with("echo "), "{e}");
            }
        }
    }

    #[test]
    fn examples_parse() {
        // Every example that is a plain `cloudmail …` invocation must parse with the real CLI.
        for (_, ex) in EXAMPLES {
            for e in *ex {
                if !e.starts_with("cloudmail ") || e.contains('|') || e.contains('>') {
                    continue;
                }
                let args = shell_split(e);
                let res = crate::cli::Cli::command().try_get_matches_from(&args);
                assert!(res.is_ok(), "example does not parse: {e}\n{}", res.unwrap_err());
            }
        }
    }

    fn shell_split(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut quoted = false;
        let mut has = false;
        for ch in s.chars() {
            match ch {
                '"' => {
                    quoted = !quoted;
                    has = true;
                }
                ' ' if !quoted => {
                    if has || !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                    has = false;
                }
                _ => cur.push(ch),
            }
        }
        if has || !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    #[test]
    fn commands_tree_has_args_and_exit_codes() {
        let tree = commands_json(&crate::cli::Cli::command());
        let cmds = tree["commands"].as_array().unwrap();
        let compose = cmds.iter().find(|c| c["command"] == "cloudmail compose").unwrap();
        assert!(compose["args"].as_array().unwrap().iter().any(|a| a["long"] == "--to" && a["required"] == true));
        assert!(!compose["examples"].as_array().unwrap().is_empty());
        assert_eq!(tree["exit_codes"].as_array().unwrap().len(), exit::TABLE.len());
        assert!(tree["global_flags"].as_array().unwrap().iter().any(|a| a["long"] == "--json"));
        assert!(commands_text(&tree).contains("\nGlobal flags: --json  --quiet  --ids-only  --count  --styled\n\nExit codes:\n"));
    }

    #[test]
    fn guide_lists_exit_codes() {
        let g = agent_guide();
        assert!(g.contains("| 3 | not configured"));
        assert!(!g.contains("{EXIT_ROWS}"));
    }
}
