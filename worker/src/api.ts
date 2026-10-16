import { deleteMailbox, findMailbox, getSettings, isForwardTarget, mailboxes, updateSettings, upsertMailbox } from "./mailboxes";
import { deleteThread, senderStatus, setSenderStatus, setSenderStatuses, storeMessage } from "./store";
import {
  authenticate,
  type Caller,
  createUser,
  findUser,
  issueInvite,
  listUsers,
  OWNER,
  redeemInvite,
  revokeUser,
  validName,
} from "./users";
import {
  type Address,
  arrayBufferToBase64,
  byteLength,
  error,
  json,
  messageIdList,
  normalizeEmail,
  normalizeMessageId,
  parseAddressList,
  textToHtml,
} from "./util";

interface ThreadRow {
  id: string;
  subject: string;
  folder: string;
  snippet: string | null;
  from_name: string | null;
  from_email: string | null;
  to_address: string | null;
  message_count: number;
  unread: number;
  has_attachments: number;
  last_at: number;
  last_sent_at: number | null;
}

interface MessageRow {
  id: string;
  thread_id: string;
  outgoing: number;
  message_id: string | null;
  in_reply_to: string | null;
  refs: string | null;
  from_name: string | null;
  from_email: string;
  to_json: string;
  cc_json: string;
  reply_to_json: string;
  subject: string;
  text_body: string | null;
  html_body: string | null;
  html_key: string | null;
  text_key: string | null;
  date: number;
  auth: string | null;
}

interface AttachmentRow {
  id: string;
  message_id: string;
  filename: string;
  mime_type: string;
  size: number;
  content_id: string | null;
  inline: number;
  r2_key: string;
}

const THREAD_COLUMNS = `id, subject, folder, snippet, from_name, from_email, to_address, message_count, unread,
  has_attachments, last_at, last_sent_at`;

function threadSummary(t: ThreadRow) {
  return {
    id: t.id,
    subject: t.subject,
    folder: t.folder,
    snippet: t.snippet ?? "",
    from: { name: t.from_name ?? "", email: t.from_email ?? "" },
    to_address: t.to_address ?? "",
    message_count: t.message_count,
    unread: !!t.unread,
    has_attachments: !!t.has_attachments,
    last_at: t.last_at,
  };
}

async function messageView(env: Env, m: MessageRow, atts: AttachmentRow[]) {
  let html = m.html_body;
  if (!html && m.html_key) html = (await (await env.BUCKET.get(m.html_key))?.text()) ?? null;
  let text = m.text_body;
  if (m.text_key) text = (await (await env.BUCKET.get(m.text_key))?.text()) ?? text;

  if (html && html.includes("cid:")) {
    let budget = 8 * 1024 * 1024;
    for (const a of atts) {
      if (!a.content_id || !html.includes(`cid:${a.content_id}`) || a.size > budget) continue;
      const obj = await env.BUCKET.get(a.r2_key);
      if (!obj) continue;
      budget -= a.size;
      const dataUri = `data:${a.mime_type};base64,${arrayBufferToBase64(await obj.arrayBuffer())}`;
      html = html.split(`cid:${a.content_id}`).join(dataUri);
    }
  }

  return {
    id: m.id,
    thread_id: m.thread_id,
    outgoing: !!m.outgoing,
    from: { name: m.from_name ?? "", email: m.from_email },
    to: JSON.parse(m.to_json) as Address[],
    cc: JSON.parse(m.cc_json) as Address[],
    reply_to: JSON.parse(m.reply_to_json) as Address[],
    subject: m.subject,
    date: m.date,
    text,
    html,
    message_id: m.message_id ?? "",
    auth: m.auth ? JSON.parse(m.auth) : null,
    attachments: atts.map((a) => ({
      id: a.id,
      filename: a.filename,
      mime_type: a.mime_type,
      size: a.size,
      inline: !!a.inline,
    })),
  };
}

