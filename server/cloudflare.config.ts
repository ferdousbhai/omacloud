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
		env: {
			PUBLIC_URL: bindings.text("https://omacloud.computer"),
		},
	},
});
