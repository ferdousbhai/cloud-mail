import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { bindings, defineConfig } from "cf/config";

// One Cloudmail install: its worker, D1 database, R2 bucket and Cloudflare account. `cloudmail setup`
// creates them and writes their names and IDs to install.json (not committed). Without install.json
// (a checkout, CI) these defaults stand in, which is enough for `cf workers types`. D1 migrations
// apply to the remote database.
type Install = { name: string; accountId?: string; database: { name: string; id?: string }; bucket: string };
const defaults: Install = { name: "cloudmail", database: { name: "cloudmail" }, bucket: "cloudmail" };

function install(): Install {
	try {
		return JSON.parse(readFileSync(join(dirname(fileURLToPath(import.meta.url)), "install.json"), "utf8"));
	} catch {
		return defaults;
	}
}

const { name, accountId, database, bucket } = install();

export default defineConfig({
	accountId,
	worker: {
		name,
		compatibilityDate: "2026-09-01",
		entrypoint: "src/index.ts",
		observability: { enabled: true },
		env: {
			// Migrations live in migrations/; setup applies them with `cf d1 migrations apply <id>`.
			DB: bindings.d1(database),
			BUCKET: bindings.r2({ name: bucket }),
			EMAIL: bindings.sendEmail({}),
			// Secret: API_TOKEN, set by `cloudmail setup` (cf workers secrets update) after the first
			// deploy. Not declared here: cf deploy keeps a Worker's existing secrets.
		},
	},
});
