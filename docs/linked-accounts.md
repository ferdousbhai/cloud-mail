# Linked accounts: HEY and Gmail

Optional. Until you add an account, nothing changes. Back to the [README](../README.md).

## HEY

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

## Gmail

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
one line says what's wrong. When a sign-in has expired or been revoked (Gmail or HEY), the app shows a
**Sign in** button next to that line and sends one desktop notification; from a terminal,
`cloudmail account login gmail` (or `hey`) signs in again. Each listing reads Gmail's threads one `gws` run at a time (a
few in parallel, 100 at most per list, and only changed threads again), so the first Gmail load
takes a moment; your own mail doesn't wait for it.
