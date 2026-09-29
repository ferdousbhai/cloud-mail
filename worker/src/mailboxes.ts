import { type Address, normalizeEmail } from "./util";

export interface Mailbox extends Address {
  screen: boolean;
  position: number;
}

export interface Settings {
  forward_to: string;
}

const DEFAULT_SETTINGS: Settings = { forward_to: "" };

export async function mailboxes(env: Env): Promise<Mailbox[]> {
  const rows = await env.DB.prepare("SELECT email, name, screen, position FROM mailboxes ORDER BY position, created_at")
    .all<{ email: string; name: string; screen: number; position: number }>();
  return rows.results.map((r) => ({ email: r.email, name: r.name, screen: !!r.screen, position: r.position }));
}

export function findMailbox(list: Mailbox[], email: string): Mailbox | undefined {
  const e = normalizeEmail(email);
  return list.find((m) => m.email === e);
}

export async function upsertMailbox(
  env: Env,
  input: { email: string; name?: string; screen?: boolean; position?: number },
): Promise<Mailbox> {
  const email = normalizeEmail(input.email);
  const existing = findMailbox(await mailboxes(env), email);
  const next: Mailbox = {
    email,
    name: input.name ?? existing?.name ?? "",
    screen: input.screen ?? existing?.screen ?? true,
    position: input.position ?? existing?.position ?? (await nextPosition(env)),
  };
  await env.DB.prepare(
    `INSERT INTO mailboxes (email, name, screen, position, created_at) VALUES (?, ?, ?, ?, ?)
     ON CONFLICT (email) DO UPDATE SET name = excluded.name, screen = excluded.screen, position = excluded.position`,
  )
    .bind(next.email, next.name, next.screen ? 1 : 0, next.position, Date.now())
    .run();
  return next;
}

async function nextPosition(env: Env): Promise<number> {
  const row = await env.DB.prepare("SELECT COALESCE(MAX(position), -1) + 1 AS p FROM mailboxes").first<{ p: number }>();
  return row?.p ?? 0;
}

export async function deleteMailbox(env: Env, email: string): Promise<boolean> {
  const res = await env.DB.prepare("DELETE FROM mailboxes WHERE email = ?").bind(normalizeEmail(email)).run();
  return (res.meta.changes ?? 0) > 0;
}

export async function getSettings(env: Env): Promise<Settings> {
  const rows = await env.DB.prepare("SELECT key, value FROM settings").all<{ key: string; value: string }>();
  const out: Settings = { ...DEFAULT_SETTINGS };
  for (const r of rows.results) if (r.key in out) (out as unknown as Record<string, string>)[r.key] = r.value;
  return out;
}

export async function updateSettings(env: Env, patch: Partial<Settings>): Promise<Settings> {
  const stmts = Object.entries(patch)
    .filter(([k, v]) => k in DEFAULT_SETTINGS && typeof v === "string")
    .map(([k, v]) =>
      env.DB.prepare("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value").bind(k, v),
    );
  if (stmts.length) await env.DB.batch(stmts);
  return getSettings(env);
}
