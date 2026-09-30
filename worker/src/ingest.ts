import PostalMime from "postal-mime";
import { deleteThread, type Folder, type NewAttachment, type NewMessage, senderStatus, setSenderStatus, storeMessage, type ThreadRef } from "./store";
import { findMailbox, getSettings, type Mailbox, mailboxes } from "./mailboxes";
import { flattenAddresses, messageIdList, normalizeEmail, normalizeMessageId, toArrayBuffer } from "./util";

export async function handleEmail(message: ForwardableEmailMessage, env: Env): Promise<void> {
  const envelopeTo = normalizeEmail(message.to);

  const [boxes, settings] = await Promise.all([
    mailboxes(env).catch(() => [] as Mailbox[]),
    getSettings(env).catch(() => ({ forward_to: "" })),
  ]);

  // Optional copy to another (verified) address, e.g. during migration. Forward before touching raw.
  if (settings.forward_to) {
    try {
      await message.forward(settings.forward_to);
    } catch (err) {
      console.error("forward failed", err);
    }
  }

  const raw = await new Response(message.raw).arrayBuffer();
  try {
    await ingest(raw, envelopeTo, normalizeEmail(message.from), boxes, env);
  } catch (err) {
    // Never bounce mail because of a storage bug; keep the original for reprocessing.
    console.error("ingest failed", err);
    try {
      await env.BUCKET.put(`failed/${Date.now()}-${crypto.randomUUID()}.eml`, raw);
    } catch (putErr) {
      console.error("could not keep the failed message either", putErr);
    }
  }
}

interface StoredCopy {
  thread_id: string;
  folder: Folder;
  from_email: string;
  auth: string | null;
  message_count: number;
}

export interface AuthVerdict {
  dmarc: string | null;
  spf: string | null;
  dkim: string | null;
  spam_score: number | null;
}

/**
 * Reads the verdicts Cloudflare's MX recorded. Its Authentication-Results header is prepended above any
 * the sender supplied, so only the first one from mx.cloudflare.net is trusted.
 */
export function authVerdict(headers: { key: string; value: string }[]): AuthVerdict | null {
  const ar = headers.find((h) => h.key === "authentication-results" && /^\s*mx\.cloudflare\.net\s*;/i.test(h.value));
  const spam = headers.find((h) => h.key === "x-cf-spamh-score");
  if (!ar && !spam) return null;
  // "mx.cloudflare.net; dkim=pass header.b=…; dmarc=pass …; spf=pass …": each result is its own
  // ;-separated clause and starts with method=. Values the sender controls (header.b, header.s)
  // sit inside clauses, so a match anywhere else in the line could be forged.
  const clauses = (ar?.value ?? "").split(";").slice(1).map((c) => c.trim());
  const pick = (method: string) =>
    clauses.map((c) => c.match(new RegExp(`^${method}=([a-z]+)\\b`, "i"))?.[1].toLowerCase()).find((v) => v) ?? null;
  const score = spam ? Number.parseFloat(spam.value) : NaN;
  return { dmarc: pick("dmarc"), spf: pick("spf"), dkim: pick("dkim"), spam_score: Number.isFinite(score) ? score : null };
}

