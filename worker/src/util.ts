import { addressParser, type Address as PMAddress } from "postal-mime";

export interface Address {
  name: string;
  email: string;
}

export function newId(prefix: string): string {
  return `${prefix}_${crypto.randomUUID().replace(/-/g, "").slice(0, 20)}`;
}

export function normalizeEmail(email: string): string {
  return email.trim().toLowerCase();
}

export function flattenAddresses(list: PMAddress[] | PMAddress | undefined): Address[] {
  if (!list) return [];
  const items = Array.isArray(list) ? list : [list];
  const out: Address[] = [];
  for (const item of items) {
    if (item.group) {
      for (const m of item.group) out.push({ name: m.name || "", email: normalizeEmail(m.address) });
    } else if (item.address) {
      out.push({ name: item.name || "", email: normalizeEmail(item.address) });
    }
  }
  return out;
}

/** Parses "Name <a@b.com>", "a@b.com" or comma-separated lists of either. */
export function parseAddressList(input: string | string[] | undefined): Address[] {
  if (!input) return [];
  const parts = Array.isArray(input) ? input : [input];
  return parts.flatMap((p) => flattenAddresses(addressParser(p))).filter((a) => a.email.includes("@"));
}

/** Splits a References / In-Reply-To header into individual <message-id> tokens. */
export function messageIdList(value: string | null | undefined): string[] {
  if (!value) return [];
  const ids = value.match(/<[^<>\s]+>/g);
  if (ids) return ids;
  return value.split(/\s+/).filter(Boolean).map((v) => (v.startsWith("<") ? v : `<${v}>`));
}

export function normalizeMessageId(value: string | null | undefined): string | null {
  if (!value) return null;
  return messageIdList(value)[0] ?? null;
}

export function htmlToText(html: string): string {
  return html
    .replace(/<(style|script|head)[^>]*>[\s\S]*?<\/\1>/gi, " ")
    .replace(/<br\s*\/?>/gi, "\n")
    .replace(/<\/(p|div|tr|li|h[1-6]|table)>/gi, "\n")
    .replace(/<\/(td|th)>/gi, " ")
    .replace(/<[^>]+>/g, "")
    .replace(/&nbsp;/gi, " ")
    .replace(/&amp;/gi, "&")
    .replace(/&lt;/gi, "<")
    .replace(/&gt;/gi, ">")
    .replace(/&quot;/gi, '"')
    .replace(/&#39;|&apos;/gi, "'")
    .replace(/[ \t]+/g, " ")
    .replace(/\n\s*\n\s*/g, "\n\n")
    .trim();
}

export function makeSnippet(text: string): string {
  const lines = text.split("\n");
  const kept: string[] = [];
  for (const line of lines) {
    const t = line.trim();
    if (t.startsWith(">")) continue;
    if (/^On .+wrote:$/.test(t)) break;
    if (t === "--") break;
    kept.push(t);
  }
  return kept.join(" ").replace(/\s+/g, " ").trim().slice(0, 200);
}

export function escapeHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

export function textToHtml(text: string): string {
  const linked = escapeHtml(text).replace(/https?:\/\/[^\s<]+/g, (url) => `<a href="${url}">${url}</a>`);
  return `<div style="font-family: sans-serif; white-space: pre-wrap;">${linked}</div>`;
}

export function arrayBufferToBase64(buf: ArrayBuffer): string {
  const bytes = new Uint8Array(buf);
  let binary = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode(...bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
}

export function toArrayBuffer(content: ArrayBuffer | Uint8Array | string): ArrayBuffer | Uint8Array {
  return typeof content === "string" ? new TextEncoder().encode(content) : content;
}

export function byteLength(s: string): number {
  return new TextEncoder().encode(s).length;
}

export function json(data: unknown, status = 200): Response {
  return new Response(JSON.stringify(data), {
    status,
    headers: { "content-type": "application/json; charset=utf-8" },
  });
}

export function error(message: string, status = 400): Response {
  return json({ error: message }, status);
}

export function timingSafeEqual(a: string, b: string): boolean {
  const enc = new TextEncoder();
  const ab = enc.encode(a);
  const bb = enc.encode(b);
  if (ab.length !== bb.length) return false;
  return crypto.subtle.timingSafeEqual(ab, bb);
}
