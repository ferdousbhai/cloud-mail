import { type Address, htmlToText, makeSnippet, newId, byteLength } from "./util";

export type Folder = "screener" | "inbox" | "archive" | "blocked";
type SenderStatus = "pending" | "approved" | "blocked";

// D1 rows cap out around 2 MB; bigger bodies live in R2.
const MAX_INLINE_BODY = 512 * 1024;

export interface NewAttachment {
  filename: string;
  mimeType: string;
  content: ArrayBuffer | Uint8Array;
  contentId: string | null;
  inline: boolean;
}

export interface NewMessage {
  outgoing: boolean;
  messageId: string | null;
  inReplyTo: string | null;
  refs: string[];
  from: Address;
  to: Address[];
  cc: Address[];
  replyTo: Address[];
  envelopeTo: string | null;
  subject: string;
  text: string | null;
  html: string | null;
  date: number;
  raw: ArrayBuffer | null;
  auth?: unknown;
  attachments: NewAttachment[];
  /** Thread to attach to; otherwise found via In-Reply-To/References or created. */
  threadId?: string;
  /** Folder for a brand-new thread. */
  newThreadFolder: Folder;
  /** Folder to move an existing thread to, if any. */
  existingThreadFolder?: (current: Folder) => Folder;
  /** Whether the message may join the thread its In-Reply-To/References match; otherwise it starts one. */
  joinThread?: (thread: ThreadRef) => boolean;
}

export async function senderStatus(env: Env, email: string): Promise<SenderStatus | null> {
  const row = await env.DB.prepare("SELECT status FROM senders WHERE email = ?").bind(email).first<{ status: SenderStatus }>();
  return row?.status ?? null;
}

export interface ThreadRef {
  id: string;
  folder: Folder;
  sender_email: string | null;
}

/** Threads containing any of these Message-IDs, most recent first. */
async function findThreadsByMessageIds(env: Env, ids: string[]): Promise<ThreadRef[]> {
  if (ids.length === 0) return [];
  const unique = [...new Set(ids)].slice(-50);
  const placeholders = unique.map(() => "?").join(",");
  const rows = await env.DB.prepare(
    `SELECT t.id, t.folder, t.sender_email, MAX(m.date) AS latest FROM messages m JOIN threads t ON t.id = m.thread_id
     WHERE m.message_id IN (${placeholders}) GROUP BY t.id ORDER BY latest DESC LIMIT 20`,
  )
    .bind(...unique)
    .all<ThreadRef & { latest: number }>();
  return rows.results.map(({ id, folder, sender_email }) => ({ id, folder, sender_email }));
}

