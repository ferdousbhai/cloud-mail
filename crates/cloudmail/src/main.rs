mod accounts;
mod admin;
mod cli;
mod docs;
mod mail;
mod output;
mod render;
mod setup;

use clap::{CommandFactory, FromArgMatches};
use std::cell::OnceCell;
use std::io::Write;

use cli::{Cli, Command};
use cloudmail_api::{Client, Mail, config};
use output::{CliError, CliResult, Mode, Response, exit};

pub struct Ctx {
    pub mode: Mode,
    mail: OnceCell<Mail>,
}

impl Ctx {
    /// Someone is at the terminal to answer questions.
    pub fn interactive(&self) -> bool {
        self.mode == Mode::Human && output::stdin_is_tty() && output::stdout_is_tty()
    }

    pub fn client(&self) -> CliResult<&Client> {
        Ok(&self.mail()?.client)
    }

    /// The worker plus any linked accounts (HEY, Gmail) from the config.
    pub fn mail(&self) -> CliResult<&Mail> {
        if let Some(m) = self.mail.get() {
            return Ok(m);
        }
        let cfg = config::load()?;
        Ok(self.mail.get_or_init(|| Mail::from_config(&cfg)))
    }
}

fn mode_for(g: &cli::GlobalArgs) -> Mode {
    if g.ids_only {
        Mode::Ids
    } else if g.count {
        Mode::Count
    } else if g.quiet {
        Mode::Quiet
    } else if g.json {
        Mode::Json
    } else if g.styled || output::stdout_is_tty() {
        Mode::Human
    } else {
        Mode::Json
    }
}

/// Output mode guessed from raw args, for errors raised before clap has parsed them.
fn mode_from_raw_args() -> Mode {
    let args: Vec<String> = std::env::args().collect();
    let has = |f: &str| args.iter().any(|a| a == f);
    mode_for(&cli::GlobalArgs {
        json: has("--json"),
        quiet: has("--quiet"),
        ids_only: has("--ids-only"),
        count: has("--count"),
        styled: has("--styled"),
    })
}

fn main() {
    config::migrate_legacy();
    let cmd = docs::with_examples(Cli::command());
    let matches = match cmd.try_get_matches() {
        Ok(m) => m,
        Err(e) => {
            use clap::error::ErrorKind;
            if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
                let _ = e.print();
                std::process::exit(exit::OK);
            }
            let mode = mode_from_raw_args();
            if mode.is_machine() {
                let rendered = e.render().to_string();
                let message = rendered
                    .lines()
                    .take_while(|l| !l.starts_with("Usage:") && !l.starts_with("For more information"))
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                let message = message.trim_start_matches("error: ");
                CliError::usage(if message.is_empty() { "invalid arguments" } else { message }).hint("run `cloudmail commands --json` or `cloudmail <command> --help`").print(mode);
                std::process::exit(exit::USAGE);
            }
            let _ = e.print();
            std::process::exit(exit::USAGE);
        }
    };
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            std::process::exit(exit::USAGE);
        }
    };
    let ctx = Ctx { mode: mode_for(&cli.global), mail: OnceCell::new() };
    match dispatch(&ctx, cli.command) {
        Ok(r) => {
            r.print(ctx.mode);
            std::process::exit(exit::OK);
        }
        Err(e) => {
            e.print(ctx.mode);
            std::process::exit(e.exit);
        }
    }
}

fn dispatch(ctx: &Ctx, command: Option<Command>) -> CliResult {
    use cli::*;
    let Some(command) = command else { return Ok(admin::orientation()) };
    match command {
        Command::Status => admin::status(ctx),
        Command::Inbox(a) => mail::list(ctx, Folder::Inbox, &a),
        Command::Archive(a) => mail::list(ctx, Folder::Archive, &a),
        Command::Sent(a) => mail::list(ctx, Folder::Sent, &a),
        Command::Blocked(a) => mail::list(ctx, Folder::Blocked, &a),
        Command::Threads(ThreadsCommand::List { folder, list }) => mail::list(ctx, folder, &list),
        Command::Search(a) => mail::search(ctx, &a),
        Command::Thread(t) => mail::thread(ctx, t),
        Command::Screener(a) => mail::screener(ctx, a.command),
        Command::Senders(a) => mail::senders(ctx, a.status),
        Command::Compose(a) => mail::compose(ctx, &a),
        Command::Reply(a) => mail::reply(ctx, &a),
        Command::Attachment(a) => mail::attachment(ctx, a),
        Command::Raw(a) => mail::raw(ctx, &a),
        Command::Watch(a) => mail::watch(ctx, &a),
        Command::Mailbox(m) => admin::mailbox(ctx, m),
        Command::Settings(s) => admin::settings(ctx, s),
        Command::Config(c) => admin::config_cmd(c),
        Command::Account(a) => accounts::account(ctx, a),
        Command::Setup(a) => setup::run(&a, ctx.interactive()),
        Command::Commands => Ok(admin::commands()),
        Command::AgentGuide => {
            let guide = docs::agent_guide();
            Ok(Response::new(&guide, "cloudmail agent guide").human(guide))
        }
        Command::Skill { command: None } => {
            std::io::stdout().write_all(docs::SKILL.as_bytes())?;
            Ok(Response::silent())
        }
        Command::Skill { command: Some(SkillCommand::Install) } => docs::install_skill(),
    }
}
