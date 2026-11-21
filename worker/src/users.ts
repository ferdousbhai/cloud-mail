import { mailboxes } from "./mailboxes";
import { newId, timingSafeEqual, toHex } from "./util";

/** The admin: the API_TOKEN secret signs in as this user, who owns everything from before users existed. */
export const OWNER = "owner";

// Long enough to pass on by hand, short enough that she can't forget it in a week.
const INVITE_TTL_MS = 7 * 24 * 60 * 60 * 1000;
export const NAME_RE = /^[a-z0-9][a-z0-9_-]{0,31}$/;

export interface Caller {
  id: string;
  name: string;
  admin: boolean;
  /** The row in `tokens` this request signed in with; null for the API_TOKEN secret. */
  machine: string | null;
}

interface UserRow {
  id: string;
  name: string;
  role: "admin" | "user";
  /** From before each machine had its own token (the `tokens` table); no longer read. */
  token_hash: string | null;
  invite_hash: string | null;
  invite_expires_at: number | null;
  created_at: number;
  revoked_at: number | null;
}

function randomHex(bytes: number): string {
  return toHex(crypto.getRandomValues(new Uint8Array(bytes)));
}

async function sha256(s: string): Promise<string> {
  return toHex(new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(s))));
}

/**
 * Who a bearer token belongs to. Each machine has its own token, looked up by its hash; the
 * API_TOKEN secret also signs in as the owner (the first machine's, from `cloudmail setup`).
 */
export async function authenticate(env: Env, token: string): Promise<Caller | null> {
  if (!token) return null;
  if (env.API_TOKEN && timingSafeEqual(token, env.API_TOKEN)) return { id: OWNER, name: OWNER, admin: true, machine: null };
  const row = await env.DB.prepare(
    "SELECT u.id, u.name, u.role, k.id AS machine FROM tokens k JOIN users u ON u.id = k.user_id WHERE k.token_hash = ? AND u.revoked_at IS NULL",
  )
    .bind(await sha256(token))
    .first<{ id: string; name: string; role: string; machine: string }>();
  return row ? { id: row.id, name: row.name, admin: row.role === "admin", machine: row.machine } : null;
}

export async function findUser(env: Env, name: string): Promise<UserRow | null> {
  return env.DB.prepare("SELECT * FROM users WHERE name = ?").bind(name).first<UserRow>();
}

export interface Invite {
  code: string;
  url: string;
  expires_at: number;
}

/** A new one-time invite for the user, replacing any earlier one; a revoked user is let back in. */
export async function issueInvite(env: Env, userId: string, origin: string): Promise<Invite> {
  const code = `inv_${randomHex(32)}`;
  const expires = Date.now() + INVITE_TTL_MS;
  await env.DB.prepare("UPDATE users SET invite_hash = ?, invite_expires_at = ?, revoked_at = NULL WHERE id = ?")
    .bind(await sha256(code), expires, userId)
    .run();
  return { code, url: `${origin}/invite/${code}`, expires_at: expires };
}

export async function createUser(env: Env, name: string): Promise<string> {
  const id = newId("u");
  await env.DB.prepare("INSERT INTO users (id, name, role, created_at) VALUES (?, ?, 'user', ?)").bind(id, name, Date.now()).run();
  return id;
}

/**
 * Trades an invite code for an API token of the machine's own. The code works once; the user's other
 * machines keep their tokens.
 */
export async function redeemInvite(env: Env, code: string, machine: string): Promise<{ token: string; user: { name: string; role: string } } | null> {
  const hash = await sha256(code);
  const row = await env.DB.prepare("SELECT * FROM users WHERE invite_hash = ? AND revoked_at IS NULL").bind(hash).first<UserRow>();
  if (!row || (row.invite_expires_at ?? 0) < Date.now()) return null;
  const token = `cm_${randomHex(32)}`;
  // Conditional on the invite still being there, so two racing redemptions can't both succeed.
  const res = await env.DB.prepare("UPDATE users SET invite_hash = NULL, invite_expires_at = NULL WHERE id = ? AND invite_hash = ?")
    .bind(row.id, hash)
    .run();
  if (!res.meta.changes) return null;
  await env.DB.prepare("INSERT INTO tokens (id, user_id, token_hash, name, created_at) VALUES (?, ?, ?, ?, ?)")
    .bind(newId("k"), row.id, await sha256(token), machineName(machine), Date.now())
    .run();
  return { token, user: { name: row.name, role: row.role } };
}

