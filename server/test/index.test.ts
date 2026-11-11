import assert from "node:assert/strict";
import { test } from "node:test";

import worker from "../src/index.ts";

// The assets as Cloudflare serves them: the Drive page and one of its scripts.
const assets = {
	fetch: async (req: Request) => {
		const path = new URL(req.url).pathname;
		if (path === "/drive") return Response.redirect("https://omacloud.computer/drive/", 307);
		if (path === "/drive/") return new Response("<!doctype html>", { headers: { "content-type": "text/html; charset=utf-8" } });
		if (path === "/drive/app.js") return new Response("//", { headers: { "content-type": "text/javascript" } });
		return new Response("Not found", { status: 404 });
	},
};
const env = { PUBLIC_URL: "https://omacloud.computer", ASSETS: assets } as unknown as Env;
const get = (path: string, method = "GET") =>
	worker.fetch(new Request(`https://omacloud.computer${path}`, { method }) as never, env);

test("the pages are served, and nothing else", async () => {
	for (const path of ["/", "/privacy", "/terms", "/llms.txt", "/health"]) assert.equal((await get(path)).status, 200, path);
	for (const gone of ["/signin", "/s", "/api/net/nodes"]) assert.equal((await get(gone)).status, 404, gone);
	assert.equal((await get("/", "POST")).status, 404);
});

test("the HTML pages, and only they, carry the security headers", async () => {
	for (const path of ["/", "/privacy", "/terms"]) {
		const r = await get(path);
		assert.equal(r.headers.get("x-frame-options"), "DENY", path);
		assert.equal(r.headers.get("referrer-policy"), "no-referrer", path);
		assert.match(r.headers.get("content-security-policy") ?? "", /frame-ancestors 'none'/, path);
	}
	for (const path of ["/llms.txt", "/health"]) assert.equal((await get(path)).headers.get("x-frame-options"), null, path);
});

test("/install.sh sends to the latest release's installer", async () => {
	const r = await get("/install.sh");
	assert.equal(r.status, 302);
	assert.equal(r.headers.get("location"), "https://github.com/ferdousbhai/omacloud/releases/latest/download/install.sh");
});

test("no page mentions what was dropped", async () => {
	for (const path of ["/", "/privacy", "/terms", "/llms.txt"]) {
		const text = await (await get(path)).text();
		assert.doesNotMatch(text, /google|waitlist|quota|turnstile|credit|holder|storage network|omacloud share|gateway/i, path);
	}
});

test("Omacloud Drive is served under a policy that loads only this site's code", async () => {
	const page = await get("/drive/");
	assert.equal(page.status, 200);
	const csp = page.headers.get("content-security-policy") ?? "";
	for (const part of ["default-src 'none'", "script-src 'self' 'wasm-unsafe-eval'", "connect-src 'self' https:", "frame-ancestors 'none'"]) {
		assert.ok(csp.includes(part), part);
	}
	assert.doesNotMatch(csp, /'unsafe-inline'|'unsafe-eval'|\*/);
	assert.equal(page.headers.get("x-frame-options"), "DENY");
	assert.equal(page.headers.get("cache-control"), "no-cache");
	const script = await get("/drive/app.js");
	assert.equal(script.status, 200);
	assert.equal(script.headers.get("x-content-type-options"), "nosniff");
	assert.equal((await get("/drive")).status, 307);
	assert.equal((await get("/drive/nothing")).status, 404);
	// only the Drive's own folder comes from the assets
	assert.equal((await get("/drivel")).status, 404);
});
