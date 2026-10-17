# cloud-mail

Cloudmail: a Cloudflare Worker that its admin deploys to their own account (`cloudmail setup`) to
receive and send mail for their domains, and can share with a few invited users (`cloudmail user
invite` / `cloudmail connect`; each sees only their own mail), the `cloudmail` CLI (alias `cmail`) and
a GTK app that read it, with HEY, Gmail and iCloud Mail linked alongside (or on their own, without a
worker). User docs: README.md; worker
HTTP API: docs/API.md; linked accounts: docs/linked-accounts.md; development and releases:
CONTRIBUTING.md.

## Layout

| path | what |
|---|---|
| `crates/cloudmail-api` | library shared by CLI and app: worker `Client` (`client.rs`), config (`config.rs`), the `Provider` trait (`provider.rs`) and its HEY, Gmail and iCloud Mail implementations (`hey.rs`, `gmail.rs`, `icloud.rs`, `session.rs`), merging and screening across providers (`unified.rs`), the Screener's lists kept in Gmail filters and iCloud rules (`screening.rs`), linking (`accounts.rs`), Secret Service (`keyring.rs`) |
| `crates/cloudmail` | the CLI: clap definitions (`cli.rs`), mail commands (`mail.rs`), `setup` on Cloudflare (`setup.rs`), mailboxes, settings, `user` and `connect` (`admin.rs`), JSON envelope and exit codes (`output.rs`), `agent-guide`, `commands` and the built-in skill (`docs.rs`). Tests in `tests/cli.rs` with fakes (`tests/fake-hey`, `tests/fake-gws`, `tests/support`) |
| `crates/cloudmail-gtk` | GTK 4 + WebKitGTK 6.0 app (`src/ui/`; the Accounts window in `accounts.rs`, the admin's People window in `people.rs`); needs Rust 1.95 |
| `worker/` | the Worker (TypeScript, `cf` CLI): `src/api.ts` (bearer-token JSON API under `/api/*`, plus `/health`), `src/ingest.ts` (incoming email), `src/store.ts`, `src/mailboxes.ts`, `src/users.ts` (tokens, invites; the `API_TOKEN` secret is the admin, user `owner`). D1 schema in `migrations/`; tests in `test/` (bun) |
| `skills/cloudmail/SKILL.md` | the agent skill; compiled into the CLI (`include_str!` in `docs.rs`) and printed by `cloudmail skill`; `skill install|remove` and the startup refresh live in `src/agent_skill.rs`, shared verbatim with our other product CLIs |
| `bin/` | `check` (every check), release and package scripts |
| `packaging/` | Arch repo and AUR packaging, with their tests |

## Commands

```sh
bin/check                                   # everything below, plus packaging tests
cargo fmt --all -- --check
cargo clippy --locked -p cloudmail-api -p cloudmail --all-targets -- -D warnings   # add -p cloudmail-gtk with gtk4 + webkitgtk-6.0
cargo test --locked -p cloudmail-api -p cloudmail
cd worker && bun install --frozen-lockfile && bun test && bun run typecheck
```

Checks run locally, not on GitHub: `git config core.hooksPath .githooks` makes every branch push run
`bin/check`. GitHub only builds release packages and CLI archives.

## Conventions

- Rust edition 2024, MSRV 1.89 (CLI and library); rustfmt `max_width = 120`, dense long lines. Keep
  it that way.
- Every CLI command prints one JSON envelope `{ok, data, summary, breadcrumbs, meta}` when piped
  (`output.rs`), never prompts off a terminal (destructive commands need `--yes`), and fails with
  `{ok:false, error:{code, message, hint}}` with the exit codes in `output.rs`. `cloudmail commands`
  is generated from clap; every command needs an entry in `EXAMPLES` in `docs.rs` (a test enforces
  it).
- A new linked provider implements `Provider`, prefixes its IDs with its account name and is added to
  `provider::open`.
- The worker is optional: `Mail.client` is `None` with linked accounts only (`Config::has_worker`).
  Use `Mail::worker()` (errors with what's needed) for worker-only work.
- Gmail's and iCloud Mail's Screener decisions live in the accounts (`screening.rs`: Gmail filters
  marked by hidden labels, iCloud rules named `Cloudmail Screener: …`), read with
  `Provider::screen_lists` and changed with `update_screen_lists` (one read, at most one write);
  never touch filters or rules that aren't Cloudmail's. `unified.rs` records a decision in every screened account and the worker.
- The worker's committed lockfile is `bun.lock` (`package-lock.json` is ignored).
- Every worker query that touches mail, senders, mailboxes or settings is scoped by `user_id`; a new
  endpoint or query must be too (`worker/test/users.test.ts` checks isolation). Only the admin runs
  `setup`, manages users and adds or routes mailboxes; docs and messages for "not configured" offer
  `cloudmail connect <invite link>` before `setup`, and linking accounts as the no-worker option.

## Gotchas

- Tests never touch real accounts or the session bus: `CLOUDMAIL_HEY_COMMAND` /
  `CLOUDMAIL_GWS_COMMAND` point at the fakes, and tests needing D-Bus start a private
  `dbus-daemon` (the `dbus` package).
- `worker/install.json` (written by `cloudmail setup`) holds a user's names and IDs and isn't
  committed; `worker/cloudflare.config.ts` reads it.
- Inbound mail that can't be parsed or stored is never bounced; it's kept in R2 under `failed/`.
- Editing `skills/cloudmail/SKILL.md` changes the CLI's output; rebuild and check `cloudmail skill`.
- Don't deploy, tag or push from an agent session: pushing a version tag is the release.

## Cloudflare CLI

Use the `cf` CLI for Cloudflare work in this repository: deploys, builds and their
logs, secrets, D1, R2, Tunnels and network routes. Use Wrangler or the dashboard
only where `cf` has no command for the task. Find commands with
`cf cli search "<what you want to do>"`, then check `<command> --help`.
