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
| `crates/cloudmail-api` | Rust API client shared by both, plus linked accounts (HEY through the `hey` CLI, Gmail through Google's `gws` CLI) |
| `docs/API.md` | HTTP API reference |

## Get started

You need a domain whose DNS is on Cloudflare and a Cloudflare account on the Workers Paid plan
(about $5/month, with 3,000 emails/month included; needed to send to anyone).

On Omarchy or any Arch Linux:

```sh
curl -fsSL https://ferdousbhai.com/cloudmail/install.sh | sudo bash
cloudmail setup you@yourdomain.com
```

That's it. `setup` logs you in to Cloudflare in your browser, deploys your mail service to your
account (a Worker, a D1 database and an R2 bucket), turns on receiving and sending for your domain,
and connects the app. Open **Cloudmail** from the app launcher.

- Several addresses and domains: `cloudmail setup you@a.com support@a.com:direct hello@b.com`.
  `:direct` skips the Screener (good for `support@`, `legal@`, …).
- If a domain already receives mail somewhere else (Google Workspace, Fastmail, …), setup tells you
  where and asks before moving it, because that replaces the domain's MX records.
- `--forward-to me@gmail.com` keeps a copy of everything going to your old inbox while you switch.
  Cloudflare emails that address a link to confirm it. Turn it off later with
  `cloudmail settings set forward-to ""`.
- `--dry-run` shows every step first. Running setup again is safe, and is how you update the
  worker after an upgrade.
- Sending adds SPF/DKIM records and a `p=reject` DMARC record to the domain. If another service
  sends mail as that domain, set it up with SPF/DKIM first.

Add addresses later with `cloudmail mailbox add sales@yourdomain.com --route`.

**For agents and scripts**, setup never prompts when output is piped. Give it a Cloudflare API token
instead of the browser login, and `--yes` to allow moving a domain's mail:

```sh
CLOUDFLARE_API_TOKEN=… cloudmail setup you@yourdomain.com --yes
```

Create the token under *My Profile › API Tokens* with these permissions:
**Account**: Workers Scripts, D1, Workers R2 Storage, Email Routing Addresses and Email Sending (all
Edit), Account Settings (Read); **Zone** (your mail domains): Email Routing Rules, Zone Settings and
DNS (Edit), Zone (Read). If setup can't tell which account to use, add `--account <id>` (the Account
ID on your Cloudflare dashboard's home page).


Cloudmail is also on its way into Omarchy's own repository and *Install › Service* menu
([omarchy-pkgs#724](https://github.com/omacom/omarchy-pkgs/pull/724),
[omarchy#13763](https://github.com/omacom/omarchy/pull/13763)).

### From source

Needs Node.js and Rust; the desktop app also needs GTK 4 and WebKitGTK 6.0
(Arch: `pacman -S gtk4 webkitgtk-6.0`).

```sh
git clone https://github.com/ferdousbhai/cloud-mail && cd cloud-mail
./install.sh                    # installs cloudmail (alias: cmail) + cloudmail-gtk to ~/.local/bin
# CLOUDMAIL_NO_GTK=1 ./install.sh   # CLI only
cloudmail setup you@yourdomain.com
```

## Use it

```sh
cloudmail-gtk                 # or "Cloudmail" in your app launcher
cloudmail                     # orientation and config status
cloudmail screener            # who's waiting
cloudmail screener approve alice@example.com
cloudmail inbox --unread
cloudmail thread read <id>
cloudmail reply <id> -m "Sounds good."
cloudmail watch --folder all  # stream new mail (JSON lines when piped)
```

Desktop keys: `j`/`k` move, `Enter` focus the message, `e` archive, `r` reply, `a` reply all, `c` compose,
`y`/`n` in the Screener, `/` search, `L` load remote images, `?` all keys. To open `mailto:` links
in Cloudmail: `xdg-mime default com.ferdousbhai.Cloudmail.desktop x-scheme-handler/mailto`.

## Your HEY mail too (optional)

If you also have a [HEY](https://hey.com) account, Cloudmail can show it next to your own mail, in the
app and the CLI. It's opt-in: until you add it, nothing changes. It uses HEY's official
[`hey` CLI](https://github.com/basecamp/hey-cli), which signs in with one browser login; Cloudmail
never sees a HEY password or token.

```sh
cloudmail account add hey     # checks hey is installed; runs `hey auth login` if you aren't signed in
cloudmail account list        # which accounts are linked and working
cloudmail account remove hey  # unlink (HEY itself is untouched)
```

| In Cloudmail | Your worker | HEY |
|---|---|---|
| Inbox | Inbox | Imbox (unread = unseen) |
| Archive (`e`) | Archive | Paper Trail (HEY has no archive, so archiving moves a thread there) |
| Screener | Screener | The Screener; a sender waiting in both shows once, and a yes/no decides both |
| Sent | Sent | (HEY's CLI has no Sent box) |
| The Feed, Paper Trail, Set Aside, Reply Later | | the HEY boxes, under a HEY heading in the app (keys 5–8) and `cloudmail threads list --folder feed` etc. |
| Search | full-text search | HEY search |

HEY threads carry a small **HEY** tag. Reading, replying (from your HEY address), writing from your
HEY address (pick it in From), marking read/unread, archiving, attachments and Screener decisions
all go through `hey`; HEY's IDs start with `hey:`. Opening an unseen HEY thread in the app marks it
seen in HEY, as opening it in HEY would; `cloudmail thread read` doesn't.

**Forwarding to HEY.** If your worker forwards to your HEY address (`--forward-to you@hey.com`),
every message is in both places. The HEY copy is hidden when it has the same sender and subject and
arrived within 15 minutes of a message in your worker; HEY's CLI doesn't expose Message-IDs, so this
is the most reliable signal it offers. A HEY copy of a thread you've since replied to in Cloudmail,
or one that arrived much later, can still show.

**If HEY is unavailable** (not installed, signed out, offline), your own mail loads as usual and one
line says what's wrong with HEY. The CLI puts it in `meta.warnings` and on stderr.

## Your Gmail too (optional)

Gmail can sit next to your mail the same way, in the app and the CLI, and again nothing changes until
you add it. Cloudmail reaches Gmail through Google's own
[Workspace CLI `gws`](https://github.com/googleworkspace/cli), with one browser sign-in and no Google
Cloud setup of your own: Cloudmail brings its own Google sign-in.

```sh
npm install -g @googleworkspace/cli   # installs `gws`
cloudmail account add gmail           # opens Google's sign-in in your browser, once
cloudmail account list
cloudmail account remove gmail        # unlink and sign Cloudmail out of Gmail on this computer
```

The sign-in asks for Gmail only (read, label, archive and send; nothing else in your Google account).
While Cloudmail's Google app awaits Google's verification, Google shows **"Google hasn't verified
this app"**: choose **Advanced**, then **Go to Cloudmail**. The sign-in is kept in Cloudmail's own
directory (`~/.config/cloudmail/gws/gmail`), apart from any `gws` you use yourself, which it never
reads or changes. A second Gmail account links with `cloudmail account add gmail --name work`.

| In Cloudmail | Your worker | Gmail |
|---|---|---|
| Inbox | Inbox | Inbox (unread = Gmail's unread) |
| Archive (`e`) | Archive | Gmail's archive: the thread leaves the Inbox, `i` brings it back |
| Screener | Screener | (Gmail has no Screener: its mail goes straight to the Inbox) |
| Sent | Sent | Sent |
| Search | full-text search | Gmail search (its own syntax works: `from:ana has:attachment`) |

Gmail threads carry a small **Gmail** tag. Reading, replying (threaded in Gmail, from your Gmail
address), writing from your Gmail address or a verified send-as alias (pick it in From), marking
read/unread, archiving, attachments and `cloudmail raw` all go through `gws`; Gmail's IDs start with
`gmail:`. Gmail's categories and labels aren't shown separately: everything in Gmail's Inbox,
Promotions and Social included, is in the Inbox.

**Forwarding.** If your worker forwards to your Gmail address, or Gmail forwards into your worker,
Gmail's copy is hidden: Gmail gives out Message-IDs, so a copy is matched exactly, message for
message, however long after the original it arrived.

**If Gmail is unavailable** (gws not installed, signed out, offline), your own mail loads as usual and
one line says what's wrong. When Google's sign-in has expired or been revoked, it says to run
`cloudmail account add gmail` again. Each listing reads Gmail's threads one `gws` run at a time (a
few in parallel, 100 at most per list, and only changed threads again), so the first Gmail load
takes a moment; your own mail doesn't wait for it.

## For AI agents

The CLI is its own documentation. When output is piped, every command prints a JSON envelope
(`ok`, `data`, `summary`, `breadcrumbs` suggesting next commands) with meaningful exit codes, and
nothing ever prompts.

```sh
cloudmail agent-guide         # concepts, output format, exit codes, workflows
cloudmail commands --json     # every command, flag and example
```

## Releasing

Bump `version` in `Cargo.toml` and `pkgver` in the PKGBUILD, commit, then push an annotated tag
whose message is the release notes:

```sh
git config core.hooksPath .githooks          # once per clone
git tag -a v0.3.2 -F notes.md --cleanup=verbatim && git push origin main v0.3.2
```

Pushing the tag is the release: the pre-push hook starts `bin/release-on-tag` in the background.
It drafts the GitHub release from the tag's message, then runs `bin/release`, which builds the
package with makepkg, signs it and the `[cloudmail]` repository database with the
package-signing key (gpg asks for its passphrase in a desktop prompt), attaches them with
`install.sh`, and has `bin/verify-release` install it with the public one-liner in a clean Arch
container. A desktop notification reports the outcome; the log is in
`~/.local/state/cloudmail/release-<version>.log`. `bin/release <version>` still works by hand,
and `CLOUDMAIL_NO_AUTO_RELEASE=1 git push …` pushes a tag without releasing it.

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

Linked accounts live in `crates/cloudmail-api`: `provider.rs` is the `Provider` trait (your worker's
`Client` implements it too), `hey.rs` maps `hey … --json` into it, `gmail.rs` maps raw Gmail API calls
through `gws gmail users … --params '<json>'`, and `unified.rs` merges providers, turns their failures
into warnings and hides forwarded copies. A new provider implements `Provider`, prefixes its IDs with
its account name and is added to `provider::open`; the config entry is `[accounts.<name>] provider =
"…"`. Tests and headless runs never touch a real account: `CLOUDMAIL_HEY_COMMAND=crates/cloudmail/tests/fake-hey`
and `CLOUDMAIL_GWS_COMMAND=crates/cloudmail/tests/fake-gws` answer with synthetic data
(`FAKE_HEY_MODE=logged_out|crash|garbage`, `FAKE_GWS_MODE=expired|revoked|offline|crash|garbage`).

Cloudmail's Google sign-in is one OAuth client, `GOOGLE_CLIENT_ID`/`GOOGLE_CLIENT_SECRET` in
`crates/cloudmail-api/src/gmail.rs` (a desktop client's secret isn't secret). Until those are filled
in, `account add gmail` says the sign-in isn't configured; a build can use its own client with
`CLOUDMAIL_GOOGLE_CLIENT_ID`/`CLOUDMAIL_GOOGLE_CLIENT_SECRET` or `--client-id`/`--client-secret`. To
make one: a Google Cloud project with the Gmail API enabled, an OAuth consent screen (External,
published "In production", since test-mode sign-ins expire after 7 days) with the
`https://www.googleapis.com/auth/gmail.modify` scope, and an OAuth client of type "Desktop app".
`gmail.modify` is a restricted scope: until Google verifies the app, users see the unverified-app
screen and at most 100 people can sign in.

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
