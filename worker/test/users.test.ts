import { Database } from "bun:sqlite";
import { beforeEach, expect, test } from "bun:test";
import { handleApi } from "../src/api";
import { handleEmail } from "../src/ingest";
import { d1, migrate, r2, request } from "./support";

let db: Database;
let env: Env;
let forwarded: { to: string; envelope: string }[];

const OWNER_TOKEN = "owner-token";
const ME = "me@example.org";
const SARA = "sara@saraadolfsen.com";

beforeEach(() => {
  db = new Database(":memory:");
  migrate(db);
  db.run(`INSERT INTO mailboxes (email, name, screen, position, created_at) VALUES ('${ME}', 'Me', 1, 0, 0)`);
  forwarded = [];
  env = {
    DB: d1(db),
    BUCKET: r2(),
    API_TOKEN: OWNER_TOKEN,
    EMAIL: { send: async () => ({ messageId: `sent-${crypto.randomUUID()}@example.org` }) },
  } as unknown as Env;
});

async function api<T = Record<string, unknown>>(token: string, method: string, path: string, body?: unknown) {
  const res = await request(env, token, method, path, body);
  return { status: res.status, body: (await res.json()) as T };
}

/** Delivers a message as Email Routing would, once per recipient mailbox. */
async function deliver(to: string, headers: Record<string, string>, text = "hello") {
  const raw = `${Object.entries(headers).map(([k, v]) => `${k}: ${v}`).join("\r\n")}\r\n\r\n${text}\r\n`;
  await handleEmail(
    {
      from: headers.From.match(/<(.+)>/)?.[1] ?? headers.From,
      to,
      raw: new Response(raw).body,
      forward: async (dest: string) => void forwarded.push({ to: dest, envelope: to }),
    } as unknown as ForwardableEmailMessage,
    env,
  );
}

/** Creates sara with her mailbox and returns her token. */
async function inviteSara(): Promise<string> {
  const created = await api<{ invite: { code: string; url: string } }>(OWNER_TOKEN, "POST", "/api/users", { name: "sara", mailboxes: [SARA] });
  expect(created.status).toBe(200);
  const { code, url } = created.body.invite;
  expect(url).toBe(`https://cloudmail.test.workers.dev/invite/${code}`);
  const connected = await api<{ token: string; user: { name: string } }>("", "POST", "/api/connect", { code });
  expect(connected.status).toBe(200);
  expect(connected.body.user.name).toBe("sara");
  return connected.body.token;
}

type Threads = { threads: { id: string; subject: string }[] };

test("the migration gives everything from before to the owner", () => {
  const old = new Database(":memory:");
  migrate(old, { until: "0006" });
  old.run("INSERT INTO mailboxes (email, name, screen, position, created_at) VALUES ('a@x.org', '', 1, 0, 0)");
  old.run("INSERT INTO senders (email, status, created_at) VALUES ('b@y.org', 'approved', 0)");
  old.run("INSERT INTO settings (key, value) VALUES ('forward_to', 'c@z.org')");
  old.run("INSERT INTO threads (id, subject, folder, last_at) VALUES ('t_1', 's', 'inbox', 0)");
  old.run("INSERT INTO messages (id, thread_id, from_email, subject, date, message_id) VALUES ('m_1', 't_1', 'b@y.org', 's', 0, '<1@y>')");
  migrate(old, { from: "0006" });
  for (const table of ["mailboxes", "senders", "settings", "threads", "messages"]) {
    expect(old.query(`SELECT DISTINCT user_id FROM ${table}`).all()).toEqual([{ user_id: "owner" }]);
  }
  expect(old.query("SELECT email, status FROM senders").all()).toEqual([{ email: "b@y.org", status: "approved" }]);
});

