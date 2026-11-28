import assert from "node:assert/strict";
import { test } from "node:test";
// The connector for agents (connector/): its sign-in and the protocol it speaks, with the files stood in for.
// @ts-ignore: plain JavaScript
import { answer, unknownVersion } from "../../connector/mcp.js";
// @ts-ignore: plain JavaScript
import { seal, signed, signin, unauthorized, unseal } from "../../connector/oauth.js";
// @ts-ignore: plain JavaScript
import { Refused } from "../../connector/files.js";

const hex = (bytes: ArrayBuffer) => [...new Uint8Array(bytes)].map((b) => b.toString(16).padStart(2, "0")).join("");
const b64 = (bytes: ArrayBuffer) => Buffer.from(bytes).toString("base64url");
const sha = (words: string) => crypto.subtle.digest("SHA-256", new TextEncoder().encode(words));
const at = "https://omacloud-agents.me.workers.dev";

async function settings() {
	return { secret: "s".repeat(64), passphrase_hash: hex(await sha("abcdefghijklmnop")), folders: ["Documents/Notes"], write: true };
}
const post = (path: string, body: string, type = "application/x-www-form-urlencoded") =>
	new Request(at + path, { method: "POST", body, headers: { "content-type": type } });

test("what the connector seals only it reads back, and only as what it is", async () => {
	const sealed = await seal("one", { t: "access", exp: Math.floor(Date.now() / 1000) + 60 });
	assert.equal((await unseal("one", sealed, "access")).t, "access");
	assert.equal(await unseal("two", sealed, "access"), null);
	assert.equal(await unseal("one", sealed, "refresh"), null);
	assert.equal(await unseal("one", await seal("one", { t: "access", exp: 1 }), "access"), null);
	assert.equal(await unseal("one", `${sealed.split(".")[0]}x.${sealed.split(".")[1]}`, "access"), null);
	assert.equal(await unseal("one", "", "access"), null);
});

test("an agent signs in with the passphrase, and nothing else lets it in", async () => {
	const config = await settings();
	const found = await (await signin(new Request(`${at}/.well-known/oauth-protected-resource/mcp`), config)).json();
	assert.equal(found.resource, `${at}/mcp`);
	const server = await (await signin(new Request(`${at}/.well-known/oauth-authorization-server`), config)).json();
	assert.deepEqual(server.code_challenge_methods_supported, ["S256"]);

	// an address that isn't https, or this computer's, can't register
	const bad = await signin(post("/register", JSON.stringify({ redirect_uris: ["http://elsewhere.example/cb"] }), "application/json"), config);
	assert.equal(bad.status, 400);
	const back = "https://claude.ai/api/mcp/auth_callback";
	const registered = await (await signin(post("/register", JSON.stringify({ redirect_uris: [back], client_name: "Claude" }), "application/json"), config)).json();
	const client = registered.client_id;

	const verifier = "v".repeat(50);
	const asked = new URLSearchParams({ response_type: "code", client_id: client, redirect_uri: back, code_challenge: b64(await sha(verifier)), code_challenge_method: "S256", state: "xyz" });
	const page = await signin(new Request(`${at}/authorize?${asked}`), config);
	assert.equal(page.status, 200);
	const shown = await page.text();
	assert.match(shown, /Claude/);
	assert.match(shown, /Documents\/Notes/);
	assert.match(shown, /read and write/);
	// somewhere it didn't register, or a client this connector never marked: said here, sent nowhere
	const elsewhere = new URLSearchParams(asked);
	elsewhere.set("redirect_uri", "https://evil.example/cb");
	assert.equal((await signin(new Request(`${at}/authorize?${elsewhere}`), config)).status, 400);
	const forged = new URLSearchParams(asked);
	forged.set("client_id", `${client}x`);
	assert.equal((await signin(new Request(`${at}/authorize?${forged}`), config)).status, 400);

	const wrong = await signin(post("/authorize", `${asked}&passphrase=nope`), config);
	assert.equal(wrong.status, 403);
	assert.equal(wrong.headers.get("location"), null);
	// as it's shown, in groups
	const right = await signin(post("/authorize", `${asked}&passphrase=ABCD-efgh-ijkl-mnop`), config);
	assert.equal(right.status, 302);
	const went = new URL(right.headers.get("location"));
	assert.equal(went.origin + went.pathname, back);
	assert.equal(went.searchParams.get("state"), "xyz");
	const code = went.searchParams.get("code") ?? "";

	const grant = (more: Record<string, string>) => signin(post("/token", String(new URLSearchParams({ grant_type: "authorization_code", client_id: client, redirect_uri: back, code, ...more }))), config);
	assert.equal((await grant({ code_verifier: "another" })).status, 400);
	assert.equal((await grant({ code_verifier: verifier, redirect_uri: "https://evil.example/cb" })).status, 400);
	const tokens = await (await grant({ code_verifier: verifier })).json();
	assert.equal(tokens.token_type, "Bearer");

	const call = (token?: string) => new Request(`${at}/mcp`, { method: "POST", headers: token ? { authorization: `Bearer ${token}` } : {} });
	assert.equal(!!(await signed(call(tokens.access_token), config)), true);
	// the token carries what opens the connector's sealed keys, worked out from the passphrase
	const opens = b64(await sha("omacloud connector key 1abcdefghijklmnop"));
	assert.equal((await signed(call(tokens.access_token), config)).k, opens);
	assert.equal(!!(await signed(call(), config)), false);
	// a refresh token isn't an access token, and another connector's secret made neither
	assert.equal(!!(await signed(call(tokens.refresh_token), config)), false);
	assert.equal(!!(await signed(call(tokens.access_token), { ...config, secret: "t".repeat(64) })), false);
	assert.match(unauthorized(call()).headers.get("www-authenticate"), /resource_metadata="https:\/\/omacloud-agents\.me\.workers\.dev\/\.well-known\/oauth-protected-resource"/);

	const again = await signin(post("/token", String(new URLSearchParams({ grant_type: "refresh_token", refresh_token: tokens.refresh_token }))), config);
	assert.equal((await signed(call((await again.json()).access_token), config)).k, opens);
	assert.equal((await signin(post("/token", "grant_type=refresh_token&refresh_token=nope"), config)).status, 400);
	assert.equal(await signin(new Request(`${at}/mcp`), config), null);
});

