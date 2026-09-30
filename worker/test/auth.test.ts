import { expect, test } from "bun:test";
import { authVerdict, isVerified } from "../src/ingest";

const cf = {
  key: "authentication-results",
  value: "mx.cloudflare.net;\n\tdkim=pass header.d=hey.com header.s=heymail;\n\tdmarc=pass header.from=hey.com policy.dmarc=quarantine;\n\tspf=pass (mx.cloudflare.net: …) smtp.helo=relay.hey.com;",
};

test("reads Cloudflare's verdicts", () => {
  expect(authVerdict([cf, { key: "x-cf-spamh-score", value: "0" }])).toMatchObject({ dmarc: "pass", spf: "pass", dkim: "pass", spam_score: 0, dkim_domains: ["hey.com"] });
});

test("ignores a sender-supplied header below Cloudflare's", () => {
  const forged = { key: "authentication-results", value: "mx.cloudflare.net; dmarc=pass; spf=pass; dkim=pass" };
  const real = { ...cf, value: cf.value.replace("dmarc=pass", "dmarc=fail") };
  expect(authVerdict([real, forged])?.dmarc).toBe("fail");
});

test("ignores other receivers' results", () => {
  expect(authVerdict([{ key: "authentication-results", value: "mx.google.com; dmarc=pass" }])).toBeNull();
});

test("a DKIM header.b chosen by the sender can't pose as the DMARC result", () => {
  const forged = {
    key: "authentication-results",
    value: "mx.cloudflare.net;\n\tdkim=fail header.d=bank.com header.s=x header.b=dmarc=pa;\n\tdmarc=fail header.from=bank.com;\n\tspf=none",
  };
  expect(authVerdict([forged])?.dmarc).toBe("fail");
  expect(authVerdict([forged])?.dkim).toBe("fail");
});

test("the real Cloudflare header parses per clause", () => {
  const real = {
    key: "authentication-results",
    value: "mx.cloudflare.net;\n\tdkim=pass header.d=hey.com header.s=heymail header.b=msNEcI5e;\n\tdmarc=pass header.from=hey.com policy.dmarc=quarantine;\n\tspf=pass (mx.cloudflare.net: domain of postmaster@01a.relay.hey.com designates 204.62.114.224 as permitted sender) smtp.helo=01a.relay.hey.com;",
  };
  expect(authVerdict([real])).toMatchObject({ dmarc: "pass", spf: "pass", dkim: "pass", spam_score: null, dkim_domains: ["hey.com"] });
});

const ar = (value: string) => ({ key: "authentication-results", value: `mx.cloudflare.net; ${value}` });
const rspf = (result: string) => ({ key: "received-spf", value: `${result} (mx.cloudflare.net: …) receiver=mx.cloudflare.net; envelope-from="x"` });

test("DMARC pass verifies; DMARC fail never does", () => {
  expect(isVerified(authVerdict([ar("dmarc=pass header.from=a.com")])!, "a@a.com", "b@elsewhere.net")).toBe(true);
  expect(isVerified(authVerdict([ar("dkim=pass header.d=a.com; dmarc=fail header.from=a.com")])!, "a@a.com", "a@a.com")).toBe(false);
});

test("without a DMARC policy, aligned DKIM or SPF verifies", () => {
  // DKIM from a subdomain of the From domain's organisation.
  expect(isVerified(authVerdict([ar("dkim=pass header.d=mail.shop.co.uk; dmarc=none")])!, "hi@shop.co.uk", "bounce@esp.net")).toBe(true);
  // SPF for an envelope sender on the same organisational domain.
  expect(isVerified(authVerdict([rspf("pass"), ar("dmarc=none")])!, "hi@shop.co.uk", "bounces@news.shop.co.uk")).toBe(true);
});

test("unaligned authentication doesn't verify", () => {
  // A valid signature from the attacker's own domain proves nothing about the From address.
  expect(isVerified(authVerdict([ar("dkim=pass header.d=attacker.com; dmarc=none")])!, "ceo@bank.com", "x@attacker.com")).toBe(false);
  expect(isVerified(authVerdict([rspf("pass"), ar("dmarc=none")])!, "ceo@bank.com", "x@attacker.com")).toBe(false);
  // Separate sites on a shared host are separate organisations.
  expect(isVerified(authVerdict([ar("dkim=pass header.d=evil.github.io; dmarc=none")])!, "me@mine.github.io", "x@y.z")).toBe(false);
});

test("a Received-SPF header written by the sender doesn't count", () => {
  // Below Cloudflare's result line, even when it claims to be Cloudflare's.
  const forged = { key: "received-spf", value: "pass (mx.cloudflare.net: …) receiver=mx.cloudflare.net" };
  expect(authVerdict([ar("dmarc=none"), forged])!.spf_envelope).toBeNull();
  expect(isVerified(authVerdict([ar("dmarc=none"), forged])!, "ceo@bank.com", "ceo@bank.com")).toBe(false);
});
