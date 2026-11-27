// Omacloud's connector for agents that aren't on one of your computers
// (claude.ai, an agent on a phone): a Worker in your own Cloudflare account,
// put there by `omacloud agents connect` and by nothing else. It reads your
// files from your own storage and decrypts them here, for the folders you
// granted, and leaves what an agent writes in the account's inbox for your
// computers to take in. Its secret (`OMACLOUD`, set at deploy) holds the
// storage's key and the repository's, so this account can read what it
// serves: docs/cloud-agents.md says what that trades. Nothing of Omacloud's
// own is in the path.

import wasm from "./omacloud_web.wasm";
import { Files } from "./files.js";
import { answer, unknownVersion } from "./mcp.js";
import { allowed, signin, unauthorized } from "./oauth.js";

// One per running copy of the Worker: the indexes read stay read.
let files;
let config;

const json = (body, status = 200) => new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json", "cache-control": "no-store" } });

export default {
	async fetch(request, env) {
		try {
			config ??= JSON.parse(env.OMACLOUD);
		} catch {
			return new Response("This connector has no settings: run `omacloud agents connect` again.", { status: 500 });
		}
		const url = new URL(request.url);
		const signing = await signin(request, config);
		if (signing) return signing;
		if (url.pathname !== "/mcp") {
			return new Response("Omacloud's connector for agents. Add this address, with /mcp after it, as a connector in your agent.", {
				status: url.pathname === "/" ? 200 : 404,
			});
		}
		// a page in a browser has no business here: agents call from servers
		const origin = request.headers.get("origin");
		if (origin && origin !== url.origin) return new Response(null, { status: 403 });
		if (request.method !== "POST") return new Response(null, { status: 405, headers: { allow: "POST" } });
		if (!(await allowed(request, config))) return unauthorized(request);
		if (unknownVersion(request.headers.get("mcp-protocol-version"))) return json({ jsonrpc: "2.0", id: null, error: { code: -32600, message: "a protocol version this connector doesn't speak" } }, 400);
		let message;
		try {
			message = await request.json();
		} catch {
			return json({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "not JSON" } }, 400);
		}
		files ??= new Files(config, wasm);
		const info = { version: config.version ?? "0", folders: config.folders ?? [], write: !!config.write && !!config.upload };
		const answered = await answer(message, files, info);
		// a notification, or an answer to something this asked (it asks nothing)
		return answered ? json(answered) : new Response(null, { status: 202 });
	},
};
