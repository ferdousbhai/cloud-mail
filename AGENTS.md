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
| `crates/cloudmail` | the CLI: clap definitions (`cli.rs`), mail commands (`mail.rs`), `setup` on Cloudflare (`setup.rs`), mailboxes, settings, `user` and `connect` (`admin.rs`), JSON envelope, exit codes and error kinds (`output.rs`), `agent-guide`, `commands` and the built-in skill (`docs.rs`). Tests in `tests/cli.rs` with fakes (`tests/fake-hey`, `tests/fake-gws`, `tests/support`) |
| `crates/cloudmail-gtk` | GTK 4 + WebKitGTK 6.0 app (`src/ui/`; the Accounts window in `accounts.rs`, the admin's People window in `people.rs`); needs Rust 1.95. Installed as `/usr/lib/cloudmail/cloudmail-gtk` and started by `cloudmail open` (`crates/cloudmail/src/app.rs`), so the CLI links no GTK (`/usr/bin/cloudmail-gtk` is `data/cloudmail-gtk`, a script for the old command that runs `cloudmail open`); to the desktop it is `cloudmail` (Wayland app id, set by `omui::name_window` from the prgname; `data/cloudmail.desktop`, icon), while its D-Bus id stays `com.ferdousbhai.Cloudmail` (GApplication needs a dot). Its look, icons, toasts, dialogs, keys, file chooser and language are [omui](https://github.com/ferdousbhai/omui)'s, a git dependency pinned by revision; `src/style.css` holds only what is Cloudmail's own, in omui's `@om_*` colours and `--om-radius` corners |
| `worker/` | the Worker (TypeScript, `cf` CLI): `src/api.ts` (bearer-token JSON API under `/api/*`, plus `/health`), `src/ingest.ts` (incoming email), `src/store.ts`, `src/mailboxes.ts`, `src/users.ts` (tokens, invites; the `API_TOKEN` secret is the admin, user `owner`). D1 schema in `migrations/`; tests in `test/` (bun) |
| `skills/cloudmail/SKILL.md` | the agent skill; compiled into the CLI (`include_str!` in `docs.rs`) and printed by `cloudmail skill`; `skill install|remove` and the startup refresh live in `src/agent_skill.rs`, shared verbatim with our other product CLIs |
| `bin/` | `check` (every check), release and package scripts |
| `packaging/` | Arch repo and AUR packaging, with their tests |

## Commands

```sh
bin/check                                   # everything below, plus the app's QA and packaging tests
cargo fmt --all -- --check
cargo clippy --locked -p cloudmail-api -p cloudmail --all-targets -- -D warnings   # add -p cloudmail-gtk with gtk4 + webkitgtk-6.0
cargo test --locked -p cloudmail-api -p cloudmail
cd worker && bun install --frozen-lockfile && bun test && bun run typecheck
```

`git config core.hooksPath .githooks` makes every branch push run `bin/check` first, and GitHub runs it on every
push too (`.github/workflows/check.yml`, in an Arch container). A missing GTK 4 or WebKitGTK 6.0 fails `bin/check`
rather than skipping the app; `CLOUDMAIL_CHECK_NO_GTK=1` leaves the app out on purpose. GitHub also builds release
packages and CLI archives.

Changes land on `main` directly: once `bin/check` passes, rebase onto `main` and push the commits there as a
fast-forward. Don't open a pull request for the maintainer's own work; those are for outside contributors.

## Conventions

- Rust edition 2024, MSRV 1.89 (CLI and library); rustfmt `max_width = 120`, dense long lines. Keep
  it that way.
- Every CLI command prints one JSON envelope `{ok, data, summary, breadcrumbs, meta}` when piped
  (`output.rs`), never prompts off a terminal (destructive commands need `--yes`), and fails with exit 1 and
  `{ok:false, error:{code, kind, message, hint}}` on stderr (Omarchy's contract: 0 or 1, errors on stderr; the
  failure's class is `kind`, never the exit code). `cloudmail commands` is generated from clap;
  every command needs an entry in `EXAMPLES` in `docs.rs` (a test enforces it).
- A new linked provider implements `Provider`, prefixes its IDs with its account name and is added to
  `provider::open`.
- The worker is optional: `Mail.client` is `None` with linked accounts only (`Config::has_worker`).
  Use `Mail::worker()` (errors with what's needed) for worker-only work.
- Gmail's and iCloud Mail's Screener decisions live in the accounts (`screening.rs`: Gmail filters
  marked by hidden labels, iCloud rules named `Cloudmail Screener: …`), read with
  `Provider::screen_lists` and changed with `update_screen_lists` (one read, at most one write);
  never touch filters or rules that aren't Cloudmail's. `unified.rs` records a decision in every screened account and the worker.
- One Google sign-in serves Cloudmail, Cloud Calendar and Cloud Contacts: same OAuth client, the browser sign-in
  asks for all three apps' scopes (`gmail::SIGN_IN_SCOPES`, the same list in each app), and an account that isn't
  signed in first copies a working sign-in from the other two apps' gws directories (`Gmail::adopt`). A copy,
  never a shared directory.
- The worker's committed lockfile is `bun.lock` (`package-lock.json` is ignored).
- Every worker query that touches mail, senders, mailboxes or settings is scoped by `user_id`; a new
  endpoint or query must be too (`worker/test/users.test.ts` checks isolation). Only the admin runs
  `setup`, manages users and adds or routes mailboxes; docs and messages for "not configured" offer
  `cloudmail connect <invite link>` before `setup`, and linking accounts as the no-worker option.

## Gotchas

- Tests never touch real accounts or the session bus: `CLOUDMAIL_HEY_COMMAND` /
  `CLOUDMAIL_GWS_COMMAND` point at the fakes, and tests needing D-Bus start a private
  `dbus-daemon` (the `dbus` package).
- Every string the app shows goes through `omui::tr` / `tr!` / `trn!` (or a wrapper the catalog test in `main.rs` is
  told of) and is listed in `src/i18n/en.json`; the test fails on one that is missing or left over. Text built from
  translated strings and kept (a status line, an error) is written again after a switch of language: `ui::Live` for
  a label, `omui::i18n::bind`/`on_change` otherwise. Dates and sizes shown in the window come from `src/format.rs`,
  never `cloudmail_api::text`, which is the CLI's fixed English. The CLI stays English.
- Work that sends or changes something runs through `util::write` (`omui::background`, which holds the app until it
  is done); reads through `util::run`, which does not hold it.
- Never run the app on your own desktop to look at a change: `crates/cloudmail-gtk/dev/preview.sh <png> [theme-dir]`
  runs a debug build in a nested Hyprland with a private home and session bus against `dev/fake-worker`'s made-up
  mail, and `CLOUDMAIL_SCRIPT` (`src/ui/devscript.rs`) puts it in a scene: for themes and the compositor's picture.
- The app's flows are checked by omui's scripted QA: `dev/qa.sh <app built with --features qa> dev/qa/*.jsonl` runs
  them against the same made-up mail on a hidden broadway display (bin/check does), and any GLib warning fails it.
  Release builds and the PKGBUILD's `check()` leave the feature off (`check()` runs the app's unit tests only).
  A `<script>.state` directory beside a script is the state directory the app starts with, and `<script>.state-after`
  lists the files that must be left in it. A QA build carries the few Danish strings of `dev/qa/da.json`, which omui's
  `{"do":"language","code":"da"}` switches to, and a script answers the file chooser with omui's `picker` (a failure,
  or `attachment.txt`, which dev/qa.sh copies to where the app runs).
- `worker/install.json` (written by `cloudmail setup`) holds a user's names and IDs and isn't
  committed; `worker/cloudflare.config.ts` reads it.
- Inbound mail that can't be parsed or stored is never bounced; it's kept in R2 under `failed/`.
- Editing `skills/cloudmail/SKILL.md` changes the CLI's output; rebuild and check `cloudmail skill`.

## Cloudflare CLI

Use the `cf` CLI alone for Cloudflare work in this repository: deploys, builds and
their logs, secrets, D1, R2, Tunnels and network routes. No Wrangler, as a command,
a config file or a dependency. The one trace left is `cloudmail setup` reading the
names out of the `wrangler.jsonc` that a setup from before `cf` generated. Find
commands with `cf cli search "<what you want to do>"`, then check `<command> --help`.