/** A machine's name as it's kept: printable, one line, short. */
function machineName(name: string): string {
  return name.replace(/[^\x20-\x7e]/g, "").trim().slice(0, 64);
}

/** The machines signed in as this user, oldest first. The API_TOKEN secret isn't one of them. */
export async function listMachines(env: Env, caller: Caller) {
  const rows = await env.DB.prepare("SELECT id, name, created_at FROM tokens WHERE user_id = ? ORDER BY created_at, id")
    .bind(caller.id)
    .all<{ id: string; name: string; created_at: number }>();
  return rows.results.map((k) => ({ ...k, current: k.id === caller.machine }));
}

/** Signs one of the user's machines out; false when it isn't theirs. */
export async function removeMachine(env: Env, userId: string, id: string): Promise<boolean> {
  const res = await env.DB.prepare("DELETE FROM tokens WHERE id = ? AND user_id = ?").bind(id, userId).run();
  return !!res.meta.changes;
}

export async function listUsers(env: Env) {
  const [rows, boxes] = await Promise.all([
    env.DB.prepare(
      `SELECT u.*, COALESCE(s.value, '') AS forward_to, (SELECT COUNT(*) FROM tokens k WHERE k.user_id = u.id) AS machines FROM users u
       LEFT JOIN settings s ON s.user_id = u.id AND s.key = 'forward_to' ORDER BY u.created_at, u.name`,
    ).all<UserRow & { forward_to: string; machines: number }>(),
    mailboxes(env, null),
  ]);
  const now = Date.now();
  return rows.results.map((u) => ({
    name: u.name,
    role: u.role,
    mailboxes: boxes.filter((b) => b.user_id === u.id).map((b) => b.email),
    // The owner can always sign in with the API_TOKEN secret.
    connected: u.id === OWNER || (u.machines > 0 && !u.revoked_at),
    machines: u.machines,
    invite_expires_at: u.invite_hash && (u.invite_expires_at ?? 0) > now ? u.invite_expires_at : null,
    revoked: !!u.revoked_at,
    created_at: u.created_at,
    forward_to: u.forward_to,
  }));
}

/** Signs a user out everywhere. With `purge`, also deletes their mail, decisions, settings and mailboxes. */
export async function revokeUser(env: Env, user: UserRow, purge: boolean): Promise<void> {
  // Signed out first, so nothing new is written while the mail goes.
  await env.DB.batch([
    env.DB.prepare("UPDATE users SET token_hash = NULL, invite_hash = NULL, invite_expires_at = NULL, revoked_at = ? WHERE id = ?").bind(Date.now(), user.id),
    env.DB.prepare("DELETE FROM tokens WHERE user_id = ?").bind(user.id),
  ]);
  if (!purge) return;
  // All their objects in R2 in one query, then all their rows in one batch: per-thread deletes
  // would run into a Worker's limit on subrequests for anyone with much mail.
  const keys = await env.DB.prepare(
    `SELECT raw_key AS k FROM messages WHERE user_id = ?1 AND raw_key IS NOT NULL
     UNION ALL SELECT html_key FROM messages WHERE user_id = ?1 AND html_key IS NOT NULL
     UNION ALL SELECT text_key FROM messages WHERE user_id = ?1 AND text_key IS NOT NULL
     UNION ALL SELECT a.r2_key FROM attachments a JOIN messages m ON m.id = a.message_id WHERE m.user_id = ?1`,
  )
    .bind(user.id)
    .all<{ k: string }>();
  const all = keys.results.map((r) => r.k);
  for (let i = 0; i < all.length; i += 1000) await env.BUCKET.delete(all.slice(i, i + 1000));
  const theirs = "SELECT id FROM messages WHERE user_id = ?";
  await env.DB.batch([
    env.DB.prepare("DELETE FROM messages_fts WHERE rowid IN (SELECT rowid FROM messages WHERE user_id = ?)").bind(user.id),
    env.DB.prepare(`DELETE FROM attachments WHERE message_id IN (${theirs})`).bind(user.id),
    env.DB.prepare("DELETE FROM messages WHERE user_id = ?").bind(user.id),
    env.DB.prepare("DELETE FROM threads WHERE user_id = ?").bind(user.id),
    env.DB.prepare("DELETE FROM senders WHERE user_id = ?").bind(user.id),
    env.DB.prepare("DELETE FROM settings WHERE user_id = ?").bind(user.id),
    env.DB.prepare("DELETE FROM mailboxes WHERE user_id = ?").bind(user.id),
    env.DB.prepare("DELETE FROM users WHERE id = ?").bind(user.id),
  ]);
}
