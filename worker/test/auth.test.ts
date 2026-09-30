import { expect, test } from "bun:test";
import { authVerdict } from "../src/ingest";

const cf = {
  key: "authentication-results",
  value: "mx.cloudflare.net;\n\tdkim=pass header.d=hey.com header.s=heymail;\n\tdmarc=pass header.from=hey.com policy.dmarc=quarantine;\n\tspf=pass (mx.cloudflare.net: …) smtp.helo=relay.hey.com;",
};

test("reads Cloudflare's verdicts", () => {
  expect(authVerdict([cf, { key: "x-cf-spamh-score", value: "0" }])).toEqual({ dmarc: "pass", spf: "pass", dkim: "pass", spam_score: 0 });
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
  expect(authVerdict([real])).toEqual({ dmarc: "pass", spf: "pass", dkim: "pass", spam_score: null });
});
