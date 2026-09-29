use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "cloudmail",
    version,
    about = "Your own email on Cloudflare, from the terminal",
    long_about = "cloudmail reads, screens and sends mail through your cloudmail worker (Cloudflare Email Service).\n\
                  Output is human-readable on a terminal and a JSON envelope when piped; see `cloudmail agent-guide`.",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct GlobalArgs {
    /// Output the JSON envelope {ok, data, summary, breadcrumbs, meta} (default when piped)
    #[arg(long, global = true)]
    pub json: bool,
    /// Output only the result data as JSON, without the envelope
    #[arg(long, global = true)]
    pub quiet: bool,
    /// Output only IDs, one per line
    #[arg(long, global = true)]
    pub ids_only: bool,
    /// Output only the number of results
    #[arg(long, global = true)]
    pub count: bool,
    /// Force human-readable output even when piped
    #[arg(long, global = true)]
    pub styled: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Show configuration, worker health and mailbox counts
    Status,

    /// List threads in the Inbox
    Inbox(ListArgs),
    /// List archived threads
    Archive(ListArgs),
    /// List threads you have sent mail in
    Sent(ListArgs),
    /// List threads from blocked senders
    Blocked(ListArgs),
    /// List threads in any folder
    #[command(subcommand)]
    Threads(ThreadsCommand),
    /// Full-text search across all folders except blocked
    Search(SearchArgs),

    /// Read or change one or more threads
    #[command(subcommand)]
    Thread(ThreadCommand),

    /// List senders waiting in the Screener, or screen them in or out
    Screener(ScreenerArgs),
    /// List senders you have already screened
    Senders(SendersArgs),

    /// Write and send a new message
    Compose(ComposeArgs),
    /// Reply to the latest incoming message in a thread
    Reply(ReplyArgs),

    /// List or save attachments
    #[command(subcommand)]
    Attachment(AttachmentCommand),
    /// Download the original .eml of a received message
    Raw(RawArgs),

    /// Stream new and updated threads as they arrive
    Watch(WatchArgs),

    /// Manage the addresses the worker receives and sends from
    #[command(subcommand)]
    Mailbox(MailboxCommand),
    /// View or change worker settings (forwarding)
    #[command(subcommand)]
    Settings(SettingsCommand),
    /// View or change the local config file
    #[command(subcommand)]
    Config(ConfigCommand),

    /// Deploy a new cloudmail worker to your Cloudflare account and connect mailboxes
    Setup(SetupArgs),

    /// List every command with its arguments, flags and examples
    Commands,
    /// Print a guide for AI agents: envelope, exit codes and common workflows
    AgentGuide,
}

#[derive(Args, Debug, Clone)]
pub struct ListArgs {
    /// Maximum number of threads
    #[arg(long, short = 'n', default_value_t = 25)]
    pub limit: u32,
    /// Only threads with activity before this time (Unix ms; use a previous last_at to page)
    #[arg(long)]
    pub before: Option<i64>,
    /// Only threads with activity after this time (Unix ms)
    #[arg(long)]
    pub since: Option<i64>,
    /// Only unread threads
    #[arg(long)]
    pub unread: bool,
}

#[derive(Subcommand, Debug)]
pub enum ThreadsCommand {
    /// List threads in a folder
    List {
        #[arg(long, short = 'f', value_enum, default_value_t = Folder::Inbox)]
        folder: Folder,
        #[command(flatten)]
        list: ListArgs,
    },
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Folder {
    Inbox,
    Screener,
    Archive,
    Sent,
    Blocked,
    /// Every folder except blocked
    All,
}

impl Folder {
    pub fn as_str(self) -> &'static str {
        match self {
            Folder::Inbox => "inbox",
            Folder::Screener => "screener",
            Folder::Archive => "archive",
            Folder::Sent => "sent",
            Folder::Blocked => "blocked",
            Folder::All => "all",
        }
    }
}

#[derive(Args, Debug)]
pub struct SearchArgs {
    /// Words to search for (prefix matching; all words must match)
    #[arg(required = true, num_args = 1..)]
    pub query: Vec<String>,
    #[arg(long, short = 'n', default_value_t = 25)]
    pub limit: u32,
}