async function ingest(raw: ArrayBuffer, envelopeTo: string, envelopeFrom: string, boxes: Mailbox[], env: Env): Promise<void> {
  const parsed = await PostalMime.parse(raw, { attachmentEncoding: "arraybuffer" });
  const messageId = normalizeMessageId(parsed.messageId);

  // One message can arrive several times: once per recipient mailbox (often concurrently), or
  // directly and through a list. A unique index keeps one stored copy; see absorbDuplicate.
  const findEarlier = () =>
    messageId
      ? env.DB.prepare(
          `SELECT m.thread_id, t.folder, m.from_email, m.auth, t.message_count FROM messages m
           JOIN threads t ON t.id = m.thread_id WHERE m.message_id = ? AND m.outgoing = 0 LIMIT 1`,
        )
          .bind(messageId)
          .first<StoredCopy>()
      : Promise.resolve(null);
  const earlier = await findEarlier();

  const from = flattenAddresses(parsed.from)[0] ?? { name: "", email: envelopeFrom };
  // Unknown recipients (e.g. a routing rule added without a mailbox entry) are screened.
  const screened = findMailbox(boxes, envelopeTo)?.screen ?? true;

  const auth = authVerdict(parsed.headers);
  // The From header is only as good as DMARC: a failing message can't ride on an approval.
  // Anything but a clear pass or none (no policy published) is untrusted: fail, temperror, permerror, junk.
  const spoofable = !!auth?.dmarc && !["pass", "none"].includes(auth.dmarc);

  const ownAddress = !!findMailbox(boxes, from.email);
  let status = ownAddress ? "approved" : await senderStatus(env, from.email);
  // Mail claiming to be from one of your own addresses that fails DMARC is forged: quarantine it
  // (the Screener lists senders to decide on, and you are not one to decide on).
  if (ownAddress && spoofable) status = "blocked";
  if (!status && screened) {
    await setSenderStatus(env, from.email, from.name || null, "pending");
    status = "pending";
  }

  const newThreadFolder: Folder =
    status === "blocked"
      ? "blocked"
      : !screened
        ? "inbox"
        : status === "approved" && !spoofable
          ? "inbox"
          : "screener";
  // Already stored. Returns true when this copy should be stored after all.
  const absorbDuplicate = async (stored: StoredCopy): Promise<boolean> => {
    const storedDmarc = stored.auth ? (JSON.parse(stored.auth) as AuthVerdict).dmarc : null;
    const storedUntrusted = !!storedDmarc && !["pass", "none"].includes(storedDmarc);
    // A copy claiming the same sender that failed DMARC got here first (say, forged from a list
    // copy) and this one passes: the forgery is alone in its thread (untrusted mail never joins
    // one), so replace it. A different sender reusing the Message-ID never replaces anything.
    if (storedUntrusted && !spoofable && stored.message_count === 1 && stored.from_email === from.email) {
      await deleteThread(env, stored.thread_id);
      return true;
    }
    // The same sender's more trusted copy (to a direct mailbox, or passing DMARC while approved)
    // lifts the stored one out of the Screener. Neither copy may have failed DMARC.
    // An approved sender's copy also counts when the stored one came from elsewhere, e.g. a list
    // that rewrote From ("Alice via Group"); a merely unscreened sender can't lift others' mail.
    const liftsIt = stored.from_email === from.email || status === "approved";
    if (!storedUntrusted && !spoofable && liftsIt && stored.folder === "screener" && newThreadFolder === "inbox") {
      await env.DB.prepare("UPDATE threads SET folder = 'inbox', unread = 1 WHERE id = ? AND folder = 'screener'")
        .bind(stored.thread_id)
        .run();
    }
    return false;
  };
  if (earlier && !(await absorbDuplicate(earlier))) return;

  // Only a sender you'd let in anyway may join an existing conversation. Otherwise quoting a known
  // Message-ID would carry a blocked, unscreened or forged sender past the Screener.
  const mayJoinThread = (thread: ThreadRef): boolean => {
    if (status === "blocked" || spoofable) return false;
    const own = thread.sender_email === from.email;
    // A sender still waiting in the Screener only adds to their own Screener thread; joining
    // anything already in the Inbox or Archive would skip the Screener.
    if (screened && status !== "approved") return own && thread.folder === "screener";
    // Otherwise: their own conversation, or someone else's once it's yours to read. Joining a
    // thread still in the Screener or in Blocked would hide this mail there, and blocking that
    // sender would take it along.
    return own || thread.folder === "inbox" || thread.folder === "archive";
  };
  const existingThreadFolder = (current: Folder): Folder => {
    if (status === "blocked") return current;
    // A reply on an archived conversation brings it back.
    if (current === "archive") return "inbox";
    return current;
  };

  const attachments: NewAttachment[] = parsed.attachments.map((a, i) => {
    const contentId = a.contentId ? a.contentId.replace(/^<|>$/g, "") : null;
    return {
      filename: a.filename || `attachment-${i + 1}`,
      mimeType: a.mimeType || "application/octet-stream",
      content: toArrayBuffer(a.content),
      contentId,
      inline: a.disposition === "inline" && !!contentId && !!parsed.html?.includes(`cid:${contentId}`),
    };
  });

  const parsedDate = parsed.date ? Date.parse(parsed.date) : NaN;
  const now = Date.now();
  const date = Number.isFinite(parsedDate) && parsedDate < now + 86_400_000 ? parsedDate : now;

  let stored: { id: string; threadId: string };
  const message: NewMessage = {
    outgoing: false,
    messageId,
    inReplyTo: normalizeMessageId(parsed.inReplyTo),
    refs: messageIdList(parsed.references),
    from,
    to: flattenAddresses(parsed.to),
    cc: flattenAddresses(parsed.cc),
    replyTo: flattenAddresses(parsed.replyTo),
    envelopeTo,
    subject: parsed.subject?.trim() || "(no subject)",
    text: parsed.text ?? null,
    html: parsed.html ?? null,
    date,
    raw,
    auth,
    attachments,
    newThreadFolder,
    existingThreadFolder,
    joinThread: mayJoinThread,
  };
  try {
    stored = await storeMessage(env, message);
  } catch (err) {
    // A concurrent copy won the unique index: it stands, is lifted, or (if forged) is replaced.
    const winner = /UNIQUE/i.test(String(err)) ? await findEarlier() : null;
    if (!winner) throw err;
    if (!(await absorbDuplicate(winner))) return;
    stored = await storeMessage(env, message);
  }

  // A decision made while this message was being stored applies to it too: setSenderStatus moved
  // the sender's threads before this one existed (or before it went back to the Screener).
  const nowStatus = screened && !ownAddress ? await senderStatus(env, from.email) : null;
  if (nowStatus === "blocked") {
    await env.DB.prepare("UPDATE threads SET folder = 'blocked', unread = 0 WHERE id = ? AND folder = 'screener'")
      .bind(stored.threadId)
      .run();
  } else if (nowStatus === "approved" && !spoofable) {
    await env.DB.prepare("UPDATE threads SET folder = 'inbox' WHERE id = ? AND folder = 'screener' AND sender_email = ?")
      .bind(stored.threadId, from.email)
      .run();
  }
}
