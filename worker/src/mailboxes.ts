import { OWNER } from "./users";
import { type Address, normalizeEmail } from "./util";

export interface Mailbox extends Address {
  screen: boolean;
  position: number;
  /** The user whose mail this is (users.id) and their name. */
  user_id: string;
  user: string;
}

interface Settings {
  forward_to: string;
}

export const DEFAULT_SETTINGS: Settings = { forward_to: "" };

/** One user's mailboxes, or with `null` everyone's (incoming mail is sorted by them). */
export async function mailboxes(env: Env, userId: string | null): Promise<Mailbox[]> {
  const rows = await env.DB.prepare(
    `SELECT m.email, m.name, m.screen, m.position, m.user_id, u.name AS user FROM mailboxes m JOIN users u ON u.id = m.user_id
     WHERE ?1 IS NULL OR m.user_id = ?1 ORDER BY m.position, m.created_at`,
  )
    .bind(userId)
    .all<{ email: string; name: string; screen: number; position: number; user_id: string; user: string }>();
  return rows.results.map((r) => ({ ...r, screen: !!r.screen }));
}

export function findMailbox(list: Mailbox[], email: string): Mailbox | undefined {
  const e = normalizeEmail(email);
  return list.find((m) => m.email === e);
}

/**
 * Creates or updates a mailbox. Giving it to another user takes the threads addressed to it along
 * (their screening decisions stay with the previous owner). Mail to an address without a mailbox
 * went to the owner, so a new mailbox takes the owner's.
 */
export async function upsertMailbox(
  env: Env,
  input: { email: string; userId: string; name?: string; screen?: boolean; position?: number },
): Promise<Mailbox> {
  const email = normalizeEmail(input.email);
  const existing = findMailbox(await mailboxes(env, null), email);
  const next = {
    email,
    user_id: input.userId,
    name: input.name ?? existing?.name ?? "",
    screen: input.screen ?? existing?.screen ?? true,
    position: input.position ?? (existing?.user_id === input.userId ? existing.position : await nextPosition(env, input.userId)),
  };
  const stmts = [
    env.DB.prepare(
      `INSERT INTO mailboxes (email, name, screen, position, user_id, created_at) VALUES (?, ?, ?, ?, ?, ?)
       ON CONFLICT (email) DO UPDATE SET name = excluded.name, screen = excluded.screen, position = excluded.position,
         user_id = excluded.user_id`,
    ).bind(next.email, next.name, next.screen ? 1 : 0, next.position, next.user_id, Date.now()),
  ];
  const previous = existing?.user_id ?? OWNER;
  if (previous !== next.user_id) {
    const moved = "SELECT id FROM threads WHERE user_id = ? AND to_address = ?";
    stmts.push(
      env.DB.prepare(`UPDATE messages SET user_id = ? WHERE thread_id IN (${moved})`).bind(next.user_id, previous, email),
      env.DB.prepare("UPDATE threads SET user_id = ? WHERE user_id = ? AND to_address = ?").bind(next.user_id, previous, email),
    );
  }
  await env.DB.batch(stmts);
  return findMailbox(await mailboxes(env, next.user_id), email)!;
}

async function nextPosition(env: Env, userId: string): Promise<number> {
  const row = await env.DB.prepare("SELECT COALESCE(MAX(position), -1) + 1 AS p FROM mailboxes WHERE user_id = ?")
    .bind(userId)
    .first<{ p: number }>();
  return row?.p ?? 0;
}

export async function deleteMailbox(env: Env, email: string): Promise<boolean> {
  const res = await env.DB.prepare("DELETE FROM mailboxes WHERE email = ?").bind(normalizeEmail(email)).run();
  return (res.meta.changes ?? 0) > 0;
}

export async function getSettings(env: Env, userId: string): Promise<Settings> {
  const rows = await env.DB.prepare("SELECT key, value FROM settings WHERE user_id = ?").bind(userId).all<{ key: string; value: string }>();
  const out: Settings = { ...DEFAULT_SETTINGS };
  for (const r of rows.results) if (r.key in out) (out as unknown as Record<string, string>)[r.key] = r.value;
  return out;
}

export async function updateSettings(env: Env, userId: string, patch: Partial<Settings>): Promise<Settings> {
  const stmts = Object.entries(patch)
    .filter(([k, v]) => k in DEFAULT_SETTINGS && typeof v === "string")
    .map(([k, v]) =>
      env.DB.prepare(
        "INSERT INTO settings (user_id, key, value) VALUES (?, ?, ?) ON CONFLICT (user_id, key) DO UPDATE SET value = excluded.value",
      ).bind(userId, k, v),
    );
  if (stmts.length) await env.DB.batch(stmts);
  return getSettings(env, userId);
}

/** Whether any user forwards a copy of their mail to this address. */
export async function isForwardTarget(env: Env, email: string): Promise<boolean> {
  const row = await env.DB.prepare("SELECT 1 AS hit FROM settings WHERE key = 'forward_to' AND value = ? LIMIT 1")
    .bind(normalizeEmail(email))
    .first<{ hit: number }>();
  return !!row;
}
