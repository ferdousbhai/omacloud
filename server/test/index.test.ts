import assert from "node:assert/strict";
import { test } from "node:test";

import worker from "../src/index.ts";

const env = { PUBLIC_URL: "https://omacloud.computer" } as unknown as Env;
const get = (path: string, method = "GET") =>
	worker.fetch(new Request(`https://omacloud.computer${path}`, { method }) as never, env);

test("the pages are served, and nothing else", async () => {
	for (const path of ["/", "/privacy", "/terms", "/llms.txt", "/s", "/health"]) assert.equal((await get(path)).status, 200, path);
	assert.equal((await get("/signin")).status, 404);
	assert.equal((await get("/", "POST")).status, 404);
});

test("/install.sh sends to the latest release's installer", async () => {
	const r = await get("/install.sh");
	assert.equal(r.status, 302);
	assert.equal(r.headers.get("location"), "https://github.com/ferdousbhai/omacloud/releases/latest/download/install.sh");
});

test("no page mentions what was dropped", async () => {
	for (const path of ["/", "/privacy", "/terms", "/llms.txt", "/s"]) {
		const text = await (await get(path)).text();
		assert.doesNotMatch(text, /google|waitlist|invit|dropbox|hetzner|\bR2\b|bucket|quota|turnstile/i, path);
	}
});
