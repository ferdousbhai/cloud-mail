# Cloudmail

Your own email service on your own domains, running entirely in your Cloudflare account, with a
HEY-style **Screener**, a fast native Linux app, and a CLI that AI agents can drive without any
special setup.

- **Receive** with Cloudflare Email Routing, **send** with Cloudflare Email Service. No mail server,
  no IMAP, nothing to patch.
- **Your data stays in your account**: messages in D1 (with full-text search), raw `.eml` files and
  attachments in R2.
- **The Screener**: the first email from someone new waits for a yes or no. Yes, and their mail goes
  to your Inbox from then on. No, and you never hear from them again. People you email are screened
  in automatically. A message its sender's domain didn't authenticate (DMARC, or aligned DKIM/SPF
  when the domain has no DMARC policy) can't ride on an approval.
- **Several domains, one inbox.** Personal addresses are screened; role addresses like `support@`
  deliver straight to the Inbox. Replies go out from the address the mail was sent to.
- **Inbox and Archive**, that's it. A reply on an archived thread brings it back.
- **Privacy by default**: remote images and tracking pixels stay blocked until you ask for them.

```
sender ─SMTP─▶ Email Routing ─▶ Worker email() ─▶ D1 (threads, messages, search) + R2 (raw mail, attachments)
cloudmail / cloudmail-gtk ─HTTPS + token─▶ Worker /api/* ─▶ Email Service (outgoing mail)
```

| Path | What |
|---|---|
| `worker/` | Cloudflare Worker (TypeScript): inbound handler, Screener, JSON API |
| `crates/cloudmail` | `cloudmail` CLI |
| `crates/cloudmail-gtk` | `cloudmail-gtk` desktop app (GTK4 + WebKitGTK), themed from Omarchy if present |
| `crates/cloudmail-api` | Rust API client shared by both |
| `docs/API.md` | HTTP API reference |

## Requirements

- A Cloudflare account on the Workers Paid plan (needed to send to arbitrary recipients; about
  $5/month, with 3,000 emails/month included), with your domains' DNS on Cloudflare.
- Node.js (for `wrangler`) and Rust. The desktop app also needs GTK 4 and WebKitGTK 6.0
  (Arch: `pacman -S gtk4 webkitgtk-6.0`).

## Install

On Omarchy or any Arch Linux, from the signed package repository (updates then arrive with
`omarchy update`):

```sh
curl -fsSL https://ferdousbhai.com/cloudmail/install.sh | sudo bash
```

It installs `cloudmail` (alias `cmail`), `cloudmail-gtk`, and the worker used by `cloudmail setup`.
Cloudmail is also on its way into Omarchy's own repository and *Install › Service* menu
([omarchy-pkgs#724](https://github.com/omacom/omarchy-pkgs/pull/724),
[omarchy#13763](https://github.com/omacom/omarchy/pull/13763)).

From source:

```sh
git clone https://github.com/ferdousbhai/cloud-mail && cd cloud-mail
./install.sh                    # installs cloudmail (alias: cmail) + cloudmail-gtk to ~/.local/bin
# CLOUDMAIL_NO_GTK=1 ./install.sh   # CLI only
npx wrangler login
```

## Set up your instance

`cloudmail setup` creates the D1 database and R2 bucket, deploys the Worker, sets the API token, writes
`~/.config/cloudmail/config.toml`, and for each mailbox enables sending on its domain and adds an Email
Routing rule. Every step is idempotent. Start with a dry run:

```sh
cloudmail setup --dry-run --mailbox hi@example.com --mailbox support@example.com:direct
cloudmail setup           --mailbox hi@example.com --mailbox support@example.com:direct
```

- `:direct` skips the Screener for that address (good for `support@`, `legal@`, …).
- `--forward-to me@gmail.com` keeps a copy of everything going to your old inbox while you switch
  (it must be a verified Email Routing destination). Turn it off later with
  `cloudmail settings set forward-to ""`.
- Existing routing rules that send an address elsewhere are left alone unless you pass
  `--take-over-routes`. Domains without Email Routing are skipped unless you pass
  `--enable-routing`, because that replaces their MX records.
- Enabling sending adds SPF/DKIM records and a `p=reject` DMARC record to the domain. If another
  service sends mail as that domain, set it up with SPF/DKIM first.

Add addresses later with `cloudmail mailbox add sales@example.com --direct --route`.

## Use it

```sh
cloudmail-gtk                 # or "Cloudmail" in your app launcher
cloudmail                     # orientation and config status
cloudmail screener            # who's waiting
cloudmail screener approve alice@example.com
cloudmail inbox --unread
cloudmail thread read <id>
cloudmail reply <id> -m "Sounds good."
cloudmail watch --folder all  # stream new mail as JSON lines
```

Desktop keys: `j`/`k` move, `Enter` open, `e` archive, `r` reply, `a` reply all, `c` compose,
`y`/`n` in the Screener, `/` search, `L` load remote images, `?` all keys. To open `mailto:` links
in Cloudmail: `xdg-mime default com.ferdousbhai.Cloudmail.desktop x-scheme-handler/mailto`.

## For AI agents

The CLI is its own documentation. When output is piped, every command prints a JSON envelope
(`ok`, `data`, `summary`, `breadcrumbs` suggesting next commands) with meaningful exit codes, and
nothing ever prompts.

```sh
cloudmail agent-guide         # concepts, output format, exit codes, workflows
cloudmail commands --json     # every command, flag and example
```

## Releasing

`bin/release <version>` builds the tagged release with makepkg, signs the package and the
`[cloudmail]` repository database with the package-signing key (gpg asks for its passphrase),
attaches them to the GitHub release with `install.sh`, and then `bin/verify-release` installs it
with the public one-liner in a clean Arch container.

## Development

```sh
cd worker && bun install
bunx wrangler d1 migrations apply cloudmail --local
printf 'API_TOKEN=dev-token\n' > .dev.vars
bunx wrangler dev --port 8799
curl -X POST 'localhost:8799/cdn-cgi/handler/email?from=a@example.com&to=hi@example.com' --data-binary @some.eml
bun test && bunx tsc --noEmit

cargo test --workspace && cargo clippy --workspace --all-targets
```

`worker/wrangler.jsonc` is generated by `cloudmail setup` from `wrangler.template.jsonc` and is not
committed. Mail that can't be parsed or stored is never bounced: the raw message is kept in R2
under `failed/`.

## Security notes

- The API is protected by a single bearer token: treat `~/.config/cloudmail/config.toml` like a password.
- Message HTML is untrusted. The desktop app renders it with JavaScript disabled, remote loads
  blocked and links opened in your browser.
- Screening trusts the `From` address only when its domain authenticated the message: DMARC, or,
  for domains without a DMARC policy, DKIM or SPF aligned with the From domain (the same test DMARC
  applies). Mail from a domain with no working SPF or DKIM therefore waits in the Screener each time.

## License

MIT
