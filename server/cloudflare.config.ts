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
		// the browser page (public/files, put there by build-files.sh). The
		// Worker answers first, so the page gets its headers like any other.
		assets: { runWorkerFirst: true },
		env: {
			PUBLIC_URL: bindings.text("https://omacloud.computer"),
			ASSETS: bindings.assets(),
			// the paid tier (src/paid.ts): who subscribed, their connectors'
			// names and their passkeys' sealed notes; Stripe's and
			// Cloudflare's keys. Without them the free site is all there is.
			DB: bindings.d1({ name: "omacloud" }),
			STRIPE_SECRET_KEY: bindings.secret(),
			STRIPE_WEBHOOK_SECRET: bindings.secret(),
			STRIPE_PRICE: bindings.secret(),
			PASSKEY_SECRET: bindings.secret(),
			CLOUDFLARE_API_TOKEN: bindings.secret(),
			CLOUDFLARE_ACCOUNT: bindings.secret(),
		},
	},
});