#[derive(Subcommand, Debug)]
pub enum ThreadCommand {
    /// Show every message in a thread (does not mark it read unless --mark-read)
    Read {
        /// Thread ID (t_…)
        id: String,
        /// Output the original HTML bodies instead of plain text
        #[arg(long)]
        html: bool,
        /// Also mark the thread as read
        #[arg(long)]
        mark_read: bool,
    },
    /// Move threads to the Archive
    Archive {
        #[arg(required = true, num_args = 1..)]
        ids: Vec<String>,
    },
    /// Move threads back to the Inbox
    Unarchive {
        #[arg(required = true, num_args = 1..)]
        ids: Vec<String>,
    },
    /// Mark threads as unread
    Unread {
        #[arg(required = true, num_args = 1..)]
        ids: Vec<String>,
    },
    /// Mark threads as read
    Markread {
        #[arg(required = true, num_args = 1..)]
        ids: Vec<String>,
    },
    /// Permanently delete threads, their messages and attachments
    Delete {
        #[arg(required = true, num_args = 1..)]
        ids: Vec<String>,
        /// Confirm without prompting (required when not on a terminal)
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Args, Debug)]
pub struct ScreenerArgs {
    #[command(subcommand)]
    pub command: Option<ScreenerCommand>,
}

#[derive(Subcommand, Debug)]
pub enum ScreenerCommand {
    /// List senders waiting to be screened (the default)
    List,
    /// Screen senders in: their mail moves to the Inbox, future mail goes straight there
    Approve {
        #[arg(required = true, num_args = 1..)]
        emails: Vec<String>,
    },
    /// Screen senders out: their mail is hidden in Blocked (undo with approve)
    Block {
        #[arg(required = true, num_args = 1..)]
        emails: Vec<String>,
    },
}

#[derive(Args, Debug)]
pub struct SendersArgs {
    #[arg(long, value_enum, default_value_t = SenderStatus::Approved)]
    pub status: SenderStatus,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum SenderStatus {
    Approved,
    Blocked,
}

impl SenderStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SenderStatus::Approved => "approved",
            SenderStatus::Blocked => "blocked",
        }
    }
}