test("the API_TOKEN secret is the admin; an invite gives someone their own token, once", async () => {
  expect((await api(OWNER_TOKEN, "GET", "/api/me")).body).toEqual({ user: { name: "owner", role: "admin" } });
  const created = await api<{ invite: { code: string } }>(OWNER_TOKEN, "POST", "/api/users", { name: "sara", mailboxes: [SARA] });
  const { code } = created.body.invite;
  const first = await api<{ token: string }>("", "POST", "/api/connect", { code });
  expect(first.status).toBe(200);
  expect((await api(first.body.token, "GET", "/api/me")).body).toEqual({ user: { name: "sara", role: "user" } });
  expect((await api("", "POST", "/api/connect", { code })).status).toBe(403);
  expect((await api("", "POST", "/api/connect", { code: "inv_nope" })).status).toBe(403);
  // Only hashes are kept.
  expect(JSON.stringify(db.query("SELECT * FROM users").all())).not.toContain(first.body.token);
});

test("an expired invite doesn't work, and a new one replaces the old token", async () => {
  const created = await api<{ invite: { code: string } }>(OWNER_TOKEN, "POST", "/api/users", { name: "sara" });
  db.run("UPDATE users SET invite_expires_at = 1 WHERE name = 'sara'");
  expect((await api("", "POST", "/api/connect", { code: created.body.invite.code })).status).toBe(403);

  // Inviting someone again gives them a new link.
  const again = await api<{ invite: { code: string } }>(OWNER_TOKEN, "POST", "/api/users", { name: "sara" });
  const old = (await api<{ token: string }>("", "POST", "/api/connect", { code: again.body.invite.code })).body.token;
  const next = await api<{ invite: { code: string } }>(OWNER_TOKEN, "POST", "/api/users", { name: "sara" });
  const token = (await api<{ token: string }>("", "POST", "/api/connect", { code: next.body.invite.code })).body.token;
  expect((await api(old, "GET", "/api/me")).status).toBe(401);
  expect((await api(token, "GET", "/api/me")).status).toBe(200);
});

test("each user sees only their own mail", async () => {
  const sara = await inviteSara();
  await deliver(ME, { From: "Ann <ann@example.net>", To: ME, Subject: "for owner", "Message-ID": "<1@example.net>" }, "owner secret");
  await deliver(SARA, { From: "Bob <bob@example.net>", To: SARA, Subject: "for sara", "Message-ID": "<2@example.net>" }, "sara secret");

  const mine = (await api<Threads>(OWNER_TOKEN, "GET", "/api/threads?folder=screener")).body.threads;
  const hers = (await api<Threads>(sara, "GET", "/api/threads?folder=screener")).body.threads;
  expect(mine.map((t) => t.subject)).toEqual(["for owner"]);
  expect(hers.map((t) => t.subject)).toEqual(["for sara"]);

  // Someone else's thread, message and raw mail don't exist for you.
  expect((await api(sara, "GET", `/api/threads/${mine[0].id}`)).status).toBe(404);
  expect((await api(sara, "POST", `/api/threads/${mine[0].id}/read`, { unread: false })).status).toBe(404);
  expect((await api(sara, "DELETE", `/api/threads/${mine[0].id}`)).status).toBe(404);
  const ownerMessage = db.query("SELECT id FROM messages WHERE thread_id = ?").get(mine[0].id) as { id: string };
  expect((await api(sara, "GET", `/api/messages/${ownerMessage.id}/raw`)).status).toBe(404);

  // Search, the Screener, counts and identities.
  expect((await api<Threads>(sara, "GET", "/api/threads?q=secret")).body.threads.map((t) => t.subject)).toEqual(["for sara"]);
  expect((await api<{ senders: { email: string }[] }>(sara, "GET", "/api/screener")).body.senders.map((s) => s.email)).toEqual(["bob@example.net"]);
  expect((await api(sara, "GET", "/api/counts")).body).toEqual({ screener: 1, inbox: 0, inbox_unread: 0 });
  expect((await api<{ identities: { email: string }[] }>(sara, "GET", "/api/identities")).body.identities.map((i) => i.email)).toEqual([SARA]);

  // Screening is per user: Sara approving Ann changes nothing for the owner.
  expect((await api(sara, "POST", "/api/senders/ann%40example.net", { status: "approved" })).status).toBe(200);
  expect((await api<Threads>(OWNER_TOKEN, "GET", "/api/threads?folder=screener")).body.threads).toHaveLength(1);
});

