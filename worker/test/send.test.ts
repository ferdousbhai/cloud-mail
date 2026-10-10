import { Database } from "bun:sqlite";
import { beforeEach, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { encodedSize, handleApi } from "../src/api";

// workerd has crypto.subtle.timingSafeEqual; Bun doesn't.
const subtle = crypto.subtle as SubtleCrypto & { timingSafeEqual?: (a: Uint8Array, b: Uint8Array) => boolean };
subtle.timingSafeEqual ??= (a, b) => a.length === b.length && a.every((x, i) => x === b[i]);

/** D1 over an in-memory SQLite with the worker's migrations applied. */
function d1(db: Database) {
  const exec = (sql: string, args: unknown[]) => {
    const q = db.query(sql);
    if (/^\s*(SELECT|WITH)\b/i.test(sql)) return { results: q.all(...(args as never[])), success: true, meta: { changes: 0 } };
    const r = q.run(...(args as never[]));
    return { results: [], success: true, meta: { changes: r.changes } };
  };
  const statement = (sql: string, args: unknown[] = []) => ({
    sql,
    args,
    bind: (...a: unknown[]) => statement(sql, a),
    first: async (col?: string) => {
      const row = (db.query(sql).get(...(args as never[])) as Record<string, unknown> | null) ?? null;
      return col ? (row?.[col] ?? null) : row;
    },
    all: async () => exec(sql, args),
    run: async () => exec(sql, args),
  });
  return {
    prepare: (sql: string) => statement(sql),
    batch: async (stmts: ReturnType<typeof statement>[]) => db.transaction(() => stmts.map((s) => exec(s.sql, s.args)))(),
  };
}

function r2() {
  const objects = new Map<string, { bytes: Uint8Array; type?: string }>();
  return {
    objects,
    put: async (key: string, value: ArrayBuffer | Uint8Array | string, opts?: { httpMetadata?: { contentType?: string } }) => {
      const bytes = typeof value === "string" ? new TextEncoder().encode(value) : new Uint8Array(value instanceof Uint8Array ? value : new Uint8Array(value));
      objects.set(key, { bytes: new Uint8Array(bytes), type: opts?.httpMetadata?.contentType });
    },
    get: async (key: string) => {
      const o = objects.get(key);
      if (!o) return null;
      return { body: o.bytes, arrayBuffer: async () => o.bytes.slice().buffer, text: async () => new TextDecoder().decode(o.bytes) };
    },
    delete: async (keys: string | string[]) => {
      for (const k of Array.isArray(keys) ? keys : [keys]) objects.delete(k);
    },
  };
}

let db: Database;
let bucket: ReturnType<typeof r2>;
let sent: Record<string, unknown>[];
let sendError: { code: string; message: string } | null;
let env: Env;

beforeEach(() => {
  db = new Database(":memory:");
  const dir = join(import.meta.dir, "..", "migrations");
  for (const f of readdirSync(dir).filter((f) => f.endsWith(".sql")).sort()) db.exec(readFileSync(join(dir, f), "utf8"));
  db.run("INSERT INTO mailboxes (email, name, screen, position, created_at) VALUES ('me@example.org', 'Me', 1, 0, 0)");
  bucket = r2();
  sent = [];
  sendError = null;
  env = {
    DB: d1(db),
    BUCKET: bucket,
    API_TOKEN: "test-token",
    EMAIL: {
      send: async (m: Record<string, unknown>) => {
        if (sendError) throw sendError;
        sent.push(m);
        return { messageId: `sent-${sent.length}@example.org` };
      },
    },
  } as unknown as Env;
});

function api(method: string, path: string, body?: unknown, headers: Record<string, string> = {}) {
  return handleApi(
    new Request(`https://worker.example${path}`, {
      method,
      headers: { authorization: "Bearer test-token", "content-type": "application/json", ...headers },
      body: body === undefined ? undefined : JSON.stringify(body),
    }),
    env,
  );
}

const b64 = (s: string | Uint8Array) => Buffer.from(s).toString("base64");

test("sends attachments through the binding and keeps them in Sent", async () => {
  const pdf = new Uint8Array([0x25, 0x50, 0x44, 0x46, 0x2d, 0x00, 0xff, 0x10]);
  const res = await api("POST", "/api/send", {
    to: ["Ann <ann@example.net>"],
    subject: "Report",
    text: "Attached.",
    attachments: [
      { filename: "report.pdf", mime_type: "application/pdf", content: b64(pdf) },
      { filename: "../notes\r\n.txt", mime_type: "Bad Type", content: b64("hello") },
    ],
  });
  expect(res.status).toBe(200);
  const out = (await res.json()) as { thread_id: string; message: { id: string; attachments: { id: string; filename: string; mime_type: string; size: number; inline: boolean }[] } };

  expect(sent).toHaveLength(1);
  // The binding gets the bytes themselves: a string content is sent as the file's literal text.
  expect(sent[0].attachments).toEqual([
    { disposition: "attachment", filename: "report.pdf", type: "application/pdf", content: pdf },
    { disposition: "attachment", filename: ".._notes.txt", type: "application/octet-stream", content: new TextEncoder().encode("hello") },
  ]);

  const atts = out.message.attachments;
  expect(atts.map((a) => [a.filename, a.mime_type, a.size, a.inline])).toEqual([
    ["report.pdf", "application/pdf", 8, false],
    [".._notes.txt", "application/octet-stream", 5, false],
  ]);
  const download = await api("GET", `/api/attachments/${atts[0].id}`);
  expect(download.headers.get("content-type")).toBe("application/pdf");
  expect(new Uint8Array(await download.arrayBuffer())).toEqual(pdf);

  const thread = (await (await api("GET", `/api/threads/${out.thread_id}`)).json()) as { thread: { has_attachments: boolean } };
  expect(thread.thread.has_attachments).toBe(true);
  const listed = (await (await api("GET", "/api/threads?folder=sent")).json()) as { threads: { has_attachments: boolean }[] };
  expect(listed.threads[0].has_attachments).toBe(true);
});

test("a message without attachments sends none", async () => {
  const res = await api("POST", "/api/send", { to: ["ann@example.net"], subject: "Hi", text: "Hello" });
  expect(res.status).toBe(200);
  expect(sent[0].attachments).toBeUndefined();
  expect(bucket.objects.size).toBe(0);
  const listed = (await (await api("GET", "/api/threads?folder=sent")).json()) as { threads: { has_attachments: boolean }[] };
  expect(listed.threads[0].has_attachments).toBe(false);
});

test("rejects malformed attachments before sending", async () => {
  const base = { to: ["ann@example.net"], subject: "Hi", text: "Hello" };
  for (const [attachments, message] of [
    [{}, "attachments must be an array"],
    [[{ mime_type: "text/plain", content: "aGk=" }], "attachments[0].filename is required"],
    [[{ filename: "a.txt", content: 5 }], "attachments[0].content must be a base64 string"],
    [[{ filename: "a.txt", content: "not base64!" }], "attachments[0].content is not valid base64"],
    [[{ filename: "a.txt", content: "aGk" }], "attachments[0].content is not valid base64"],
    [Array.from({ length: 33 }, () => ({ filename: "a.txt", content: "aGk=" })), "too many attachments (33); at most 32"],
  ] as const) {
    const res = await api("POST", "/api/send", { ...base, attachments });
    expect(res.status).toBe(400);
    expect(((await res.json()) as { error: string }).error).toBe(message);
  }
  expect(sent).toHaveLength(0);
});

test("refuses attachments past the Email Service's 5 MiB message limit", async () => {
  const big = new Uint8Array(3_900_000);
  const res = await api("POST", "/api/send", {
    to: ["ann@example.net"],
    subject: "Big",
    text: "Too big",
    attachments: [{ filename: "big.bin", mime_type: "application/octet-stream", content: b64(big) }],
  });
  expect(res.status).toBe(413);
  expect(((await res.json()) as { error: string }).error).toMatch(/^the attachments are too large \(3\.7 MiB\): .* at most 5\.0 MiB per message once encoded, which leaves about 3\.\d MiB for attachments$/);
  expect(sent).toHaveLength(0);

  // Just under the limit goes out.
  const fits = new Uint8Array(3_600_000);
  const ok = await api("POST", "/api/send", {
    to: ["ann@example.net"],
    subject: "Fits",
    text: "Fits",
    attachments: [{ filename: "fits.bin", mime_type: "application/octet-stream", content: b64(fits) }],
  });
  expect(ok.status).toBe(200);
  expect(encodedSize(3_600_000)).toBeLessThan(5 * 1024 * 1024);
});

test("a request far past the limit isn't read", async () => {
  const res = await api("POST", "/api/send", { to: ["ann@example.net"], text: "x" }, { "content-length": String(11 * 1024 * 1024) });
  expect(res.status).toBe(413);
  expect(sent).toHaveLength(0);
});

test("the Email Service's own size refusal is a 413, not a gateway error", async () => {
  sendError = { code: "E_CONTENT_TOO_LARGE", message: "Email content exceeds size limit" };
  const res = await api("POST", "/api/send", {
    to: ["ann@example.net"],
    subject: "Hi",
    text: "Hello",
    attachments: [{ filename: "a.txt", mime_type: "text/plain", content: "aGk=" }],
  });
  expect(res.status).toBe(413);
  expect(bucket.objects.size).toBe(0);
});

test("a reply with an attachment joins the thread", async () => {
  const first = (await (await api("POST", "/api/send", { to: ["ann@example.net"], subject: "Plan", text: "v1" })).json()) as {
    thread_id: string;
    message: { id: string };
  };
  const res = await api("POST", "/api/send", {
    to: ["ann@example.net"],
    subject: "Re: Plan",
    text: "v2 attached",
    reply_to_message_id: first.message.id,
    attachments: [{ filename: "plan.txt", mime_type: "text/plain", content: b64("v2") }],
  });
  const out = (await res.json()) as { thread_id: string };
  expect(out.thread_id).toBe(first.thread_id);
  expect((sent[1].headers as Record<string, string>)["In-Reply-To"]).toBe("<sent-1@example.org>");
  const thread = (await (await api("GET", `/api/threads/${first.thread_id}`)).json()) as {
    thread: { has_attachments: boolean };
    messages: { attachments: unknown[] }[];
  };
  expect(thread.thread.has_attachments).toBe(true);
  expect(thread.messages.map((m) => m.attachments.length)).toEqual([0, 1]);
});

test("linked accounts' senders: looked up and decided in bulk, a no kept when screening in", async () => {
  expect((await api("POST", "/api/senders/ann%40x.com", { status: "blocked" })).status).toBe(200);
  let r = await api("POST", "/api/senders/batch", {
    senders: [{ email: "Ann@X.com", name: "Ann" }, { email: "bob@y.com", name: "Bob" }, { email: "nope" }],
    status: "approved",
    only_undecided: true,
  });
  expect(await r.json()).toEqual({ ok: true, changed: 1 });
  r = await api("POST", "/api/senders/lookup", { emails: ["ANN@x.com", "bob@y.com", "carol@z.com"] });
  const found = ((await r.json()) as { senders: { email: string; status: string }[] }).senders;
  expect(found.sort((a, b) => a.email.localeCompare(b.email))).toEqual([
    { email: "ann@x.com", status: "blocked" },
    { email: "bob@y.com", status: "approved" },
  ]);
  r = await api("POST", "/api/senders/batch", { senders: [{ email: "ann@x.com" }], status: "approved" });
  expect(await r.json()).toEqual({ ok: true, changed: 1 });
  expect((await api("POST", "/api/senders/batch", { senders: [], status: "maybe" })).status).toBe(400);
});
