import { defineWranglerConfig } from "wrangler/experimental-config";

export default defineWranglerConfig({
	types: {
		generate: false,
	},
	// Omacloud Drive's page, built by build-drive.sh; the Worker hands it out
	// (see cloudflare.config.ts).
	assetsDirectory: "./public",
});
