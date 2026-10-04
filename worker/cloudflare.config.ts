import { readFileSync } from "node:fs";
import { bindings, defineConfig } from "cf/config";

// One Cloudmail install: its worker, D1 database, R2 bucket and Cloudflare account. `cloudmail setup`
// creates them and writes their names and IDs to install.json (not committed). In development mode
// (`bun run dev`, which is `cf dev --mode development`) install.json is ignored and everything is
// simulated locally under these defaults, the names wrangler.local.jsonc uses too.
type Install = { name: string; accountId?: string; database: { name: string; id?: string }; bucket: string };
const defaults: Install = { name: "cloudmail", database: { name: "cloudmail" }, bucket: "cloudmail" };

function install(mode: string | undefined): Install {
	if (mode === "development") return defaults;
	try {
		return JSON.parse(readFileSync(new URL("./install.json", import.meta.url), "utf8"));
	} catch {
		return defaults;
	}
}

export default defineConfig(({ mode }) => {
	const { name, accountId, database, bucket } = install(mode);
	return {
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
	};
});