test("Claude Code comes back to this computer, on whatever port it took", async () => {
	const config = await settings();
	const registered = await (await signin(post("/register", JSON.stringify({ redirect_uris: ["http://localhost/callback"] }), "application/json"), config)).json();
	const asked = new URLSearchParams({ response_type: "code", client_id: registered.client_id, redirect_uri: "http://localhost:51234/callback", code_challenge: "c", code_challenge_method: "S256" });
	assert.equal((await signin(new Request(`${at}/authorize?${asked}`), config)).status, 200);
});

test("the protocol: tools listed and called, refusals said to the agent", async () => {
	const files = {
		list: async (path: string) => (path ? [{ name: "plan.md", type: "file", size: 2048, modified: "2026-11-27T10:00:00Z" }] : [{ name: "Documents", type: "folder", size: 0, modified: "" }]),
		read: async (path: string, offset: number, most: number) => {
			if (path === "secret.md") throw new Refused("secret.md is outside the folders this connector was given.");
			if (path === "photo.png") return { bytes: new Uint8Array([137, 80, 0, 0]), size: 4 };
			// "né" five times over: every é is two bytes
			const whole = new TextEncoder().encode("né".repeat(5));
			return { bytes: whole.subarray(offset, Math.min(whole.length, offset + (path === "short.md" ? most : 5))), size: whole.length };
		},
		search: async () => ({ found: [{ path: "Documents/plan.md", line: 3, text: "the plan" }, { path: "Documents/planning" }], done: false }),
		versions: async () => [{ version: "1a2b3c4d", made: "2026-11-27T10:00:00Z", by: "thinkpad" }],
		write: async (path: string) => path,
	};
	const info = { version: "0.0.33", folders: ["Documents"], write: true };
	const ask = (method: string, params?: unknown, id: unknown = 1) => answer({ jsonrpc: "2.0", id, method, params }, files, info);
	const said = async (name: string, args = {}) => (await ask("tools/call", { name, arguments: args })).result;

	const hello = (await ask("initialize", { protocolVersion: "2025-06-18", capabilities: {} })).result;
	assert.equal(hello.protocolVersion, "2025-06-18");
	assert.match(hello.instructions, /Documents, and may write there/);
	assert.equal((await ask("initialize", { protocolVersion: "1999-01-01" })).result.protocolVersion, "2025-11-25");
	assert.equal(await answer({ jsonrpc: "2.0", method: "notifications/initialized" }, files, info), null);
	assert.deepEqual((await ask("ping")).result, {});
	assert.equal((await ask("no/such")).error.code, -32601);
	assert.equal((await answer("nonsense", files, info)).error.code, -32600);
	assert.equal(unknownVersion("1999-01-01"), true);
	assert.equal(unknownVersion(null), false);

	const tools = (await ask("tools/list")).result.tools.map((t: { name: string }) => t.name);
	assert.deepEqual(tools, ["list_files", "read_file", "search_files", "list_versions", "write_file"]);
	const readOnly = (await answer({ jsonrpc: "2.0", id: 1, method: "tools/list" }, files, { ...info, write: false })).result.tools;
	assert.equal(readOnly.some((t: { name: string }) => t.name === "write_file"), false);

	assert.equal((await said("list_files")).content[0].text, "Documents/");
	assert.equal((await said("list_files", { path: "Documents" })).content[0].text, "plan.md  (2.0 KB, 2026-11-27)");
	assert.equal((await said("read_file", { path: "short.md" })).content[0].text, "né".repeat(5));
	// a part ends at a whole character, and says where to read on
	const part = (await said("read_file", { path: "long.md" })).content[0].text;
	assert.match(part, /^nén\n\n\[4 of 15 bytes so far\. Read on with offset 4\.\]$/);
	assert.match((await said("read_file", { path: "photo.png" })).content[0].text, /isn't text/);
	const refused = await said("read_file", { path: "secret.md" });
	assert.equal(refused.isError, true);
	assert.match(refused.content[0].text, /outside the folders/);
	assert.match((await said("search_files", { query: "plan" })).content[0].text, /Documents\/plan\.md:3: the plan\nDocuments\/planning\n\[Stopped early/);
	assert.match((await said("list_versions")).content[0].text, /1a2b3c4d {2}2026-11-27T10:00:00Z {2}by thinkpad/);
	assert.match((await said("write_file", { path: "Documents/new.md", content: "hello" })).content[0].text, /^Sent Documents\/new\.md \(5 B\)/);
	assert.equal((await said("write_file", { path: "Documents/new.md" })).isError, true);
	assert.equal((await ask("tools/call", { name: "no_such_tool" })).error.code, -32602);
});
