# Contributing to Cloudmail

Thanks for taking a look. Issues and pull requests are welcome at
[github.com/ferdousbhai/cloud-mail](https://github.com/ferdousbhai/cloud-mail).

## Development

You need Rust 1.88+ (1.92+ for `cloudmail-gtk`), Bun or Node.js, and for the desktop app GTK 4 and
WebKitGTK 6.0.

```sh
cd worker && bun install
bun run db:local                     # schema for the local database (wrangler, see wrangler.local.jsonc)
printf 'API_TOKEN=dev-token\n' > .dev.vars
bun run dev --port 8799              # cf dev --mode development: local resources only
curl -X POST 'localhost:8799/cdn-cgi/handler/email?from=a@example.com&to=hi@example.com' --data-binary @some.eml
bun test && bunx tsc --noEmit

cargo test --workspace && cargo clippy --workspace --all-targets
```

## The worker

The worker deploys with Cloudflare's `cf` CLI from `worker/cloudflare.config.ts`, which reads the
install's names and IDs from `worker/install.json`. `cloudmail setup` writes that file and it is not
committed (an install made before cf has a generated `wrangler.jsonc` instead; setup carries its
names over). `cf` still builds the worker through wrangler, which is why both are dev dependencies.
Mail that can't be parsed or stored is never bounced: the raw message is kept in R2 under `failed/`.

Packages install the worker source under `/usr/share/cloudmail/worker`. `cloudmail setup` copies it
to `~/.local/share/cloudmail/worker` and deploys from there.

## Linked accounts

Linked accounts live in `crates/cloudmail-api`: `provider.rs` is the `Provider` trait (your worker's
`Client` implements it too), `hey.rs` maps `hey … --json` into it, `gmail.rs` maps raw Gmail API calls
through `gws gmail users … --params '<json>'`, and `unified.rs` merges providers, turns their failures
into warnings and hides forwarded copies. A new provider implements `Provider`, prefixes its IDs with
its account name and is added to `provider::open`; the config entry is `[accounts.<name>] provider =
"…"`. Tests and headless runs never touch a real account: `CLOUDMAIL_HEY_COMMAND=crates/cloudmail/tests/fake-hey`
and `CLOUDMAIL_GWS_COMMAND=crates/cloudmail/tests/fake-gws` answer with synthetic data
(`FAKE_HEY_MODE=logged_out|crash|garbage`, `FAKE_GWS_MODE=expired|revoked|offline|crash|garbage`).

## Google sign-in for Gmail

Cloudmail's Google sign-in is one OAuth client, `GOOGLE_CLIENT_ID`/`GOOGLE_CLIENT_SECRET` in
`crates/cloudmail-api/src/gmail.rs` (a desktop client's secret isn't secret), and builds ship with it.
A build or a user can use its own client instead with
`CLOUDMAIL_GOOGLE_CLIENT_ID`/`CLOUDMAIL_GOOGLE_CLIENT_SECRET` or `--client-id`/`--client-secret`; if
the built-in values are ever blanked out and none is given, `account add gmail` says the sign-in isn't
configured. To make a client: a Google Cloud project with the Gmail API enabled, an OAuth consent
screen (External, published "In production", since test-mode sign-ins expire after 7 days) with the
`https://www.googleapis.com/auth/gmail.modify` scope, and an OAuth client of type "Desktop app".
`gmail.modify` is a restricted scope: until Google verifies the app, users see the unverified-app
screen and at most 100 people can sign in.

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
container, and then moves the omarchy-pkgs pull request to the new version (while it is open).
A desktop notification reports the outcome; the log is in
`~/.local/state/cloudmail/release-<version>.log`. `bin/release <version>` still works by hand,
and `CLOUDMAIL_NO_AUTO_RELEASE=1 git push …` pushes a tag without releasing it.

When the release is published, the `release-binaries` GitHub Actions workflow builds the `cloudmail`
CLI for Linux (x86_64 and aarch64, static musl) and macOS (Apple Silicon and Intel) and attaches the
archives and a `SHA256SUMS` file to it. Run it by hand from the Actions tab, giving a tag, to
backfill an older release.
