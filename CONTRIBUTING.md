# Contributing to Cloudmail

Thanks for taking a look. Issues and pull requests are welcome at
[github.com/ferdousbhai/cloud-mail](https://github.com/ferdousbhai/cloud-mail).

## Development

You need Rust 1.89+ (1.95+ for `cloudmail-gtk`), Bun or Node.js, and for the desktop app GTK 4 and
WebKitGTK 6.0.

```sh
cd worker && bun install
bun run db:local                     # schema for the local database (wrangler, see wrangler.local.jsonc)
printf 'API_TOKEN=dev-token\n' > .dev.vars
bun run dev --port 8799              # cf dev --mode development: local resources only
curl -X POST 'localhost:8799/cdn-cgi/handler/email?from=a@example.com&to=hi@example.com' --data-binary @some.eml
bun test && bunx tsc --noEmit

bin/check                            # everything: rustfmt, clippy, Rust tests, worker tests and types, packaging
```

Checks run here, not on GitHub: with `git config core.hooksPath .githooks`, every push of a branch
runs `bin/check` first and stops if it fails (`CLOUDMAIL_NO_CHECK=1` skips it). GitHub only builds
releases: the packages on a version tag, and the CLI archives once a release is published.

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
through `gws gmail users … --params '<json>'`, `icloud.rs` maps the requests icloud.com's own Mail
app sends (its doc comment says what each is and where that is established) with the session from
`session.rs` (icloud-session's D-Bus interface), `accounts.rs` links and unlinks accounts for the CLI
and the app, `keyring.rs` keeps secrets in the Secret Service, and `unified.rs` merges providers,
screens Gmail and iCloud Mail with your worker's decisions, turns their failures into warnings and
hides forwarded copies. A new provider implements `Provider`, prefixes its IDs with
its account name and is added to `provider::open`; the config entry is `[accounts.<name>] provider =
"…"`. Tests and headless runs never touch a real account: `CLOUDMAIL_HEY_COMMAND=crates/cloudmail/tests/fake-hey`
and `CLOUDMAIL_GWS_COMMAND=crates/cloudmail/tests/fake-gws` answer with synthetic data
(`FAKE_HEY_MODE=logged_out|crash|garbage`, `FAKE_GWS_MODE=expired|revoked|offline|crash|garbage`).
The CLI tests never reach the real session bus: each that needs one starts a private `dbus-daemon`
(the `dbus` package) with stand-ins for the Secret Service and icloud-session, and iCloud Mail's
web services as an in-process HTTP server (`crates/cloudmail/tests/support`); every other test runs
with no session bus at all.

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
It drafts the GitHub release from the tag's message, then runs `bin/release`. The native
package workflow, which runs only for version tags, builds and tests unsigned x86_64 and aarch64 packages. The release command
waits for that exact commit's successful CI run, verifies each artifact's source commit and
package metadata, and signs the packages and both repository databases locally. The private
signing key never goes to GitHub. `[cloudmail]` serves x86_64; `[cloudmail-aarch64]` serves ARM,
and the installer chooses the native repository automatically.

After publication, GitHub verifies the exact release on native Arch and Arch Linux ARM
runners. Failed verification reports an error without deleting a release or tag. Run
`bin/verify-release <version> <architecture>` manually on a matching native host; ARM requires
`ARM_BUILD_IMAGE` pointing to an Arch Linux ARM Docker image. `bin/build-package <architecture>`
builds an unsigned package locally with the same native-container process used by CI.

The release hook then moves the omarchy-pkgs pull request to the new version (while open).
A desktop notification reports the outcome; the log is in
`~/.local/state/cloudmail/release-<version>.log`. `bin/release <version> [CI-run-id]` also works
by hand, and `CLOUDMAIL_NO_AUTO_RELEASE=1 git push …` pushes a tag without releasing it.

When the release is published, the `release-binaries` GitHub Actions workflow builds the `cloudmail`
CLI for Linux (x86_64 and aarch64, static musl) and attaches the archives and a `SHA256SUMS` file
to it. Run it by hand from the Actions tab, giving a tag, to backfill an older release.
