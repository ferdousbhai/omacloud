import { cloudflare } from "@cloudflare/vite-plugin";
import { defineConfig } from "vite";

// `cf` builds the Worker through Cloudflare's Vite plugin. Vite copies
// public/ (built by build-files.sh) whole into the Worker's assets: the
// browser page (files/), which the Worker hands out, and the connector for
// agents (connector/), which it uploads for subscribers and serves to
// nobody, as index.ts passes only /files on. See cloudflare.config.ts.
export default defineConfig({ plugins: [cloudflare()] });