#[derive(Args, Debug, Clone, Default)]
pub struct BodyArgs {
    /// Message body (plain text); use - to read it from stdin
    #[arg(long, short = 'm')]
    pub message: Option<String>,
    /// Read the message body from a file (- for stdin)
    #[arg(long, conflicts_with = "message")]
    pub message_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct ComposeArgs {
    /// Recipient(s); repeat or comma-separate. "Name <a@b.com>" works
    #[arg(long, required = true, num_args = 1..)]
    pub to: Vec<String>,
    #[arg(long, num_args = 1..)]
    pub cc: Vec<String>,
    #[arg(long, num_args = 1..)]
    pub bcc: Vec<String>,
    /// One of your mailboxes (see `cloudmail mailbox list`); defaults to the first
    #[arg(long)]
    pub from: Option<String>,
    #[arg(long, short = 's', required = true)]
    pub subject: String,
    #[command(flatten)]
    pub body: BodyArgs,
    /// Show the request that would be sent, without sending
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct ReplyArgs {
    /// Thread ID (t_…)
    pub thread_id: String,
    /// Reply to everyone on the latest message (your own addresses are left out)
    #[arg(long, short = 'a')]
    pub all: bool,
    /// Send from this mailbox instead of the one the thread was addressed to
    #[arg(long)]
    pub from: Option<String>,
    #[command(flatten)]
    pub body: BodyArgs,
    /// Don't quote the original message
    #[arg(long)]
    pub no_quote: bool,
    /// Show the request that would be sent, without sending
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Subcommand, Debug)]
pub enum AttachmentCommand {
    /// List the attachments in a thread
    List {
        /// Thread ID (t_…)
        thread_id: String,
    },
    /// Download an attachment
    Save {
        /// Attachment ID (a_…)
        id: String,
        /// Output file or directory (- for stdout); defaults to the attachment's name in the current directory
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
    },
}

#[derive(Args, Debug)]
pub struct RawArgs {
    /// Message ID (m_…)
    pub id: String,
    /// Write to a file instead of stdout
    #[arg(long, short = 'o')]
    pub output: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct WatchArgs {
    /// Seconds between checks
    #[arg(long, default_value_t = 30)]
    pub interval: u64,
    #[arg(long, value_enum, default_value_t = WatchFolder::All)]
    pub folder: WatchFolder,
    /// Also emit threads with activity after this time (Unix ms); default: only new activity
    #[arg(long)]
    pub since: Option<i64>,
    /// Stop after this many checks (default: run until interrupted)
    #[arg(long)]
    pub max_polls: Option<u64>,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum WatchFolder {
    Inbox,
    Screener,
    All,
}

impl WatchFolder {
    pub fn as_str(self) -> &'static str {
        match self {
            WatchFolder::Inbox => "inbox",
            WatchFolder::Screener => "screener",
            WatchFolder::All => "all",
        }
    }
}

#[derive(Args, Debug, Clone, Default)]
pub struct RouteArgs {
    /// Also point this address's Cloudflare Email Routing rule at the worker (runs wrangler)
    #[arg(long)]
    pub route: bool,
    /// Replace an existing routing rule that sends this address elsewhere (with --route)
    #[arg(long, requires = "route")]
    pub take_over_route: bool,
    /// Worker directory holding wrangler.jsonc (with --route; default ./worker)
    #[arg(long)]
    pub worker_dir: Option<PathBuf>,
    /// Worker name (with --route; default: "name" from wrangler.jsonc)
    #[arg(long)]
    pub worker_name: Option<String>,
    /// Command used to run wrangler
    #[arg(long, default_value = "npx wrangler")]
    pub wrangler: String,
}

#[derive(Subcommand, Debug)]
pub enum MailboxCommand {
    /// List your mailboxes in display order (the first is the default From)
    List,
    /// Add a mailbox (an address the worker receives and can send from)
    Add {
        email: String,
        /// Display name used when sending
        #[arg(long)]
        name: Option<String>,
        /// Deliver unknown senders straight to the Inbox (for support@-style addresses)
        #[arg(long)]
        direct: bool,
        #[command(flatten)]
        route: RouteArgs,
    },
    /// Change a mailbox's name, screening or position
    Set {
        email: String,
        #[arg(long)]
        name: Option<String>,
        /// Send first-time senders to the Screener (true) or straight to the Inbox (false)
        #[arg(long)]
        screen: Option<bool>,
        /// Display position; 0 makes it the default From
        #[arg(long)]
        position: Option<i64>,
    },
    /// Remove a mailbox (mail already received is kept; the routing rule is left alone)
    Remove {
        email: String,
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum SettingsCommand {
    /// Show worker settings
    Get,
    /// Change a worker setting
    Set {
        #[arg(value_enum)]
        key: SettingKey,
        /// New value ("" turns forwarding off)
        value: String,
    },
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum SettingKey {
    /// Verified address that gets a copy of every message
    ForwardTo,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    /// Show the effective configuration (token redacted)
    Show {
        /// Include the API token
        #[arg(long)]
        show_token: bool,
    },
    /// Set a key in the config file
    Set {
        #[arg(value_enum)]
        key: ConfigKey,
        value: String,
    },
    /// Print the config file path
    Path,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum ConfigKey {
    ApiUrl,
    ApiToken,
    PollSeconds,
}

#[derive(Args, Debug)]
pub struct SetupArgs {
    /// Worker directory (contains wrangler.template.jsonc); default ./worker
    #[arg(long)]
    pub worker_dir: Option<PathBuf>,
    /// Name for the worker, D1 database and R2 bucket
    #[arg(long, default_value = "cloudmail")]
    pub name: String,
    /// Mailbox to create and route, as address or address:direct (repeatable)
    #[arg(long = "mailbox", value_name = "ADDRESS[:direct]")]
    pub mailboxes: Vec<String>,
    /// Replace existing routing rules that send these addresses elsewhere
    #[arg(long)]
    pub take_over_routes: bool,
    /// Turn on Email Routing for domains that don't have it (changes their MX records!)
    #[arg(long)]
    pub enable_routing: bool,
    /// Verified address to receive a copy of every message
    #[arg(long)]
    pub forward_to: Option<String>,
    /// Overwrite an existing wrangler.jsonc and config file and rotate the API token
    #[arg(long)]
    pub force: bool,
    /// Print the plan without running anything
    #[arg(long)]
    pub dry_run: bool,
    /// Command used to run wrangler
    #[arg(long, default_value = "npx wrangler")]
    pub wrangler: String,
}