test("mail to both users is stored for each, and a reply can't join the other's thread", async () => {
  const sara = await inviteSara();
  const headers = { From: "Ann <ann@example.net>", To: `${ME}, ${SARA}`, Subject: "to both", "Message-ID": "<both@example.net>" };
  await deliver(ME, headers);
  await deliver(SARA, headers);
  const mine = (await api<Threads>(OWNER_TOKEN, "GET", "/api/threads?folder=screener")).body.threads;
  const hers = (await api<Threads>(sara, "GET", "/api/threads?folder=screener")).body.threads;
  expect(mine).toHaveLength(1);
  expect(hers).toHaveLength(1);
  expect(mine[0].id).not.toBe(hers[0].id);

  // Approved by the owner only; a reply to the owner's thread reaches Sara as her own new mail.
  await api(OWNER_TOKEN, "POST", "/api/senders/ann%40example.net", { status: "approved" });
  await deliver(SARA, { From: "Ann <ann@example.net>", To: SARA, Subject: "Re: to both", "Message-ID": "<r@example.net>", "In-Reply-To": "<both@example.net>" });
  const thread = db.query("SELECT thread_id FROM messages WHERE message_id = '<r@example.net>'").get() as { thread_id: string };
  expect(thread.thread_id).toBe(hers[0].id);
});

test("sending is from your own mailboxes, and replies only to your own messages", async () => {
  const sara = await inviteSara();
  expect((await api(sara, "POST", "/api/send", { from: ME, to: "x@example.net", text: "hi" })).status).toBe(400);
  const ok = await api<{ thread_id: string }>(sara, "POST", "/api/send", { to: "Ann <ann@example.net>", text: "hi" });
  expect(ok.status).toBe(200);
  expect((await api<Threads>(OWNER_TOKEN, "GET", "/api/threads?folder=sent")).body.threads).toHaveLength(0);
  // Writing to Ann screened her in for Sara, not for the owner.
  expect((await api<{ senders: unknown[] }>(sara, "GET", "/api/senders")).body.senders).toHaveLength(1);
  expect((await api<{ senders: unknown[] }>(OWNER_TOKEN, "GET", "/api/senders")).body.senders).toHaveLength(0);

  const mineSent = await api<{ message: { id: string } }>(OWNER_TOKEN, "POST", "/api/send", { to: "x@example.net", text: "mine" });
  expect((await api(sara, "POST", "/api/send", { to: "x@example.net", text: "re", reply_to_message_id: mineSent.body.message.id })).status).toBe(404);
});

test("forwarding is per user", async () => {
  const sara = await inviteSara();
  await api(sara, "PATCH", "/api/settings", { forward_to: "sara@gmail.example" });
  await deliver(SARA, { From: "a@example.net", To: SARA, Subject: "s" });
  await deliver(ME, { From: "a@example.net", To: ME, Subject: "s" });
  expect(forwarded).toEqual([{ to: "sara@gmail.example", envelope: SARA }]);
  expect((await api<{ settings: { forward_to: string } }>(OWNER_TOKEN, "GET", "/api/settings?user=sara")).body.settings.forward_to).toBe("sara@gmail.example");
  expect((await api(sara, "GET", "/api/settings?user=owner")).status).toBe(403);
  const listed = (await api<{ users: { name: string; forward_to: string }[] }>(OWNER_TOKEN, "GET", "/api/users")).body.users;
  expect(listed.map(({ name, forward_to }) => ({ name, forward_to }))).toEqual([
    { name: "owner", forward_to: "" },
    { name: "sara", forward_to: "sara@gmail.example" },
  ]);
});

