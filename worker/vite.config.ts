import { cloudflare } from "@cloudflare/vite-plugin";
import { defineConfig } from "vite";

// `cf` builds the worker through Cloudflare's Vite plugin; cloudflare.config.ts holds its settings.
export default defineConfig({ plugins: [cloudflare()] });
