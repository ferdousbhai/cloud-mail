import { Database } from "bun:sqlite";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

// workerd has crypto.subtle.timingSafeEqual; Bun doesn't.
const subtle = crypto.subtle as SubtleCrypto & { timingSafeEqual?: (a: Uint8Array, b: Uint8Array) => boolean };
subtle.timingSafeEqual ??= (a, b) => a.length === b.length && a.every((x, i) => x === b[i]);

const MIGRATIONS = join(import.meta.dir, "..", "migrations");

/** The worker's migrations in order; only those from `from` and before `until` (file names), when given. */
export function migrate(db: Database, { from, until }: { from?: string; until?: string } = {}) {
  for (const f of readdirSync(MIGRATIONS).filter((f) => f.endsWith(".sql")).sort()) {
    if ((from && f < from) || (until && f >= until)) continue;
    db.exec(readFileSync(join(MIGRATIONS, f), "utf8"));
  }
}

/** D1 over an in-memory SQLite with the worker's migrations applied. */
export function d1(db: Database) {
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

export function r2() {
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
