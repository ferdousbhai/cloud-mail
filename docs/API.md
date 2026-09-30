# cloudmail API

Base URL: the Worker URL (e.g. `https://cloudmail.<subdomain>.workers.dev`).
Every `/api/*` request needs `Authorization: Bearer <API_TOKEN>`. JSON in, JSON out.
Timestamps are Unix milliseconds. Errors: `{ "error": "message" }` with a 4xx/5xx status.

## Model

Mail is grouped into **threads**. A thread has one `folder`:

- `screener` – first message from a sender you haven't decided on yet
- `inbox`
- `archive`
- `blocked` – from a blocked sender (kept so a mistake can be undone; not shown in normal UI)

Sent mail is stored as messages with `outgoing: true` inside its thread; the "Sent" view is a
query, not a folder. Threads from approved senders land in `inbox`; a new message on an archived
thread moves it back to `inbox`.

Senders are screened by lowercase address, only on mailboxes configured with `screen: true`; mail to
direct mailboxes (e.g. `support@example.org`) from unknown senders goes straight to `inbox`.
A reply only joins an existing thread (by In-Reply-To/References) when its sender is trusted (approved,
or anyone not blocked on a direct mailbox, and not failing DMARC) and the thread is either their own or
already in your Inbox/Archive. A sender still in the Screener keeps adding to their own Screener thread.
Anything else starts a new thread, so no mail hides inside another sender's Screener or Blocked thread.
A message that arrives more than once (one copy per recipient mailbox, or directly and via a list) is
stored once; if a later copy would have gone to the Inbox while the stored one is in the Screener
(e.g. the later copy was sent to a direct mailbox), the thread moves to the Inbox. Blocked
senders are blocked everywhere. Replying to / emailing someone approves them.

## Types

```jsonc
// Address
{ "name": "Stripe", "email": "receipts@stripe.com" }

// ThreadSummary
{
  "id": "t_…",
  "subject": "Your receipt",
  "folder": "inbox",
  "snippet": "Thanks for your payment…",
  "from": Address,          // the other party: sender of the latest incoming message, or who you wrote to
  "to_address": "hi@example.com", // which of your mailboxes it was sent to (reply from this)
  "message_count": 3,
  "unread": true,
  "has_attachments": false,
  "last_at": 1727600000000
}

// Message
{
  "id": "m_…",
  "thread_id": "t_…",
  "outgoing": false,
  "from": Address,
  "to": [Address], "cc": [Address],
  "reply_to": [Address],
  "subject": "…",
  "date": 1727600000000,
  "text": "plain text body or null",
  "html": "html body or null (cid: images already inlined as data: URIs). Untrusted: render without scripts or remote loads.",
  "message_id": "<abc@example.com>",
  // Cloudflare MX verdicts for incoming mail, null for sent mail. When dmarc is anything but "pass" or
  // "none", the From header can't be trusted: the message never joins an existing thread; on a screened
  // mailbox an approval doesn't apply and it goes to the Screener; on a direct mailbox it still reaches
  // the Inbox (clients show a warning); mail forged as one of your own addresses goes to `blocked`.
  "auth": { "dmarc": "pass", "spf": "pass", "dkim": "pass", "spam_score": 0 },
  "attachments": [{ "id": "a_…", "filename": "invoice.pdf", "mime_type": "application/pdf", "size": 12345, "inline": false }]
}

// Mailbox – an address the worker receives and can send from. The first is the default From.
// screen: true sends first-time senders to the Screener; false delivers straight to the Inbox.
{ "email": "hi@example.com", "name": "Jane Doe", "screen": true, "position": 0 }

// PendingSender (screener): any non-blocked sender with threads waiting in the screener
{ "email": "new@person.com", "name": "New Person", "thread_count": 2, "last_subject": "Hi", "last_at": 1727600000000 }
```

## Endpoints

| Method | Path | Body / query | Returns |
|---|---|---|---|
| GET | `/api/counts` | | `{ "screener": 3, "inbox": 12, "inbox_unread": 2 }` (screener = pending sender count) |
| GET | `/api/threads` | `?folder=inbox\|archive\|screener\|sent\|blocked\|all&limit=50&before=<last_at>&since=<last_at>&q=<search>` | `{ "threads": [ThreadSummary] }` newest first. `all` = every non-blocked folder. `since` returns only threads with activity after that time (use it to poll for new mail). `q` does full-text search across all non-blocked folders (folder ignored). |
| GET | `/api/threads/:id` | | `{ "thread": ThreadSummary, "messages": [Message] }` oldest first. Does not mark read. |
| POST | `/api/threads/:id/move` | `{ "folder": "inbox"\|"archive" }` | `{ "ok": true }` |
| POST | `/api/threads/:id/read` | `{ "unread": false }` | `{ "ok": true }` |
| DELETE | `/api/threads/:id` | | `{ "ok": true }` – deletes thread, messages, R2 objects |
| GET | `/api/screener` | | `{ "senders": [PendingSender] }` |
| POST | `/api/senders/:email` | `{ "status": "approved"\|"blocked" }` | `{ "ok": true, "moved": 2 }` – approved moves their screener threads to inbox; blocked moves them to blocked |
| GET | `/api/senders` | `?status=approved\|blocked` | `{ "senders": [{ "email", "name", "status", "decided_at" }] }` |
| POST | `/api/send` | see below | `{ "ok": true, "thread_id": "t_…", "message": Message }`; if the mail was sent but couldn't be saved: `{ "ok": true, "thread_id": <the replied-to thread, or null>, "message": null, "warning": "…" }` (don't retry) |
| GET | `/api/attachments/:id` | | raw bytes with `Content-Type` and `Content-Disposition` |
| GET | `/api/messages/:id/raw` | | original `.eml` (`message/rfc822`) |
| GET | `/api/identities` | | `{ "identities": [Address], "default": Address }` – addresses you can send from (= mailboxes) |
| GET | `/api/mailboxes` | | `{ "mailboxes": [Mailbox] }` in display order |
| PUT | `/api/mailboxes/:email` | `{ "name"?: string, "screen"?: bool, "position"?: int }` | `{ "ok": true, "mailbox": Mailbox }` – creates or updates |
| DELETE | `/api/mailboxes/:email` | | `{ "ok": true }` |
| GET | `/api/settings` | | `{ "settings": { "forward_to": "" } }` |
| PATCH | `/api/settings` | `{ "forward_to": "me@elsewhere.com" }` (`""` turns forwarding off) | `{ "ok": true, "settings": {…} }` |

### POST /api/send

```jsonc
{
  "from": "support@example.org",       // optional; must be one of /api/identities; defaults to the first
  "to": ["a@b.com"], "cc": [], "bcc": [], // strings, "Name <a@b.com>" allowed
  "subject": "Hello",
  "text": "plain body",                 // required
  "html": "<p>optional</p>",            // optional; server derives one from text if absent
  "reply_to_message_id": "m_…"          // optional; threads the reply (In-Reply-To/References) and
                                        // puts the sent message in that thread
}
```

Health check (no auth): `GET /health` → `{ "ok": true }`.

Receiving also needs a Cloudflare Email Routing rule per address pointing at the worker; the API
cannot create those (they are managed with wrangler / `cloudmail setup` / `cloudmail mailbox add --route`).
The forward target must be a verified Email Routing destination address.