export async function storeMessage(env: Env, msg: NewMessage): Promise<{ id: string; threadId: string }> {
  const id = newId("m");
  const now = Date.now();

  let thread: { id: string; folder: Folder } | null = null;
  if (msg.threadId) {
    thread = await env.DB.prepare("SELECT id, folder FROM threads WHERE id = ?").bind(msg.threadId).first();
  }
  if (!thread) {
    // Includes the message's own id so a message you sent to yourself joins the thread it was sent from.
    const ids = [...msg.refs, msg.inReplyTo, msg.outgoing ? null : msg.messageId];
    // The newest referenced thread this message may join; a reply-all that also references a
    // newcomer's separate Screener thread still joins the conversation it belongs to.
    const candidates = await findThreadsByMessageIds(env, ids.filter((x): x is string => !!x));
    thread = candidates.find((t) => !msg.joinThread || msg.joinThread(t)) ?? null;
  }

  const rawKey = msg.raw ? `raw/${id}.eml` : null;
  const puts: Promise<unknown>[] = [];
  if (rawKey && msg.raw) puts.push(env.BUCKET.put(rawKey, msg.raw, { httpMetadata: { contentType: "message/rfc822" } }));

  let htmlBody = msg.html;
  let htmlKey: string | null = null;
  if (htmlBody && byteLength(htmlBody) > MAX_INLINE_BODY) {
    htmlKey = `html/${id}.html`;
    puts.push(env.BUCKET.put(htmlKey, htmlBody, { httpMetadata: { contentType: "text/html; charset=utf-8" } }));
    htmlBody = null;
  }
  let textBody = msg.text;
  let textKey: string | null = null;
  if (textBody && byteLength(textBody) > MAX_INLINE_BODY) {
    textKey = `text/${id}.txt`;
    puts.push(env.BUCKET.put(textKey, textBody, { httpMetadata: { contentType: "text/plain; charset=utf-8" } }));
    // Keep a readable head inline for listings; the full text is fetched from R2.
    textBody = textBody.slice(0, 64 * 1024);
  }

  const attachmentRows = msg.attachments.map((a) => {
    const attId = newId("a");
    const key = `att/${attId}`;
    puts.push(env.BUCKET.put(key, a.content, { httpMetadata: { contentType: a.mimeType } }));
    return { id: attId, key, a, size: a.content.byteLength };
  });
  // Nothing references the objects just written until the batch below lands; don't leave them behind.
  const removeWritten = () =>
    env.BUCKET.delete([rawKey, htmlKey, textKey, ...attachmentRows.map((r) => r.key)].filter((k): k is string => !!k)).catch(() => {});
  // If any upload fails, remove the ones that landed.
  const settled = await Promise.allSettled(puts);
  const rejected = settled.find((r): r is PromiseRejectedResult => r.status === "rejected");
  if (rejected) {
    await removeWritten();
    throw rejected.reason;
  }

  const bodyText = msg.text ?? (msg.html ? htmlToText(msg.html) : "");
  const snippet = makeSnippet(bodyText);
  const hasVisibleAttachments = msg.attachments.some((a) => !a.inline);
  const stmts: D1PreparedStatement[] = [];

  let threadId: string;
  if (thread) {
    threadId = thread.id;
    const folder = msg.existingThreadFolder ? msg.existingThreadFolder(thread.folder) : thread.folder;
    stmts.push(
      env.DB.prepare(
        `UPDATE threads SET folder = ?, snippet = ?, message_count = message_count + 1, last_at = ?,
           unread = CASE WHEN ? THEN unread ELSE 1 END,
           has_attachments = has_attachments OR ?,
           last_sent_at = CASE WHEN ? THEN ? ELSE last_sent_at END,
           from_name = CASE WHEN ? THEN from_name ELSE ? END,
           from_email = CASE WHEN ? THEN from_email ELSE ? END
         WHERE id = ?`,
      ).bind(
        folder, snippet, now,
        msg.outgoing ? 1 : 0,
        hasVisibleAttachments ? 1 : 0,
        msg.outgoing ? 1 : 0, now,
        msg.outgoing ? 1 : 0, msg.from.name,
        msg.outgoing ? 1 : 0, msg.from.email,
        threadId,
      ),
    );
  } else {
    threadId = newId("t");
    const subject = msg.subject.replace(/^((re|fwd?|aw|sv)\s*:\s*)+/i, "").trim() || "(no subject)";
    // A thread shows the other party: the sender, or for one you start, who you wrote to.
    const party = msg.outgoing ? (msg.to[0] ?? msg.cc[0] ?? msg.from) : msg.from;
    stmts.push(
      env.DB.prepare(
        `INSERT INTO threads (id, subject, folder, sender_email, from_name, from_email, to_address, snippet,
           message_count, unread, has_attachments, last_sent_at, last_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?)`,
      ).bind(
        threadId, subject, msg.newThreadFolder,
        msg.outgoing ? (msg.to[0]?.email ?? null) : msg.from.email,
        party.name, party.email,
        msg.outgoing ? msg.from.email : msg.envelopeTo,
        snippet,
        msg.outgoing || msg.newThreadFolder === "blocked" ? 0 : 1,
        hasVisibleAttachments ? 1 : 0,
        msg.outgoing ? now : null,
        now,
      ),
    );
  }

  stmts.push(
    env.DB.prepare(
      `INSERT INTO messages (id, thread_id, outgoing, message_id, in_reply_to, refs, from_name, from_email,
         to_json, cc_json, reply_to_json, envelope_to, subject, text_body, text_key, html_body, html_key, date, raw_key, auth)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
    ).bind(
      id, threadId, msg.outgoing ? 1 : 0, msg.messageId, msg.inReplyTo, msg.refs.join(" ") || null,
      msg.from.name, msg.from.email,
      JSON.stringify(msg.to), JSON.stringify(msg.cc), JSON.stringify(msg.replyTo),
      msg.envelopeTo, msg.subject, textBody, textKey, htmlBody, htmlKey, msg.date, rawKey,
      msg.auth ? JSON.stringify(msg.auth) : null,
    ),
  );

  for (const r of attachmentRows) {
    stmts.push(
      env.DB.prepare(
        `INSERT INTO attachments (id, message_id, filename, mime_type, size, content_id, inline, r2_key)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)`,
      ).bind(r.id, id, r.a.filename, r.a.mimeType, r.size, r.a.contentId, r.a.inline ? 1 : 0, r.key),
    );
  }

  stmts.push(
    env.DB.prepare(
      `INSERT INTO messages_fts (rowid, subject, body, from_text)
       VALUES ((SELECT rowid FROM messages WHERE id = ?), ?, ?, ?)`,
    ).bind(id, msg.subject, bodyText.slice(0, 100_000), [msg.from, ...msg.to, ...msg.cc].map((a) => `${a.name} ${a.email}`).join(" ")),
  );

  try {
    await env.DB.batch(stmts);
  } catch (err) {
    await removeWritten();
    throw err;
  }
  return { id, threadId };
}

/** Records a screening decision and moves that sender's screener threads accordingly. */
export async function setSenderStatus(env: Env, email: string, name: string | null, status: SenderStatus): Promise<number> {
  const now = Date.now();
  const [, moved] = await env.DB.batch([
    status === "pending"
      ? // First sight of a sender never overrides a decision made meanwhile (or by a racing delivery).
        env.DB.prepare(
          "INSERT INTO senders (email, name, status, decided_at, created_at) VALUES (?, ?, 'pending', NULL, ?) ON CONFLICT (email) DO NOTHING",
        ).bind(email, name, now)
      : env.DB.prepare(
          `INSERT INTO senders (email, name, status, decided_at, created_at) VALUES (?, ?, ?, ?, ?)
           ON CONFLICT (email) DO UPDATE SET status = excluded.status, decided_at = excluded.decided_at,
             name = COALESCE(senders.name, excluded.name)`,
        ).bind(email, name, status, now, now),
    status === "approved"
      ? // Mail back from Blocked was never seen (blocking marks it read), so it comes back unread.
        env.DB.prepare(
          `UPDATE threads SET unread = CASE WHEN folder = 'blocked' THEN 1 ELSE unread END, folder = 'inbox'
           WHERE sender_email = ? AND folder IN ('screener', 'blocked')`,
        ).bind(email)
      : status === "blocked"
        ? env.DB.prepare("UPDATE threads SET folder = 'blocked', unread = 0 WHERE sender_email = ? AND folder = 'screener'").bind(email)
        : env.DB.prepare("SELECT 1"),
  ]);
  return moved.meta.changes ?? 0;
}

export async function deleteThread(env: Env, threadId: string): Promise<boolean> {
  const messages = await env.DB.prepare("SELECT raw_key, html_key, text_key FROM messages WHERE thread_id = ?")
    .bind(threadId)
    .all<{ raw_key: string | null; html_key: string | null; text_key: string | null }>();
  const atts = await env.DB.prepare(
    "SELECT a.r2_key FROM attachments a JOIN messages m ON m.id = a.message_id WHERE m.thread_id = ?",
  )
    .bind(threadId)
    .all<{ r2_key: string }>();

  const keys = [
    ...messages.results.flatMap((m) => [m.raw_key, m.html_key, m.text_key]),
    ...atts.results.map((a) => a.r2_key),
  ].filter((k): k is string => !!k);
  for (let i = 0; i < keys.length; i += 1000) await env.BUCKET.delete(keys.slice(i, i + 1000));

  const res = await env.DB.batch([
    env.DB.prepare("DELETE FROM messages_fts WHERE rowid IN (SELECT rowid FROM messages WHERE thread_id = ?)").bind(threadId),
    env.DB.prepare("DELETE FROM attachments WHERE message_id IN (SELECT id FROM messages WHERE thread_id = ?)").bind(threadId),
    env.DB.prepare("DELETE FROM messages WHERE thread_id = ?").bind(threadId),
    env.DB.prepare("DELETE FROM threads WHERE id = ?").bind(threadId),
  ]);
  return (res[3].meta.changes ?? 0) > 0;
}
