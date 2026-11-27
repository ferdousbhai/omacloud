import { defineWranglerConfig } from "wrangler/experimental-config";

export default defineWranglerConfig({
	types: {
		generate: false,
	},
	// the browser page, built by build-files.sh; the Worker hands it out
	// (see cloudflare.config.ts).
	assetsDirectory: "./public",
});
