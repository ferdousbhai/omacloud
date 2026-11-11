import { bindings, defineConfig } from "cf/config";

export default defineConfig({
	worker: {
		workersDev: false,
		previewUrls: false,
		name: "omacloud",
		compatibilityDate: "2026-10-05",
		entrypoint: "src/index.ts",
		// no request logs or traces kept: the site collects nothing
		observability: {
			enabled: false,
		},
		domains: ["omacloud.computer"],
		// Omacloud Drive's page (public/drive, put there by build-drive.sh). The
		// Worker answers first, so the page gets its headers like any other.
		assets: { runWorkerFirst: true },
		env: {
			PUBLIC_URL: bindings.text("https://omacloud.computer"),
			ASSETS: bindings.assets(),
		},
	},
});