async function loadMessages(env: Env, where: string, bind: unknown[]) {
  const messages = await env.DB.prepare(`SELECT * FROM messages WHERE ${where} ORDER BY date ASC`)
    .bind(...bind)
    .all<MessageRow>();
  if (messages.results.length === 0) return [];
  // A join rather than `IN (?, …)`: D1 allows 100 bound parameters, and threads can be longer.
  const atts = await env.DB.prepare(`SELECT a.* FROM attachments a JOIN messages m ON m.id = a.message_id WHERE m.${where}`)
    .bind(...bind)
    .all<AttachmentRow>();
  return Promise.all(messages.results.map((m) => messageView(env, m, atts.results.filter((a) => a.message_id === m.id))));
}

const EMAIL_RE = /^[^@\s]+@[^@\s]+\.[^@\s]+$/;

function ftsQuery(q: string): string {
  return q
    .split(/\s+/)
    .map((w) => w.replace(/"/g, ""))
    .filter(Boolean)
    .map((w) => `"${w}"*`)
    .join(" ");
}

function asRecipient(a: Address) {
  return a.name ? { email: a.email, name: a.name } : a.email;
}

async function identities(env: Env, userId: string): Promise<Address[]> {
  return (await mailboxes(env, userId)).map(({ name, email }) => ({ name, email }));
}

/** decodeURIComponent that yields "" (an invalid address) instead of throwing on malformed input. */
function safeDecode(s: string): string {
  try {
    return decodeURIComponent(s);
  } catch {
    return "";
  }
}

async function readJson<T>(req: Request): Promise<T | null> {
  try {
    return (await req.json()) as T;
  } catch {
    return null;
  }
}

async function listThreads(env: Env, u: Caller, url: URL): Promise<Response> {
  const folder = url.searchParams.get("folder") ?? "inbox";
  const limit = Math.max(1, Math.min(Math.trunc(Number(url.searchParams.get("limit"))) || 50, 200));
  const before = Number(url.searchParams.get("before")) || Number.MAX_SAFE_INTEGER;
  const q = url.searchParams.get("q")?.trim();
  const since = Number(url.searchParams.get("since")) || 0;
  const unread = url.searchParams.get("unread") === "1" ? "AND unread = 1" : "";

  let rows: D1Result<ThreadRow>;
  if (q) {
    const match = ftsQuery(q);
    if (!match) return json({ threads: [] });
    rows = await env.DB.prepare(
      `SELECT ${THREAD_COLUMNS} FROM threads WHERE user_id = ? AND folder != 'blocked' AND last_at < ? AND last_at > ? ${unread} AND id IN (
         SELECT m.thread_id FROM messages_fts f JOIN messages m ON m.rowid = f.rowid WHERE messages_fts MATCH ? AND m.user_id = ?
       ) ORDER BY last_at DESC LIMIT ?`,
    )
      .bind(u.id, before, since, match, u.id, limit)
      .all<ThreadRow>();
  } else if (folder === "sent") {
    rows = await env.DB.prepare(
      `SELECT ${THREAD_COLUMNS} FROM threads WHERE user_id = ? AND last_sent_at IS NOT NULL AND last_sent_at < ? AND last_sent_at > ?
       ${unread} ORDER BY last_sent_at DESC LIMIT ?`,
    )
      .bind(u.id, before, since, limit)
      .all<ThreadRow>();
    return json({ threads: rows.results.map((t) => ({ ...threadSummary(t), last_at: t.last_sent_at })) });
  } else if (folder === "all" || ["inbox", "archive", "screener", "blocked"].includes(folder)) {
    const [where, binds] = folder === "all" ? ["folder != 'blocked'", []] : ["folder = ?", [folder]];
    rows = await env.DB.prepare(
      `SELECT ${THREAD_COLUMNS} FROM threads WHERE user_id = ? AND ${where} AND last_at < ? AND last_at > ?
       ${unread} ORDER BY last_at DESC LIMIT ?`,
    )
      .bind(u.id, ...binds, before, since, limit)
      .all<ThreadRow>();
  } else {
    return error("unknown folder");
  }
  return json({ threads: rows.results.map(threadSummary) });
}

interface SendBody {
  from?: string;
  to?: string[] | string;
  cc?: string[] | string;
  bcc?: string[] | string;
  subject?: string;
  text?: string;
  html?: string;
  reply_to_message_id?: string;
  attachments?: unknown;
}

// Cloudflare Email Service takes at most 5 MiB per message, attachments included once encoded, and 32
// attachments (developers.cloudflare.com/email-service/platform/limits/ and …/api/send-emails/workers-api/).
const MAX_MESSAGE_BYTES = 5 * 1024 * 1024;
const MAX_ATTACHMENTS = 32;
// Headers, MIME boundaries and part headers, generously.
const MESSAGE_OVERHEAD = 16 * 1024;

interface OutgoingAttachment {
  filename: string;
  mimeType: string;
  bytes: Uint8Array;
}

const MIME_RE = /^[a-z0-9][a-z0-9!#$&^_.+-]*\/[a-z0-9][a-z0-9!#$&^_.+-]*$/;
const BASE64_RE = /^[A-Za-z0-9+/]*={0,2}$/;
// Native where the runtime has it (far cheaper in CPU than atob for megabytes).
const fromBase64 = (Uint8Array as unknown as { fromBase64?: (s: string) => Uint8Array }).fromBase64;

/** Bytes once base64-encoded in 76-character lines, as a MIME part carries them. */
export function encodedSize(bytes: number): number {
  const b64 = Math.ceil(bytes / 3) * 4;
  return b64 + Math.ceil(b64 / 76) * 2;
}

function megabytes(n: number): string {
  return `${(n / (1024 * 1024)).toFixed(1)} MiB`;
}

/** A file name safe for a Content-Disposition header and for listing: one line, no path. */
function cleanFilename(name: string): string {
  const clean = name.replace(/[\u0000-\u001f\u007f]/g, "").replace(/[/\\]/g, "_").trim().slice(0, 255);
  return clean || "attachment";
}

export function parseAttachments(input: unknown): OutgoingAttachment[] | string {
  if (input === undefined || input === null) return [];
  if (!Array.isArray(input)) return "attachments must be an array";
  if (input.length > MAX_ATTACHMENTS) return `too many attachments (${input.length}); at most ${MAX_ATTACHMENTS}`;
  const out: OutgoingAttachment[] = [];
  for (const [i, a] of input.entries()) {
    const { filename, mime_type, content } = (a ?? {}) as { filename?: unknown; mime_type?: unknown; content?: unknown };
    if (typeof filename !== "string" || !filename.trim()) return `attachments[${i}].filename is required`;
    if (typeof content !== "string") return `attachments[${i}].content must be a base64 string`;
    const b64 = content.replace(/\s+/g, "");
    if (b64.length % 4 !== 0 || !BASE64_RE.test(b64)) return `attachments[${i}].content is not valid base64`;
    let bytes: Uint8Array;
    try {
      bytes = fromBase64 ? fromBase64.call(Uint8Array, b64) : Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
    } catch {
      return `attachments[${i}].content is not valid base64`;
    }
    const type = typeof mime_type === "string" ? mime_type.trim().toLowerCase() : "";
    out.push({ filename: cleanFilename(filename), mimeType: MIME_RE.test(type) ? type : "application/octet-stream", bytes });
  }
  return out;
}

async function send(env: Env, u: Caller, req: Request): Promise<Response> {
  // Base64 makes attachments a third bigger in the request than in the message; anything far past
  // the message limit can't be sent anyway, so don't read it.
  if (Number(req.headers.get("content-length")) > 2 * MAX_MESSAGE_BYTES) {
    return error(`the request is too large; a message can be at most ${megabytes(MAX_MESSAGE_BYTES)} with its attachments`, 413);
  }
  const body = await readJson<SendBody>(req);
  if (!body) return error("invalid JSON");

  const to = parseAddressList(body.to);
  const cc = parseAddressList(body.cc);
  const bcc = parseAddressList(body.bcc);
  if (to.length + cc.length + bcc.length === 0) return error("no recipients");
  if (typeof body.text !== "string") return error("text is required");
  const attachments = parseAttachments(body.attachments);
  if (typeof attachments === "string") return error(attachments);

  const ids = await identities(env, u.id);
  const requested = body.from ? parseAddressList(body.from)[0] : ids[0];
  const identity = requested && ids.find((i) => i.email === requested.email);
  if (!identity) return error("from must be one of your mailboxes");
  const from = { email: identity.email, name: requested.name || identity.name };

  let original: MessageRow | null = null;
  if (body.reply_to_message_id) {
    original = await env.DB.prepare("SELECT * FROM messages WHERE id = ? AND user_id = ?")
      .bind(body.reply_to_message_id, u.id)
      .first<MessageRow>();
    if (!original) return error("reply_to_message_id not found", 404);
  }

  const headers: Record<string, string> = {};
  let refs: string[] = [];
  if (original?.message_id) {
    refs = [...messageIdList(original.refs), original.message_id].slice(-20);
    headers["In-Reply-To"] = original.message_id;
    headers["References"] = refs.join(" ");
  }

  const subject = body.subject?.trim() || "(no subject)";
  const html = body.html ?? textToHtml(body.text);

  if (attachments.length) {
    // Text and HTML bodies may be quoted-printable or base64 encoded: count them as base64.
    const bodies = encodedSize(byteLength(body.text)) + encodedSize(byteLength(html)) + MESSAGE_OVERHEAD;
    const files = attachments.reduce((n, a) => n + a.bytes.length, 0);
    if (bodies + attachments.reduce((n, a) => n + encodedSize(a.bytes.length), 0) > MAX_MESSAGE_BYTES) {
      const room = Math.max(0, Math.floor(((MAX_MESSAGE_BYTES - bodies) * 3) / 4 / (78 / 76)));
      return error(
        `the attachments are too large (${megabytes(files)}): Cloudflare Email Service sends at most ${megabytes(MAX_MESSAGE_BYTES)} per message once encoded, which leaves about ${megabytes(room)} for attachments`,
        413,
      );
    }
  }

  let result: EmailSendResult;
  try {
    result = await env.EMAIL.send({
      from,
      to: to.map(asRecipient),
      cc: cc.length ? cc.map(asRecipient) : undefined,
      bcc: bcc.length ? bcc.map(asRecipient) : undefined,
      subject,
      text: body.text,
      html,
      headers,
      attachments: attachments.length
        ? // Bytes, not base64: the deployed Email Service sends a string content as the file's literal text.
          attachments.map((a) => ({ disposition: "attachment" as const, filename: a.filename, type: a.mimeType, content: a.bytes }))
        : undefined,
    });
  } catch (err) {
    const e = err as { code?: string; message?: string };
    const status = e.code === "E_CONTENT_TOO_LARGE" ? 413 : e.code === "E_TOO_MANY_ATTACHMENTS" ? 400 : 502;
    return error(`send failed: ${e.code ?? ""} ${e.message ?? String(err)}`.trim(), status);
  }

  // The mail has gone out. Anything failing from here must not look like a failed send, or a
  // client would retry and send it twice.
  try {
    const { id, threadId } = await storeMessage(env, {
      userId: u.id,
      outgoing: true,
      messageId: normalizeMessageId(result.messageId) ?? `<${result.messageId}>`,
      inReplyTo: original?.message_id ?? null,
      refs,
      from,
      to,
      cc,
      replyTo: [],
      envelopeTo: null,
      subject,
      text: body.text,
      html,
      date: Date.now(),
      raw: null,
      attachments: attachments.map((a) => ({ filename: a.filename, mimeType: a.mimeType, content: a.bytes, contentId: null, inline: false })),
      threadId: original?.thread_id,
      // A new conversation you start isn't something to act on; replies bring it to the inbox.
      newThreadFolder: "archive",
    });
    await env.DB.prepare("UPDATE threads SET unread = 0 WHERE id = ?").bind(threadId).run();

    // Anyone you write to is screened in.
    for (const r of [...to, ...cc, ...bcc]) {
      if (ids.some((i) => i.email === r.email)) continue;
      if ((await senderStatus(env, u.id, r.email)) !== "approved") await setSenderStatus(env, u.id, r.email, r.name || null, "approved");
    }

    const [message] = await loadMessages(env, "id = ?", [id]);
    return json({ ok: true, thread_id: threadId, message });
  } catch (err) {
    console.error("sent but not stored", err);
    return json({
      ok: true,
      thread_id: original?.thread_id ?? null,
      message: null,
      warning: "it couldn't be saved to Sent",
    });
  }
}

export async function handleApi(req: Request, env: Env): Promise<Response> {
  const url = new URL(req.url);
  const path = url.pathname;

  const method = req.method;
  let m: RegExpMatchArray | null;

  if (path === "/health") return json({ ok: true });
  // Someone opened an invite link in a browser: say what to do with it. Says nothing about the code.
  if (method === "GET" && path.startsWith("/invite/")) {
    return new Response(
      `This is an invitation to read your mail with Cloudmail.\n\nOn your computer, install Cloudmail and run:\n\n  cloudmail connect ${url.origin}${path}\n`,
      { headers: { "content-type": "text/plain; charset=utf-8" } },
    );
  }
  if (!path.startsWith("/api/")) return error("not found", 404);

  // The one call without a token: trading an invite for one.
  if (method === "POST" && path === "/api/connect") {
    const body = await readJson<{ code?: unknown }>(req);
    const code = typeof body?.code === "string" ? body.code.trim() : "";
    const redeemed = code ? await redeemInvite(env, code) : null;
    if (!redeemed) return error("this invite is invalid, used or expired; ask for a new one", 403);
    return json({ ok: true, ...redeemed });
  }

  const auth = req.headers.get("authorization") ?? "";
  const u = await authenticate(env, auth.startsWith("Bearer ") ? auth.slice(7) : "");
  if (!u) return error("unauthorized", 401);

  if (method === "GET" && path === "/api/me") return json({ user: { name: u.name, role: u.admin ? "admin" : "user" } });
  if (path === "/api/users" || path.startsWith("/api/users/")) {
    if (!u.admin) return error("only the admin manages users", 403);
    return users(env, req, url);
  }

  if (method === "GET" && path === "/api/counts") {
    const row = await env.DB.prepare(
      `SELECT
         (SELECT COUNT(*) FROM senders s WHERE s.user_id = ?1 AND s.status != 'blocked'
            AND EXISTS (SELECT 1 FROM threads t WHERE t.user_id = ?1 AND t.sender_email = s.email AND t.folder = 'screener')) AS screener,
         (SELECT COUNT(*) FROM threads WHERE user_id = ?1 AND folder = 'inbox') AS inbox,
         (SELECT COUNT(*) FROM threads WHERE user_id = ?1 AND folder = 'inbox' AND unread = 1) AS inbox_unread`,
    )
      .bind(u.id)
      .first<{ screener: number; inbox: number; inbox_unread: number }>();
    return json(row);
  }

  if (method === "GET" && path === "/api/threads") return listThreads(env, u, url);

  if ((m = path.match(/^\/api\/threads\/([\w-]+)$/))) {
    const id = m[1];
    if (method === "GET") {
      const t = await env.DB.prepare(`SELECT ${THREAD_COLUMNS} FROM threads WHERE id = ? AND user_id = ?`).bind(id, u.id).first<ThreadRow>();
      if (!t) return error("not found", 404);
      return json({ thread: threadSummary(t), messages: await loadMessages(env, "thread_id = ?", [id]) });
    }
    if (method === "DELETE") {
      return (await deleteThread(env, u.id, id)) ? json({ ok: true }) : error("not found", 404);
    }
  }

  if (method === "POST" && (m = path.match(/^\/api\/threads\/([\w-]+)\/move$/))) {
    const body = await readJson<{ folder?: string }>(req);
    if (body?.folder !== "inbox" && body?.folder !== "archive") return error("folder must be inbox or archive");
    // Only between Inbox and Archive: a Screener or Blocked thread leaves by deciding on its sender.
    const res = await env.DB.prepare("UPDATE threads SET folder = ? WHERE id = ? AND user_id = ? AND folder IN ('inbox', 'archive')")
      .bind(body.folder, m[1], u.id)
      .run();
    if (res.meta.changes) return json({ ok: true });
    const t = await env.DB.prepare("SELECT folder FROM threads WHERE id = ? AND user_id = ?").bind(m[1], u.id).first<{ folder: string }>();
    if (!t) return error("not found", 404);
    return error(`this thread is in ${t.folder}; approve or block its sender instead (POST /api/senders/:email)`, 409);
  }

  if (method === "POST" && (m = path.match(/^\/api\/threads\/([\w-]+)\/read$/))) {
    const body = await readJson<{ unread?: boolean }>(req);
    const res = await env.DB.prepare("UPDATE threads SET unread = ? WHERE id = ? AND user_id = ?").bind(body?.unread ? 1 : 0, m[1], u.id).run();
    return res.meta.changes ? json({ ok: true }) : error("not found", 404);
  }

  if (method === "GET" && path === "/api/screener") {
    const rows = await env.DB.prepare(
      `SELECT s.email, s.name, COUNT(t.id) AS thread_count, MAX(t.last_at) AS last_at,
         (SELECT subject FROM threads t2 WHERE t2.user_id = s.user_id AND t2.sender_email = s.email AND t2.folder = 'screener'
            ORDER BY t2.last_at DESC LIMIT 1) AS last_subject
       FROM senders s JOIN threads t ON t.user_id = s.user_id AND t.sender_email = s.email AND t.folder = 'screener'
       WHERE s.user_id = ? AND s.status != 'blocked'
       GROUP BY s.email ORDER BY last_at DESC`,
    )
      .bind(u.id)
      .all();
    return json({ senders: rows.results.map((r) => ({ ...r, name: r.name ?? "" })) });
  }

  if (method === "GET" && path === "/api/senders") {
    const status = url.searchParams.get("status") ?? "approved";
    const rows = await env.DB.prepare(
      "SELECT email, COALESCE(name, '') AS name, status, decided_at FROM senders WHERE user_id = ? AND status = ? ORDER BY decided_at DESC",
    )
      .bind(u.id, status)
      .all();
    return json({ senders: rows.results });
  }

  // Decisions for linked accounts' senders (Gmail, iCloud Mail), which your Screener decides:
  // their mail isn't stored here, so these only read and record decisions.
  if (method === "POST" && path === "/api/senders/lookup") {
    const body = await readJson<{ emails?: unknown }>(req);
    if (!Array.isArray(body?.emails) || body.emails.length > 1000) return error("emails must be a list of at most 1000 addresses");
    const emails = [...new Set(body.emails.filter((e): e is string => typeof e === "string").map(normalizeEmail).filter((e) => e.includes("@")))];
    const found: { email: string; status: string }[] = [];
    for (let i = 0; i < emails.length; i += 90) {
      const chunk = emails.slice(i, i + 90);
      const rows = await env.DB.prepare(`SELECT email, status FROM senders WHERE user_id = ? AND email IN (${chunk.map(() => "?").join(",")})`)
        .bind(u.id, ...chunk)
        .all<{ email: string; status: string }>();
      found.push(...rows.results);
    }
    return json({ senders: found });
  }

  if (method === "POST" && path === "/api/senders/batch") {
    const body = await readJson<{ senders?: unknown; status?: string; only_undecided?: boolean }>(req);
    if (body?.status !== "approved" && body?.status !== "blocked") return error("status must be approved or blocked");
    if (!Array.isArray(body.senders) || body.senders.length > 1000) return error("senders must be a list of at most 1000 {email, name}");
    const senders = body.senders
      .filter((s): s is { email: string; name?: unknown } => typeof s === "object" && s !== null && typeof (s as { email?: unknown }).email === "string")
      .map((s) => ({ email: normalizeEmail(s.email), name: typeof s.name === "string" && s.name.trim() ? s.name.trim() : null }))
      .filter((s) => s.email.includes("@"));
    const changed = await setSenderStatuses(env, u.id, senders, body.status, body.only_undecided === true);
    return json({ ok: true, changed });
  }

  if (method === "POST" && (m = path.match(/^\/api\/senders\/([^/]+)$/))) {
    const email = normalizeEmail(safeDecode(m[1]));
    if (!email.includes("@")) return error("invalid email address");
    const body = await readJson<{ status?: string }>(req);
    if (body?.status !== "approved" && body?.status !== "blocked") return error("status must be approved or blocked");
    const moved = await setSenderStatus(env, u.id, email, null, body.status);
    return json({ ok: true, moved });
  }

  if (method === "POST" && path === "/api/send") return send(env, u, req);

  if (method === "GET" && (m = path.match(/^\/api\/attachments\/([\w-]+)$/))) {
    const a = await env.DB.prepare("SELECT a.* FROM attachments a JOIN messages m ON m.id = a.message_id WHERE a.id = ? AND m.user_id = ?")
      .bind(m[1], u.id)
      .first<AttachmentRow>();
    const obj = a && (await env.BUCKET.get(a.r2_key));
    if (!a || !obj) return error("not found", 404);
    return new Response(obj.body, {
      headers: {
        "content-type": a.mime_type,
        "content-disposition": `attachment; filename*=UTF-8''${encodeURIComponent(a.filename)}`,
      },
    });
  }

  if (method === "GET" && (m = path.match(/^\/api\/messages\/([\w-]+)\/raw$/))) {
    const row = await env.DB.prepare("SELECT raw_key FROM messages WHERE id = ? AND user_id = ?")
      .bind(m[1], u.id)
      .first<{ raw_key: string | null }>();
    const obj = row?.raw_key && (await env.BUCKET.get(row.raw_key));
    if (!obj) return error("not found", 404);
    return new Response(obj.body, {
      headers: { "content-type": "message/rfc822", "content-disposition": `attachment; filename="${m[1]}.eml"` },
    });
  }

  if (method === "GET" && path === "/api/identities") {
    const ids = await identities(env, u.id);
    return json({ identities: ids, default: ids[0] ?? null });
  }

  if (method === "GET" && path === "/api/mailboxes") {
    // The admin can see who has which address.
    const all = u.admin && url.searchParams.get("all") === "1";
    return json({ mailboxes: await mailboxes(env, all ? null : u.id) });
  }

  if ((m = path.match(/^\/api\/mailboxes\/([^/]+)$/))) {
    const email = normalizeEmail(safeDecode(m[1]));
    if (method === "PUT") {
      const body = await readJson<{ name?: unknown; screen?: unknown; position?: unknown; user?: unknown }>(req);
      if (!body) return error("invalid JSON");
      if (!EMAIL_RE.test(email)) return error("invalid email address");
      if (body.name !== undefined && typeof body.name !== "string") return error("name must be a string");
      if (body.screen !== undefined && typeof body.screen !== "boolean") return error("screen must be a boolean");
      if (body.position !== undefined && !Number.isInteger(body.position)) return error("position must be an integer");
      if (body.user !== undefined && typeof body.user !== "string") return error("user must be a user name");
      const existing = findMailbox(await mailboxes(env, null), email);
      let userId: string;
      if (!u.admin) {
        // Anyone can rename or reorder their own addresses; adding or giving them away is the admin's.
        if (body.user !== undefined || existing?.user_id !== u.id) return error("only the admin adds mailboxes or gives them to someone", 403);
        userId = u.id;
      } else if (typeof body.user === "string") {
        const target = await findUser(env, body.user);
        if (!target) return error(`no user named ${body.user}`, 404);
        userId = target.id;
      } else {
        userId = existing?.user_id ?? u.id;
      }
      // Mail to this address would be forwarded straight back here, forever.
      if (await isForwardTarget(env, email)) return error("this address is a forward_to target; change forward_to first");
      const mailbox = await upsertMailbox(env, {
        email,
        userId,
        name: body.name as string | undefined,
        screen: body.screen as boolean | undefined,
        position: body.position as number | undefined,
      });
      return json({ ok: true, mailbox });
    }
    if (method === "DELETE") {
      if (!u.admin) return error("only the admin removes mailboxes", 403);
      return (await deleteMailbox(env, email)) ? json({ ok: true }) : error("not found", 404);
    }
  }

  if (path === "/api/settings") {
    // The admin can see and set someone's forwarding (its destination is verified in the admin's Cloudflare account).
    const who = url.searchParams.get("user");
    let userId = u.id;
    if (who) {
      if (!u.admin) return error("only the admin changes someone else's settings", 403);
      const target = await findUser(env, who);
      if (!target) return error(`no user named ${who}`, 404);
      userId = target.id;
    }
    if (method === "GET") return json({ settings: await getSettings(env, userId) });
    if (method === "PATCH") {
      const body = await readJson<Record<string, unknown>>(req);
      if (!body) return error("invalid JSON");
      let forward_to: string | undefined;
      if (body.forward_to !== undefined) {
        if (typeof body.forward_to !== "string") return error("forward_to must be a string");
        forward_to = normalizeEmail(body.forward_to);
        if (forward_to && !EMAIL_RE.test(forward_to)) return error("forward_to must be an email address");
        // Forwarding to an address this worker receives would route straight back here, forever.
        if ((await mailboxes(env, null)).some((mb) => mb.email === forward_to)) return error("forward_to cannot be a mailbox here");
      }
      return json({ ok: true, settings: await updateSettings(env, userId, { forward_to }) });
    }
  }

  return error("not found", 404);
}

/** The admin's user management: /api/users and /api/users/:name[/invite]. */
async function users(env: Env, req: Request, url: URL): Promise<Response> {
  const method = req.method;
  const path = url.pathname;
  if (path === "/api/users") {
    if (method === "GET") return json({ users: await listUsers(env) });
    if (method === "POST") {
      const body = await readJson<{ name?: unknown; mailboxes?: unknown }>(req);
      const name = typeof body?.name === "string" ? body.name.trim().toLowerCase() : "";
      if (!validName(name)) return error("name must be 1–32 lowercase letters, digits, - or _, starting with a letter or digit");
      const list = body?.mailboxes ?? [];
      if (!Array.isArray(list) || list.some((e) => typeof e !== "string" || !EMAIL_RE.test(normalizeEmail(e)))) {
        return error("mailboxes must be a list of email addresses");
      }
      if (await findUser(env, name)) return error(`${name} already exists; send a new invite with POST /api/users/${name}/invite`, 409);
      for (const e of list as string[]) {
        if (await isForwardTarget(env, e)) return error(`${normalizeEmail(e)} is a forward_to target; change forward_to first`);
      }
      const id = await createUser(env, name);
      for (const e of list as string[]) await upsertMailbox(env, { email: e, userId: id });
      const invite = await issueInvite(env, id, url.origin);
      const user = (await listUsers(env)).find((x) => x.name === name);
      return json({ ok: true, user, invite });
    }
    return error("not found", 404);
  }

  const m = path.match(/^\/api\/users\/([^/]+)(\/invite)?$/);
  if (!m) return error("not found", 404);
  const user = await findUser(env, safeDecode(m[1]));
  if (!user) return error("not found", 404);
  if (user.id === OWNER) return error("the admin signs in with the worker's API_TOKEN; rotate it with `cloudmail setup --force`");
  if (method === "POST" && m[2]) return json({ ok: true, invite: await issueInvite(env, user.id, url.origin) });
  if (method === "DELETE" && !m[2]) {
    const purge = url.searchParams.get("purge") === "1";
    await revokeUser(env, user, purge);
    return json({ ok: true, purged: purge });
  }
  return error("not found", 404);
}
