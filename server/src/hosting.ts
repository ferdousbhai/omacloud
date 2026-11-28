// Running a subscriber's connector for agents (connector/) in Omacloud's own
// Cloudflare account: the same Worker `omacloud agents connect` puts in a
// person's own account (crates/omacloud/src/cloudflare.rs), one per
// connector, so no two subscribers' keys are ever open in one place. Its
// modules are this site's own files (build-files.sh puts them in
// public/connector, which is served to nobody).

export interface Cloud {
	token: string;
	account: string;
	// the connector's modules, by name
	module(name: string): Promise<ArrayBuffer>;
}

// the one that runs comes first
const MODULES = ["worker.js", "files.js", "mcp.js", "oauth.js", "reader.js", "s3.js", "dropbox.js", "omacloud_web.wasm"];
const API = "https://api.cloudflare.com/client/v4";

async function call(cloud: Cloud, net: typeof fetch, what: string, path: string, init: RequestInit): Promise<any> {
	const response = await net(`${API}/accounts/${cloud.account}/workers/${path}`, {
		...init,
		headers: { authorization: `Bearer ${cloud.token}`, ...(init.headers as Record<string, string>) },
	});
	const said = (await response.json().catch(() => null)) as { success?: boolean; result?: unknown; errors?: { message: string }[] } | null;
	if (!response.ok || said?.success === false) {
		const words = said?.errors?.map((e) => e.message).join("; ") || `it answered ${response.status}`;
		throw Object.assign(new Error(`Cloudflare refused ${what}: ${words}`), { status: response.status });
	}
	return said?.result;
}

// Put the connector up as the Worker `script` with `settings` as its secret;
// its address.
export async function deployConnector(cloud: Cloud, net: typeof fetch, script: string, settings: string): Promise<string> {
	const form = new FormData();
	form.set(
		"metadata",
		new Blob(
			[
				JSON.stringify({
					main_module: MODULES[0],
					compatibility_date: "2026-10-05",
					bindings: [{ type: "secret_text", name: "OMACLOUD", text: settings }],
				}),
			],
			{ type: "application/json" },
		),
	);
	for (const name of MODULES) {
		const type = name.endsWith(".wasm") ? "application/wasm" : "application/javascript+module";
		form.set(name, new File([await cloud.module(name)], name, { type }));
	}
	await call(cloud, net, "the connector", `scripts/${script}`, { method: "PUT", body: form });
	await call(cloud, net, "giving the connector its address", `scripts/${script}/subdomain`, {
		method: "POST",
		headers: { "content-type": "application/json" },
		body: JSON.stringify({ enabled: true }),
	});
	const { subdomain } = await call(cloud, net, "saying the workers.dev name", "subdomain", { method: "GET" });
	return `https://${script}.${subdomain}.workers.dev`;
}

// Take the Worker `script` down; one that's already gone is no error.
export async function removeConnector(cloud: Cloud, net: typeof fetch, script: string): Promise<void> {
	try {
		await call(cloud, net, "removing the connector", `scripts/${script}`, { method: "DELETE" });
	} catch (e) {
		if ((e as { status?: number }).status !== 404) throw e;
	}
}
