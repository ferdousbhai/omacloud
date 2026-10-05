import { bindings, defineConfig, triggers } from "cf/config";

// `--mode test` (scripts/gateway-sync.sh) runs without the domains: the
// local dev server would otherwise rewrite every request's Host to theirs,
// and Host tells sign-in from storage.
export default defineConfig(({ mode }) => ({
	worker: {
		name: "omacloud",
		compatibilityDate: "2026-10-05",
		entrypoint: "src/index.ts",
		observability: {
			enabled: true,
			traces: {
				enabled: true,
			},
		},
		domains: mode === "test" ? [] : ["omacloud.computer", "storage.omacloud.computer"],
		triggers: [
			// usage counts, for accounts written to
			triggers.scheduled({
				schedule: "*/15 * * * *",
			}),
		],
		env: {
			PUBLIC_URL: bindings.text("https://omacloud.computer"),
			STORAGE_HOST: bindings.text("storage.omacloud.computer"),
			// the region computers sign for
			REGION: bindings.text("omacloud"),
			// "invite" (emails in the invites table) or "open"
			SIGNUPS: bindings.text("invite"),
			// bytes for a new account: 200 GB
			DEFAULT_QUOTA: bindings.text("200000000000"),
			UPSTREAM_ENDPOINT: bindings.text("https://fsn1.your-objectstorage.com"),
			UPSTREAM_REGION: bindings.text("fsn1"),
			UPSTREAM_BUCKET: bindings.text("omacloud-storage"),
			GOOGLE_AUTH_URL: bindings.text("https://accounts.google.com/o/oauth2/v2/auth"),
			GOOGLE_TOKEN_URL: bindings.text("https://oauth2.googleapis.com/token"),
			// the bucket's key and the Google OAuth client (both halves of
			// each), and what account keys derive from (32 bytes or more, hex;
			// set once, as changing it voids every account's key): deploy.sh
			UPSTREAM_KEY_ID: bindings.secret(),
			UPSTREAM_SECRET: bindings.secret(),
			GOOGLE_CLIENT_ID: bindings.secret(),
			GOOGLE_SECRET: bindings.secret(),
			MASTER_KEY: bindings.secret(),
			// accounts, keys and sign-ins (schema: migrations/, applied with
			// `cf d1 migrations apply`), in the EU jurisdiction
			DB: bindings.d1({
				name: "omacloud",
				id: "414e15fe-bc1b-4936-9754-ada7e8634ea1",
			}),
		},
	},
}));
