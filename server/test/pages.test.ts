import assert from "node:assert/strict";
import { test } from "node:test";

import { llms } from "../src/pages.ts";

test("llms.txt follows llmstxt.org: a title, a summary, and sections of absolute links", async () => {
	const res = llms({ PUBLIC_URL: "https://omacloud.computer/" } as unknown as Env);
	assert.match(res.headers.get("content-type") ?? "", /^text\/plain/);
	const text = await res.text();
	const lines = text.split("\n");
	assert.equal(lines[0], "# Omacloud");
	assert.match(text, /^> /m);
	assert.match(text, /^## Docs$/m);
	assert.match(text, /^- \[Privacy\]\(https:\/\/omacloud\.computer\/privacy\): /m);
	for (const [, href] of text.matchAll(/^- \[[^\]]+\]\(([^)]+)\)/gm)) assert.match(href, /^https:\/\//);
	assert.doesNotMatch(text, /computer\/\/privacy/);
});
