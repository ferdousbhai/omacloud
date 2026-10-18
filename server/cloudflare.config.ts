import { bindings, defineConfig } from "cf/config";

export default defineConfig({
	worker: {
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
			// for the storage network (schema: migrations/, applied with
			// `cf d1 migrations apply`), in the EU jurisdiction
			DB: bindings.d1({
				name: "omacloud",
				id: "414e15fe-bc1b-4936-9754-ada7e8634ea1",
			}),
		},
	},
});
