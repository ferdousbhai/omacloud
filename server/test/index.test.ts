import assert from "node:assert/strict";
import { test } from "node:test";

import worker from "../src/index.ts";

const env = { PUBLIC_URL: "https://omacloud.computer" } as unknown as Env;
const get = (path: string, method = "GET") =>
	worker.fetch(new Request(`https://omacloud.computer${path}`, { method }) as never, env);

test("the pages are served, and nothing else", async () => {
	for (const path of ["/", "/privacy", "/terms", "/llms.txt", "/health"]) assert.equal((await get(path)).status, 200, path);
	for (const path of ["/signin", "/waitlist", "/api/keys", "/auth/google/callback"]) assert.equal((await get(path)).status, 404, path);
	assert.equal((await get("/", "POST")).status, 404);
});

test("no page mentions what was dropped", async () => {
	for (const path of ["/", "/privacy", "/terms", "/llms.txt"]) {
		const text = await (await get(path)).text();
		assert.doesNotMatch(text, /google|waitlist|invit|dropbox|hetzner|\bR2\b|bucket|quota|turnstile/i, path);
	}
});
