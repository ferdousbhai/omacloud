import { cloudflare } from "@cloudflare/vite-plugin";
import { defineConfig } from "vite";

// `cf` builds the Worker through Cloudflare's Vite plugin. Vite copies
// public/ (the browser page, built by build-files.sh) whole into the
// Worker's assets; see cloudflare.config.ts.
export default defineConfig({ plugins: [cloudflare()] });