test("only the admin manages users and adds or gives away mailboxes", async () => {
  const sara = await inviteSara();
  expect((await api(sara, "GET", "/api/users")).status).toBe(403);
  expect((await api(sara, "POST", "/api/users", { name: "eve" })).status).toBe(403);
  expect((await api(sara, "PUT", "/api/mailboxes/new%40saraadolfsen.com", {})).status).toBe(403);
  expect((await api(sara, "PUT", `/api/mailboxes/${encodeURIComponent(ME)}`, { name: "mine now" })).status).toBe(403);
  expect((await api(sara, "PUT", `/api/mailboxes/${encodeURIComponent(SARA)}`, { name: "Sara", screen: false })).status).toBe(200);
  expect((await api(sara, "DELETE", `/api/mailboxes/${encodeURIComponent(SARA)}`)).status).toBe(403);
  expect((await api(OWNER_TOKEN, "POST", "/api/users", { name: "owner" })).status).toBe(400);
  // Inviting an existing user again adds mailboxes; their token works until the new link is used.
  const again = await api<{ user: { mailboxes: string[] } }>(OWNER_TOKEN, "POST", "/api/users", { name: "sara", mailboxes: ["hi@saraadolfsen.com"] });
  expect(again.body.user.mailboxes.sort()).toEqual(["hi@saraadolfsen.com", SARA]);
  expect((await api(sara, "GET", "/api/me")).status).toBe(200);
  expect((await api(OWNER_TOKEN, "POST", "/api/users", { name: "Not Valid!" })).status).toBe(400);

  const listed = (await api<{ users: { name: string; mailboxes: string[]; connected: boolean }[] }>(OWNER_TOKEN, "GET", "/api/users")).body.users;
  expect(listed.map(({ name, mailboxes, connected }) => ({ name, mailboxes, connected }))).toEqual([
    { name: "owner", mailboxes: [ME], connected: true },
    { name: "sara", mailboxes: [SARA, "hi@saraadolfsen.com"], connected: true },
  ]);
});

test("giving a mailbox to someone takes its threads along", async () => {
  await deliver("hello@forbiddenfeminine.com", { From: "a@example.net", To: "hello@forbiddenfeminine.com", Subject: "early" });
  const sara = await inviteSara();
  expect((await api<Threads>(sara, "GET", "/api/threads?folder=all")).body.threads).toHaveLength(0);
  await api(OWNER_TOKEN, "PUT", "/api/mailboxes/hello%40forbiddenfeminine.com", { user: "sara" });
  expect((await api<Threads>(sara, "GET", "/api/threads?folder=all")).body.threads.map((t) => t.subject)).toEqual(["early"]);
  expect((await api<Threads>(OWNER_TOKEN, "GET", "/api/threads?folder=all")).body.threads).toHaveLength(0);
});

test("revoking signs someone out; purging also deletes their mail", async () => {
  const sara = await inviteSara();
  await deliver(SARA, { From: "a@example.net", To: SARA, Subject: "s" });
  expect((await api(OWNER_TOKEN, "DELETE", "/api/users/sara")).status).toBe(200);
  expect((await api(sara, "GET", "/api/me")).status).toBe(401);
  expect(db.query("SELECT COUNT(*) AS n FROM threads WHERE user_id != 'owner'").get()).toEqual({ n: 1 });

  expect((await api(OWNER_TOKEN, "DELETE", "/api/users/sara?purge=1")).status).toBe(200);
  expect(db.query("SELECT COUNT(*) AS n FROM threads WHERE user_id != 'owner'").get()).toEqual({ n: 0 });
  expect(db.query("SELECT COUNT(*) AS n FROM mailboxes WHERE email = ?").get(SARA)).toEqual({ n: 0 });
  expect((await api(OWNER_TOKEN, "DELETE", "/api/users/sara")).status).toBe(404);
});

test("an invite link opened in a browser says what to do", async () => {
  const res = await handleApi(new Request("https://cloudmail.test.workers.dev/invite/inv_abc"), env);
  expect(await res.text()).toContain("cloudmail connect https://cloudmail.test.workers.dev/invite/inv_abc");
});
