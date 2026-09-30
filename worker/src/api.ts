import { deleteMailbox, getSettings, mailboxes, updateSettings, upsertMailbox } from "./mailboxes";
import { deleteThread, type SenderStatus, setSenderStatus, storeMessage } from "./store";
import {
  type Address,
  arrayBufferToBase64,
  error,
  json,
  messageIdList,
  normalizeEmail,
  normalizeMessageId,
  parseAddressList,
  textToHtml,
  timingSafeEqual,
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

async function identities(env: Env): Promise<Address[]> {
  return (await mailboxes(env)).map(({ name, email }) => ({ name, email }));
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

async function listThreads(env: Env, url: URL): Promise<Response> {
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
      `SELECT ${THREAD_COLUMNS} FROM threads WHERE folder != 'blocked' AND last_at < ? AND last_at > ? ${unread} AND id IN (
         SELECT m.thread_id FROM messages_fts f JOIN messages m ON m.rowid = f.rowid WHERE messages_fts MATCH ?
       ) ORDER BY last_at DESC LIMIT ?`,
    )
      .bind(before, since, match, limit)
      .all<ThreadRow>();
  } else if (folder === "sent") {
    rows = await env.DB.prepare(
      `SELECT ${THREAD_COLUMNS} FROM threads WHERE last_sent_at IS NOT NULL AND last_sent_at < ? AND last_sent_at > ?
       ${unread} ORDER BY last_sent_at DESC LIMIT ?`,
    )
      .bind(before, since, limit)
      .all<ThreadRow>();
    return json({ threads: rows.results.map((t) => ({ ...threadSummary(t), last_at: t.last_sent_at })) });
  } else if (folder === "all") {
    rows = await env.DB.prepare(
      `SELECT ${THREAD_COLUMNS} FROM threads WHERE folder != 'blocked' AND last_at < ? AND last_at > ?
       ${unread} ORDER BY last_at DESC LIMIT ?`,
    )
      .bind(before, since, limit)
      .all<ThreadRow>();
  } else if (["inbox", "archive", "screener", "blocked"].includes(folder)) {
    rows = await env.DB.prepare(
      `SELECT ${THREAD_COLUMNS} FROM threads WHERE folder = ? AND last_at < ? AND last_at > ?
       ${unread} ORDER BY last_at DESC LIMIT ?`,
    )
      .bind(folder, before, since, limit)
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
}

async function send(env: Env, req: Request): Promise<Response> {
  const body = await readJson<SendBody>(req);
  if (!body) return error("invalid JSON");

  const to = parseAddressList(body.to);
  const cc = parseAddressList(body.cc);
  const bcc = parseAddressList(body.bcc);
  if (to.length + cc.length + bcc.length === 0) return error("no recipients");
  if (typeof body.text !== "string") return error("text is required");

  const ids = await identities(env);
  const requested = body.from ? parseAddressList(body.from)[0] : ids[0];
  const identity = requested && ids.find((i) => i.email === requested.email);
  if (!identity) return error("from must be one of your mailboxes");
  const from = { email: identity.email, name: requested.name || identity.name };

  let original: MessageRow | null = null;
  if (body.reply_to_message_id) {
    original = await env.DB.prepare("SELECT * FROM messages WHERE id = ?").bind(body.reply_to_message_id).first<MessageRow>();
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

  let result: EmailSendResult;
  try {
    result = await env.EMAIL.send({
      from: { email: from.email, name: from.name },
      to: to.map(asRecipient),
      cc: cc.length ? cc.map(asRecipient) : undefined,
      bcc: bcc.length ? bcc.map(asRecipient) : undefined,
      subject,
      text: body.text,
      html,
      headers,
    });
  } catch (err) {
    const e = err as { code?: string; message?: string };
    return error(`send failed: ${e.code ?? ""} ${e.message ?? String(err)}`.trim(), 502);
  }

  // The mail has gone out. Anything failing from here must not look like a failed send, or a
  // client would retry and send it twice.
  try {
    return await recordSent(env, { body, result, original, refs, from, to, cc, bcc, subject, html, ids });
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

async function recordSent(
  env: Env,
  s: {
    body: SendBody;
    result: EmailSendResult;
    original: MessageRow | null;
    refs: string[];
    from: Address;
    to: Address[];
    cc: Address[];
    bcc: Address[];
    subject: string;
    html: string;
    ids: Address[];
  },
): Promise<Response> {
  const { body, result, original, refs, from, to, cc, bcc, subject, html, ids } = s;
  const { id, threadId } = await storeMessage(env, {
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
    text: body.text ?? null,
    html,
    date: Date.now(),
    raw: null,
    attachments: [],
    threadId: original?.thread_id,
    // A new conversation you start isn't something to act on; replies bring it to the inbox.
    newThreadFolder: "archive",
  });
  await env.DB.prepare("UPDATE threads SET unread = 0 WHERE id = ?").bind(threadId).run();

  // Anyone you write to is screened in.
  for (const r of [...to, ...cc, ...bcc]) {
    if (ids.some((i) => i.email === r.email)) continue;
    const existing = await env.DB.prepare("SELECT status FROM senders WHERE email = ?").bind(r.email).first<{ status: SenderStatus }>();
    if (existing?.status !== "approved") await setSenderStatus(env, r.email, r.name || null, "approved");
  }

  const [message] = await loadMessages(env, "id = ?", [id]);
  return json({ ok: true, thread_id: threadId, message });
}

export async function handleApi(req: Request, env: Env): Promise<Response> {
  const url = new URL(req.url);
  const path = url.pathname;

  if (path === "/health") return json({ ok: true });
  if (!path.startsWith("/api/")) return error("not found", 404);

  const auth = req.headers.get("authorization") ?? "";
  const token = auth.startsWith("Bearer ") ? auth.slice(7) : "";
  if (!env.API_TOKEN || !token || !timingSafeEqual(token, env.API_TOKEN)) return error("unauthorized", 401);

  const method = req.method;
  let m: RegExpMatchArray | null;

  if (method === "GET" && path === "/api/counts") {
    const row = await env.DB.prepare(
      `SELECT
         (SELECT COUNT(*) FROM senders s WHERE s.status != 'blocked'
            AND EXISTS (SELECT 1 FROM threads t WHERE t.sender_email = s.email AND t.folder = 'screener')) AS screener,
         (SELECT COUNT(*) FROM threads WHERE folder = 'inbox') AS inbox,
         (SELECT COUNT(*) FROM threads WHERE folder = 'inbox' AND unread = 1) AS inbox_unread`,
    ).first<{ screener: number; inbox: number; inbox_unread: number }>();
    return json(row);
  }

  if (method === "GET" && path === "/api/threads") return listThreads(env, url);

  if ((m = path.match(/^\/api\/threads\/([\w-]+)$/))) {
    const id = m[1];
    if (method === "GET") {
      const t = await env.DB.prepare(`SELECT ${THREAD_COLUMNS} FROM threads WHERE id = ?`).bind(id).first<ThreadRow>();
      if (!t) return error("not found", 404);
      return json({ thread: threadSummary(t), messages: await loadMessages(env, "thread_id = ?", [id]) });
    }
    if (method === "DELETE") {
      return (await deleteThread(env, id)) ? json({ ok: true }) : error("not found", 404);
    }
  }

  if (method === "POST" && (m = path.match(/^\/api\/threads\/([\w-]+)\/move$/))) {
    const body = await readJson<{ folder?: string }>(req);
    if (body?.folder !== "inbox" && body?.folder !== "archive") return error("folder must be inbox or archive");
    // Only between Inbox and Archive: a Screener or Blocked thread leaves by deciding on its sender.
    const res = await env.DB.prepare("UPDATE threads SET folder = ? WHERE id = ? AND folder IN ('inbox', 'archive')")
      .bind(body.folder, m[1])
      .run();
    if (res.meta.changes) return json({ ok: true });
    const t = await env.DB.prepare("SELECT folder FROM threads WHERE id = ?").bind(m[1]).first<{ folder: string }>();
    if (!t) return error("not found", 404);
    return error(`this thread is in ${t.folder}; approve or block its sender instead (POST /api/senders/:email)`, 409);
  }

  if (method === "POST" && (m = path.match(/^\/api\/threads\/([\w-]+)\/read$/))) {
    const body = await readJson<{ unread?: boolean }>(req);
    const res = await env.DB.prepare("UPDATE threads SET unread = ? WHERE id = ?").bind(body?.unread ? 1 : 0, m[1]).run();
    return res.meta.changes ? json({ ok: true }) : error("not found", 404);
  }

  if (method === "GET" && path === "/api/screener") {
    const rows = await env.DB.prepare(
      `SELECT s.email, s.name, COUNT(t.id) AS thread_count, MAX(t.last_at) AS last_at,
         (SELECT subject FROM threads t2 WHERE t2.sender_email = s.email AND t2.folder = 'screener'
            ORDER BY t2.last_at DESC LIMIT 1) AS last_subject
       FROM senders s JOIN threads t ON t.sender_email = s.email AND t.folder = 'screener'
       WHERE s.status != 'blocked'
       GROUP BY s.email ORDER BY last_at DESC`,
    ).all();
    return json({ senders: rows.results.map((r) => ({ ...r, name: r.name ?? "" })) });
  }

  if (method === "GET" && path === "/api/senders") {
    const status = url.searchParams.get("status") ?? "approved";
    const rows = await env.DB.prepare(
      "SELECT email, COALESCE(name, '') AS name, status, decided_at FROM senders WHERE status = ? ORDER BY decided_at DESC",
    )
      .bind(status)
      .all();
    return json({ senders: rows.results });
  }

  if (method === "POST" && (m = path.match(/^\/api\/senders\/([^/]+)$/))) {
    const email = normalizeEmail(safeDecode(m[1]));
    if (!email.includes("@")) return error("invalid email address");
    const body = await readJson<{ status?: string }>(req);
    if (body?.status !== "approved" && body?.status !== "blocked") return error("status must be approved or blocked");
    const moved = await setSenderStatus(env, email, null, body.status);
    return json({ ok: true, moved });
  }

  if (method === "POST" && path === "/api/send") return send(env, req);

  if (method === "GET" && (m = path.match(/^\/api\/attachments\/([\w-]+)$/))) {
    const a = await env.DB.prepare("SELECT * FROM attachments WHERE id = ?").bind(m[1]).first<AttachmentRow>();
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
    const row = await env.DB.prepare("SELECT raw_key FROM messages WHERE id = ?").bind(m[1]).first<{ raw_key: string | null }>();
    const obj = row?.raw_key && (await env.BUCKET.get(row.raw_key));
    if (!obj) return error("not found", 404);
    return new Response(obj.body, {
      headers: { "content-type": "message/rfc822", "content-disposition": `attachment; filename="${m[1]}.eml"` },
    });
  }

  if (method === "GET" && path === "/api/identities") {
    const ids = await identities(env);
    return json({ identities: ids, default: ids[0] ?? null });
  }

  if (method === "GET" && path === "/api/mailboxes") return json({ mailboxes: await mailboxes(env) });

  if ((m = path.match(/^\/api\/mailboxes\/([^/]+)$/))) {
    const email = normalizeEmail(safeDecode(m[1]));
    if (method === "PUT") {
      const body = await readJson<{ name?: unknown; screen?: unknown; position?: unknown }>(req);
      if (!body) return error("invalid JSON");
      if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email)) return error("invalid email address");
      // Mail to this address would be forwarded straight back here, forever.
      if ((await getSettings(env)).forward_to === email) return error("this address is the forward_to target; change forward_to first");
      if (body.name !== undefined && typeof body.name !== "string") return error("name must be a string");
      if (body.screen !== undefined && typeof body.screen !== "boolean") return error("screen must be a boolean");
      if (body.position !== undefined && !Number.isInteger(body.position)) return error("position must be an integer");
      const mailbox = await upsertMailbox(env, {
        email,
        name: body.name as string | undefined,
        screen: body.screen as boolean | undefined,
        position: body.position as number | undefined,
      });
      return json({ ok: true, mailbox });
    }
    if (method === "DELETE") return (await deleteMailbox(env, email)) ? json({ ok: true }) : error("not found", 404);
  }

  if (path === "/api/settings") {
    if (method === "GET") return json({ settings: await getSettings(env) });
    if (method === "PATCH") {
      const body = await readJson<Record<string, unknown>>(req);
      if (!body) return error("invalid JSON");
      if (body.forward_to !== undefined) {
        if (typeof body.forward_to !== "string") return error("forward_to must be a string");
        body.forward_to = normalizeEmail(body.forward_to);
        if (body.forward_to && !/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(body.forward_to as string)) return error("forward_to must be an email address");
        // Forwarding to one of your own mailboxes would route straight back here, forever.
        if ((await mailboxes(env)).some((mb) => mb.email === body.forward_to)) return error("forward_to cannot be one of your mailboxes");
      }
      return json({ ok: true, settings: await updateSettings(env, { forward_to: body.forward_to as string | undefined }) });
    }
  }

  return error("not found", 404);
}
