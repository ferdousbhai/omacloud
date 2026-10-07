import assert from "node:assert/strict";
import { afterEach, test } from "node:test";

import { masterKey, secretOf } from "../src/db.ts";
import { handle } from "../src/gateway.ts";
import { amzDate, canonicalRequest, sign } from "../src/sigv4.ts";
import * as trash from "../src/trash.ts";
import { d1 } from "./d1.ts";
import { bucket } from "./s3.ts";

const realFetch = globalThis.fetch;
const realError = console.error;
afterEach(() => {
	globalThis.fetch = realFetch;
	console.error = realError;
});

const KEY = "OCAAAAAAAAAAAAAAAAAA";
const A = "u00000000000000aa";
const HOST = "storage.omacloud.computer";
const LOCK = `locks/${"ab".repeat(32)}`;

/** An account (quota `quota`) with a key, the bucket behind standing in. */
function setup(quota = 1_000_000) {
	const DB = d1();
	const s3 = bucket();
	globalThis.fetch = s3.fetch;
	const outbox: { to: string; subject: string; text: string }[] = [];
	const env = {
		DB,
		STORAGE_HOST: HOST,
		REGION: "omacloud",
		MASTER_KEY: "ab".repeat(32),
		UPSTREAM_ENDPOINT: "https://bucket.test",
		UPSTREAM_REGION: "r",
		UPSTREAM_BUCKET: "everyone",
		UPSTREAM_KEY_ID: "k",
		UPSTREAM_SECRET: "s",
		ADMIN_EMAIL: "admin@example.com",
		EMAIL: { send: async (m: { to: string; subject: string; text: string }) => void outbox.push(m) },
	} as unknown as Env;
	sql(DB, "INSERT INTO accounts (id, google_sub, email, created, quota) VALUES (?, 's', 'a@example.com', 0, ?)", A, quota);
	sql(DB, "INSERT INTO keys (id, account, created) VALUES (?, ?, 0)", KEY, A);
	const waits: Promise<unknown>[] = [];
	const ctx = { waitUntil: (p: Promise<unknown>) => waits.push(p) } as unknown as ExecutionContext;
	/** A request signed with the account's key; `path` as the URL will have it. */
	const req = async (method: string, path: string, query = "", body?: string, headers: Record<string, string> = {}) => {
		const stamp = amzDate();
		const scopeText = `${stamp.slice(0, 8)}/omacloud/s3/aws4_request`;
		const url = new URL(`https://${HOST}${path}${query ? `?${query}` : ""}`);
		const signed: [string, string][] = [
			["host", HOST],
			["x-amz-content-sha256", "UNSIGNED-PAYLOAD"],
			["x-amz-date", stamp],
		];
		const canonical = canonicalRequest(method, url.pathname, url.search.slice(1), signed, "UNSIGNED-PAYLOAD");
		const signature = await sign(await secretOf(masterKey(env), KEY), stamp, scopeText, canonical);
		const r = new Request(url, {
			method,
			body,
			headers: {
				...Object.fromEntries(signed),
				...(body !== undefined ? { "content-length": String(new TextEncoder().encode(body).length) } : {}),
				...headers,
				authorization: `AWS4-HMAC-SHA256 Credential=${KEY}/${scopeText}, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=${signature}`,
			},
		});
		const out = await handle(r, env, ctx);
		await Promise.all(waits.splice(0));
		return out;
	};
	const trashed = () => [...s3.objects.keys()].filter((k) => k.startsWith(trash.TRASH)).sort();
	return { env, DB, s3, req, trashed, outbox };
}

const sql = (DB: D1Database, q: string, ...p: unknown[]) =>
	(DB as unknown as { sqlite: import("node:sqlite").DatabaseSync }).sqlite.prepare(q).all(...(p as never[]));

const deleting = (...keys: string[]) =>
	`<Delete>${keys.map((k) => `<Object><Key>${k}</Key></Object>`).join("")}</Delete>`;

test("a delete, a batch delete and an overwrite keep what they destroy, first", async () => {
	const { DB, s3, req, trashed } = setup();
	s3.put(`${A}/data/1`, "one");
	s3.put(`${A}/data/2`, "two");
	s3.put(`${A}/snapshots/s`, "snap");
	s3.put(`${A}/index/i`, "index");

	assert.equal((await req("DELETE", `/${A}/data/1`)).status, 204);
	assert.deepEqual(s3.calls, [`HEAD ${A}/data/1`, s3.calls[1], `DELETE ${A}/data/1`]);
	assert.match(s3.calls[1], new RegExp(`^COPY \\.trash/${A}/\\d{13}\\.[0-9a-f]{6}/data/1 from /everyone/${A}/data/1$`));

	s3.calls.length = 0;
	const many = await req("POST", `/${A}`, "delete", deleting("data/2", "snapshots/s", "nothing/here"));
	assert.equal(many.status, 200);
	assert.equal(s3.calls.at(-1), "POST ");
	assert.equal(s3.calls.filter((c) => c.startsWith("COPY")).length, 2);
	assert.ok(!s3.objects.has(`${A}/data/2`) && !s3.objects.has(`${A}/snapshots/s`));

	s3.calls.length = 0;
	assert.equal((await req("PUT", `/${A}/index/i`, "", "junk")).status, 200);
	assert.deepEqual(
		s3.calls.map((c) => c.split(" ")[0]),
		["HEAD", "COPY", "PUT"],
	);
	assert.equal(s3.text(`${A}/index/i`), "junk");

	// a finished multipart upload replaces too
	s3.calls.length = 0;
	assert.equal((await req("POST", `/${A}/index/i`, "uploadId=U", "<CompleteMultipartUpload/>")).status, 200);
	assert.deepEqual(
		s3.calls.map((c) => c.split(" ")[0]),
		["GET", "HEAD", "COPY", "POST"],
	);

	const kept = trashed().map((k) => [trash.parseTrashKey(k)!.key, s3.text(k)]);
	assert.deepEqual(kept.sort(), [
		["data/1", "one"],
		["data/2", "two"],
		["index/i", "index"],
		["index/i", "junk"],
		["snapshots/s", "snap"],
	]);
	const [t] = sql(DB, "SELECT * FROM trash");
	assert.equal(t.account, A);
	assert.equal(t.bytes, 3 + 3 + 4 + 5 + 4);
	assert.ok(t.oldest);
	// not counted in the quota
	assert.ok(trashed().every((k) => !k.startsWith(`${A}/`)));
});

test("a new object costs a look, no copy, and stays new while it's written", async () => {
	const { s3, req, trashed } = setup();
	assert.equal((await req("PUT", `/${A}/data/new`, "", "fresh")).status, 200);
	assert.deepEqual(s3.calls, [`HEAD ${A}/data/new`, `PUT ${A}/data/new`]);
	assert.deepEqual(trashed(), []);
	// created by someone else between the look and the write: try again
	const real = s3.fetch;
	globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
		if (init?.method === "PUT") s3.put(`${A}/data/raced`, "theirs");
		return real(input, init);
	}) as typeof fetch;
	assert.equal((await req("PUT", `/${A}/data/raced`, "", "mine")).status, 503);
	assert.equal(s3.text(`${A}/data/raced`), "theirs");
	globalThis.fetch = real;
	// a create-only write needs no look at all
	s3.calls.length = 0;
	assert.equal((await req("PUT", `/${A}/heads/1`, "", "h", { "if-none-match": "*" })).status, 200);
	assert.deepEqual(s3.calls, [`PUT ${A}/heads/1`]);
	assert.equal((await req("PUT", `/${A}/heads/1`, "", "h2", { "if-none-match": "*" })).status, 412);
	assert.equal(s3.text(`${A}/heads/1`), "h");
	// but If-None-Match on a delete is no excuse
	s3.calls.length = 0;
	assert.equal((await req("DELETE", `/${A}/heads/1`, "", undefined, { "if-none-match": "*" })).status, 204);
	assert.equal(trashed().length, 1);
});

test("if the copy fails, so does the request, and nothing is lost", async () => {
	const { DB, s3, req, trashed } = setup();
	s3.put(`${A}/data/1`, "one");
	s3.put(`${A}/data/2`, "two");
	s3.state.failCopy = true;
	const errors: string[] = [];
	console.error = (m: string) => errors.push(m);
	assert.equal((await req("DELETE", `/${A}/data/1`)).status, 503);
	assert.equal((await req("POST", `/${A}`, "delete", deleting("data/1", "data/2"))).status, 503);
	assert.equal((await req("PUT", `/${A}/data/2`, "", "junk")).status, 503);
	assert.equal(s3.text(`${A}/data/1`), "one");
	assert.equal(s3.text(`${A}/data/2`), "two");
	assert.deepEqual(trashed(), []);
	assert.ok(!s3.calls.some((c) => c.startsWith("DELETE") || c.startsWith("POST") || c.startsWith("PUT")));
	// the space the write reserved is given back
	assert.equal(sql(DB, "SELECT used FROM accounts")[0].used, 0);
	assert.ok(errors.every((e) => JSON.parse(e).event === "trash failed" && !e.includes("@")));
});

test("locks and the change marker skip the trash", async () => {
	const { s3, req, trashed } = setup();
	assert.ok(trash.exempt(LOCK));
	assert.ok(trash.exempt(`omacloud-e2/${LOCK}`));
	assert.ok(trash.exempt("omacloud-coordination/changed"));
	// answered join requests: approving at the cap mustn't fail, nor a restore bring them back
	assert.ok(trash.exempt("omacloud-coordination/requests/6c36-f628-f6cf-7124.json"));
	assert.ok(!trash.exempt("omacloud-coordination/requests/a/b.json"));
	assert.ok(!trash.exempt("omacloud-coordination/grants/6c36-f628-f6cf-7124-0.json"));
	assert.ok(!trash.exempt("locks/short"));
	assert.ok(!trash.exempt("omacloud-coordination/heads/00000000000000000001.json"));
	assert.ok(!trash.exempt("data/locks"));
	for (const k of [LOCK, `omacloud-e1/${LOCK}`, "omacloud-coordination/changed"]) s3.put(`${A}/${k}`, "x");
	s3.calls.length = 0;
	assert.equal((await req("PUT", `/${A}/omacloud-coordination/changed`, "", "y")).status, 200);
	assert.equal((await req("DELETE", `/${A}/${LOCK}`)).status, 204);
	assert.equal((await req("POST", `/${A}`, "delete", deleting(`omacloud-e1/${LOCK}`))).status, 200);
	assert.deepEqual(trashed(), []);
	assert.ok(!s3.calls.some((c) => c.startsWith("HEAD") || c.startsWith("COPY")), s3.calls.join("\n"));
});

test("no request reaches the trash", async () => {
	const { s3, req, trashed } = setup();
	const entry = trash.trashKey(A, Date.now(), "data/secret");
	s3.put(entry, "kept");
	const other = trash.trashKey("u00000000000000bb", Date.now(), "data/theirs");
	s3.put(other, "theirs");
	const refused = [400, 403, 501];
	const tries: [string, string, string?][] = [
		["GET", `/.trash/${A}`],
		["GET", "/.trash", "list-type=2"],
		["GET", `/${A}/../.trash/${A}`],
		["GET", `/${A}/%2E%2E/.trash`, "list-type=2"],
		["GET", `/${A}/..%2F.trash%2F${A}`],
		["GET", `/${A}%2F..%2F.trash`, "list-type=2"],
		["PUT", `/${A}/..%2F.trash%2Fx`],
		["DELETE", `/${A}/..%2F..%2F.trash%2F${A}`],
		["DELETE", `/%2E%2E/.trash/${A}`],
		["POST", `/.trash`, "delete"],
	];
	for (const [method, path, query] of tries) {
		const r = await req(method, path, query ?? "", method === "PUT" || method === "POST" ? deleting("x") : undefined);
		assert.ok(refused.includes(r.status), `${method} ${path} ${query}: ${r.status}`);
	}
	assert.equal((await req("POST", `/${A}`, "delete", deleting(`../.trash/${A}`))).status, 400);
	// listings stay in the folder, whatever they ask for
	for (const q of ["list-type=2", "list-type=2&prefix=..%2F.trash%2F", `list-type=2&start-after=..%2F.trash`, "list-type=2&prefix=.trash"]) {
		const r = await req("GET", `/${A}`, q);
		assert.equal(r.status, 200);
		assert.ok(!/<Key>[^<]*trash/.test(await r.text()), q);
	}
	// a key named like it is just a key in the folder
	assert.equal((await req("PUT", `/${A}/.trash/x`, "", "mine")).status, 200);
	assert.equal(s3.text(`${A}/.trash/x`), "mine");
	assert.equal(s3.text(entry), "kept");
	assert.equal(s3.text(other), "theirs");
	assert.deepEqual(trashed(), [entry, other].sort());
	assert.ok(!s3.calls.some((c) => / \.trash\//.test(c)), s3.calls.join("\n"));
});

const DAY = 86400 * 1000;

test("the purge removes what's older than 14 days, a closed account's too", async () => {
	const { env, DB, s3, trashed } = setup();
	const now = Date.parse("2026-10-07T12:00:00Z");
	const old = trash.trashKey(A, now - 15 * DAY, "data/old");
	const older = trash.trashKey(A, now - 20 * DAY, "data/older");
	const recent = trash.trashKey(A, now - 13 * DAY, "data/recent");
	for (const k of [old, older, recent]) s3.put(k, "12345");
	sql(DB, "INSERT INTO trash (account, bytes, oldest, added) VALUES (?, 15, ?, ?)", A, (now - 20 * DAY) / 1000, (now - 13 * DAY) / 1000);
	// an account that's gone: its trash still goes
	const gone = "u00000000000000cc";
	const goneKey = trash.trashKey(gone, now - 15 * DAY, "data/x");
	s3.put(goneKey, "123");
	sql(DB, "INSERT INTO trash (account, bytes, oldest, added) VALUES (?, 3, ?, ?)", gone, (now - 15 * DAY) / 1000, (now - 15 * DAY) / 1000);
	// and a closed one's
	sql(DB, "UPDATE accounts SET disabled = 1 WHERE id = ?", A);
	const logs: string[] = [];
	const realLog = console.log;
	console.log = (m: string) => logs.push(m);
	try {
		await trash.purge(env, now);
	} finally {
		console.log = realLog;
	}
	assert.deepEqual(trashed(), [recent]);
	const rows = Object.fromEntries(sql(DB, "SELECT account, bytes, oldest FROM trash").map((r) => [r.account, r]));
	assert.equal(rows[A].bytes, 5);
	assert.equal(rows[A].oldest, Math.floor((now - 13 * DAY) / 1000));
	assert.equal(rows[gone].bytes, 0);
	assert.equal(rows[gone].oldest, null);
	assert.ok(logs.some((l) => JSON.parse(l).event === "trash purged"));
	// nothing due: nothing listed
	s3.calls.length = 0;
	await trash.purge(env, now);
	assert.equal(s3.calls.length, 0, s3.calls.join("\n"));
});

/** Run the scheduled restore until it's done; how many runs it took. */
async function restoreAll(env: Env, limit: number): Promise<number> {
	const realLog = console.log;
	console.log = () => {};
	try {
		for (let runs = 1; runs < 100; runs++) {
			await trash.restore(env, Date.now(), limit);
			const pending = await env.DB.prepare("SELECT count(*) AS n FROM restores WHERE done IS NULL").first<{ n: number }>();
			if (pending!.n === 0) return runs;
		}
	} finally {
		console.log = realLog;
	}
	throw new Error("the restore never finished");
}

test("a restore puts the account back as it was, over several runs, and again changes nothing", async () => {
	const { env, DB, s3, req, trashed } = setup();
	const before = Date.now() - 60_000;
	// deleted before the time restored to: stays deleted
	s3.put(trash.trashKey(A, before - 1000, "data/long-gone"), "gone");
	const files: Record<string, string> = {
		config: "config",
		"keys/k": "key",
		"data/1": "pack one",
		"data/2": "pack two",
		"index/i": "index",
		"snapshots/s": "snapshot",
		"omacloud-coordination/heads/00000000000000000001.json": "head 1",
		"omacloud-coordination/grants/d-0.json": "grant",
	};
	for (const [k, v] of Object.entries(files)) s3.put(`${A}/${k}`, v);
	const at = Math.floor(before / 1000) + 30;

	// the attack: everything deleted, some overwritten first (twice), junk added
	assert.equal((await req("PUT", `/${A}/index/i`, "", "junk 1")).status, 200);
	assert.equal((await req("PUT", `/${A}/index/i`, "", "junk 2")).status, 200);
	assert.equal((await req("PUT", `/${A}/omacloud-coordination/grants/d-0.json`, "", "bad grant")).status, 200);
	assert.equal((await req("PUT", `/${A}/data/junk`, "", "junk")).status, 200);
	const keys = Object.keys(files).filter((k) => !k.startsWith("snapshots/") && !k.startsWith("omacloud-coordination/grants"));
	assert.equal((await req("POST", `/${A}`, "delete", deleting(...keys))).status, 200);
	assert.equal((await req("DELETE", `/${A}/snapshots/s`)).status, 204);
	assert.equal(s3.text(`${A}/config`), null);

	sql(DB, "INSERT INTO restores (account, at, requested) VALUES (?, ?, ?)", A, at, Math.floor(Date.now() / 1000));
	// no purge while it's restoring
	const later = Date.now() + 15 * DAY;
	s3.calls.length = 0;
	await trash.purge(env, later);
	assert.equal(s3.calls.length, 0, s3.calls.join("\n"));

	const runs = await restoreAll(env, 3);
	assert.ok(runs >= 3, `${runs} runs`);
	for (const [k, v] of Object.entries(files)) assert.equal(s3.text(`${A}/${k}`), v, k);
	assert.equal(s3.text(`${A}/data/long-gone`), null);
	// made since: left as it is
	assert.equal(s3.text(`${A}/data/junk`), "junk");
	const [r] = sql(DB, "SELECT * FROM restores");
	assert.ok(r.done);
	assert.equal(r.restored, Object.keys(files).length);
	assert.deepEqual(sql(DB, "SELECT * FROM restored"), []);
	assert.equal(sql(DB, "SELECT dirty FROM accounts")[0].dirty, 1);
	// what the restore replaced (the junk over the grant and the index) is in the trash too
	assert.ok(trashed().some((k) => k.endsWith("/grants/d-0.json") && s3.text(k) === "bad grant"));
	assert.ok(trashed().some((k) => k.endsWith("/index/i") && s3.text(k) === "junk 2"));

	// asked again: nothing changes, nothing is copied
	const objects = new Map([...s3.objects].map(([k, v]) => [k, v.etag]));
	sql(DB, "INSERT INTO restores (account, at, requested) VALUES (?, ?, ?)", A, at, Math.floor(Date.now() / 1000) + 1);
	s3.calls.length = 0;
	await restoreAll(env, 4);
	assert.ok(!s3.calls.some((c) => c.startsWith("COPY")), s3.calls.join("\n"));
	assert.deepEqual(new Map([...s3.objects].map(([k, v]) => [k, v.etag])), objects);
	assert.equal(sql(DB, "SELECT restored FROM restores ORDER BY id")[1].restored, 0);
	// and the purge carries on
	await trash.purge(env, later);
	assert.deepEqual(trashed(), []);
});

test("trash keys", () => {
	const k = trash.trashKey(A, 1791374400123, "data/a b/c");
	const p = trash.parseTrashKey(k)!;
	assert.deepEqual(p, { account: A, ms: 1791374400123, key: "data/a b/c" });
	assert.ok(k.startsWith(`.trash/${A}/1791374400123.`));
	assert.equal(trash.parseTrashKey(`${A}/data/x`), null);
	// they list in time order
	assert.ok(trash.trashKey(A, 999, "z") < trash.trashKey(A, 1000, "a"));
});

test("nothing leaves the trash before 14 days; at its cap, deleting and replacing are refused, creating isn't", async () => {
	const { env, DB, s3, req, trashed, outbox } = setup(10);
	for (const k of ["a", "b", "c", "d"]) s3.put(`${A}/data/${k}`, "123456");
	const errors: string[] = [];
	console.error = (m: string) => errors.push(m);
	assert.equal((await req("DELETE", `/${A}/data/a`)).status, 204);
	assert.equal(outbox.length, 0);
	// over the cap with this one: it goes, and the admin hears
	assert.equal((await req("DELETE", `/${A}/data/b`)).status, 204);
	assert.equal(outbox.length, 1);
	assert.equal(outbox[0].to, "admin@example.com");
	assert.match(outbox[0].text, new RegExp(A));
	assert.ok(!outbox[0].text.includes("a@example.com") && !outbox[0].text.includes(KEY));
	// now nothing more is destroyed
	const refused = async (r: Response) => {
		assert.equal(r.status, 400);
		assert.match(await r.text(), /<Code>TooMuchDeleted<\/Code><Message>Too much was deleted/);
	};
	await refused(await req("DELETE", `/${A}/data/c`));
	await refused(await req("PUT", `/${A}/data/c`, "", "junk"));
	await refused(await req("POST", `/${A}`, "delete", deleting("data/c", "data/d")));
	await refused(await req("POST", `/${A}/data/d`, "uploadId=U", "<CompleteMultipartUpload/>"));
	assert.equal(s3.text(`${A}/data/c`), "123456");
	assert.equal(s3.text(`${A}/data/d`), "123456");
	assert.equal(sql(DB, "SELECT used FROM accounts")[0].used, 0);
	// but new objects can be made, and nothing is no loss
	assert.equal((await req("PUT", `/${A}/data/new`, "", "x")).status, 200);
	assert.equal((await req("DELETE", `/${A}/data/none`)).status, 204);
	// told once a day
	assert.equal(outbox.length, 1);
	sql(DB, "UPDATE trash SET alerted = alerted - 86400");
	await refused(await req("DELETE", `/${A}/data/c`));
	assert.equal(outbox.length, 2);
	assert.ok(errors.every((e) => !e.includes("@")));
	// and the purge takes nothing early, however full
	await trash.purge(env, Date.now() + 13 * DAY);
	assert.equal(trashed().length, 2);
	// once it's expired, deleting works again
	await trash.purge(env, Date.now() + 15 * DAY);
	assert.deepEqual(trashed(), []);
	assert.equal((await req("DELETE", `/${A}/data/c`)).status, 204);
});

test("a single delete or put lands only on the version it kept", async () => {
	const { s3, req, trashed } = setup();
	s3.put(`${A}/data/1`, "v1");
	// another writer lands between the copy and the delete
	const real = s3.fetch;
	let sneak = "";
	globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
		if (sneak && (init?.method === "DELETE" || (init?.method === "PUT" && !new Headers(init.headers).has("x-amz-copy-source"))))
			s3.put(`${A}/data/1`, sneak);
		return real(input, init);
	}) as typeof fetch;
	sneak = "v2";
	assert.equal((await req("DELETE", `/${A}/data/1`)).status, 503);
	assert.equal(s3.text(`${A}/data/1`), "v2");
	sneak = "v3";
	assert.equal((await req("PUT", `/${A}/data/1`, "", "mine")).status, 503);
	assert.equal(s3.text(`${A}/data/1`), "v3");
	// tried again, it keeps v3 and goes ahead
	sneak = "";
	assert.equal((await req("PUT", `/${A}/data/1`, "", "mine")).status, 200);
	assert.ok(trashed().some((k) => s3.text(k) === "v3"));
	// a delete of nothing needn't reach the bucket, so can't delete what came since
	s3.calls.length = 0;
	assert.equal((await req("DELETE", `/${A}/data/none`)).status, 204);
	assert.deepEqual(s3.calls, [`HEAD ${A}/data/none`]);
});

test("a bucket that doesn't take If-Match still deletes and replaces, after keeping", async () => {
	const { s3, req, trashed } = setup();
	s3.put(`${A}/data/1`, "v1");
	s3.put(`${A}/grants/g`, "old grant");
	const real = s3.fetch;
	globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
		if (new Headers(init?.headers).has("if-match"))
			return new Response("<Error><Code>NotImplemented</Code></Error>", { status: 501 });
		return real(input, init);
	}) as typeof fetch;
	assert.equal((await req("DELETE", `/${A}/data/1`)).status, 204);
	assert.equal(s3.text(`${A}/data/1`), null);
	assert.equal((await req("PUT", `/${A}/grants/g`, "", "new grant")).status, 200);
	assert.equal(s3.text(`${A}/grants/g`), "new grant");
	const kept = trashed().map((k) => s3.text(k));
	assert.ok(kept.includes("v1") && kept.includes("old grant"), "both kept first");
});

test("objects stay small enough to keep: over 5 GiB is refused, and a restore leaves one as it is", async () => {
	const { env, DB, s3, req } = setup(100 * 1024 ** 3);
	const big = String(trash.MAX_OBJECT + 1);
	const r = await req("PUT", `/${A}/data/big`, "", "x", { "content-length": big });
	assert.equal(r.status, 400);
	assert.match(await r.text(), /EntityTooLarge/);
	assert.equal(sql(DB, "SELECT used FROM accounts")[0].used, 0);
	s3.state.parts.set("BIG", [3 * 1024 ** 3, 3 * 1024 ** 3]);
	assert.equal((await req("POST", `/${A}/data/big`, "uploadId=BIG", "<CompleteMultipartUpload/>")).status, 400);
	assert.equal(s3.text(`${A}/data/big`), null);
	s3.state.parts.set("OK", [5, 5]);
	assert.equal((await req("POST", `/${A}/data/ok`, "uploadId=OK", "<CompleteMultipartUpload/>")).status, 200);
	// one there already (made before the limit) can't be copied, so stays
	s3.put(`${A}/data/huge`, "pretend");
	s3.state.sizes.set(`${A}/data/huge`, trash.MAX_OBJECT + 1);
	assert.equal((await req("DELETE", `/${A}/data/huge`)).status, 400);
	assert.equal(s3.text(`${A}/data/huge`), "pretend");
	// and a restore that would replace it leaves it, and finishes
	s3.put(trash.trashKey(A, Date.now() - 1000, "data/huge"), "older");
	sql(DB, "INSERT INTO restores (account, at, requested) VALUES (?, ?, ?)", A, Math.floor(Date.now() / 1000) - 60, Math.floor(Date.now() / 1000));
	await restoreAll(env, 10);
	assert.equal(s3.text(`${A}/data/huge`), "pretend");
	assert.equal(sql(DB, "SELECT restored FROM restores")[0].restored, 0);
});

test("a restore failing 5 runs in a row is given up on, the admin told, and holds up nothing", async () => {
	const { env, DB, s3, outbox, trashed } = setup();
	const B = "u00000000000000bb";
	const t = Math.floor(Date.now() / 1000);
	s3.put(trash.trashKey(A, Date.now() - 1000, "data/a"), "a");
	s3.put(trash.trashKey(B, Date.now() - 1000, "data/b"), "b");
	sql(DB, "INSERT INTO restores (account, at, requested) VALUES (?, ?, ?)", A, t - 60, t);
	sql(DB, "INSERT INTO restores (account, at, requested) VALUES (?, ?, ?)", B, t - 60, t + 1);
	sql(DB, "INSERT INTO trash (account, bytes, oldest, added) VALUES (?, 1, 0, 0)", A);
	s3.state.failCopyOf = `.trash/${A}/`;
	const errors: string[] = [];
	console.error = (m: string) => errors.push(m);
	const later = Date.now() + 15 * DAY;
	for (let run = 1; run <= trash.RESTORE_TRIES; run++) {
		// the account's trash waits while the restore may still work
		await trash.purge(env, later);
		assert.equal(trashed().length, 2);
		await trash.restore(env);
	}
	const [a, b] = sql(DB, "SELECT failed, done, attempts FROM restores ORDER BY id");
	assert.ok(a.failed && !a.done);
	assert.equal(a.attempts, trash.RESTORE_TRIES);
	assert.equal(b.done, null);
	assert.equal(outbox.length, 1);
	assert.match(outbox[0].subject, /restore 1 failed/);
	assert.ok(errors.some((e) => JSON.parse(e).event === "restore failed"));
	// the next goes ahead, and the trash purges again
	console.log = () => {};
	await trash.restore(env);
	assert.equal(s3.text(`${B}/data/b`), "b");
	assert.ok(sql(DB, "SELECT done FROM restores WHERE account = ?", B)[0].done);
	await trash.purge(env, later);
	assert.ok(!trashed().some((k) => k.startsWith(`.trash/${A}/`)));
});

test("the trash counts only what it holds, and no account's trash is forgotten", async () => {
	const { env, DB, s3, req, trashed } = setup();
	// a copy that fails or finds nothing adds nothing
	s3.put(`${A}/data/1`, "12345");
	s3.state.failCopy = true;
	console.error = () => {};
	assert.equal((await req("DELETE", `/${A}/data/1`)).status, 503);
	s3.state.failCopy = false;
	const real = s3.fetch;
	globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
		const r = await real(input, init);
		// gone between the look and the copy
		if (init?.method === "HEAD") s3.objects.delete(`${A}/data/1`);
		return r;
	}) as typeof fetch;
	assert.equal((await req("DELETE", `/${A}/data/1`)).status, 204);
	globalThis.fetch = real;
	assert.deepEqual(sql(DB, "SELECT bytes FROM trash"), []);

	// added to while the purge walks it: still known, and purged later
	const now = Date.now();
	s3.put(trash.trashKey(A, now - 15 * DAY, "data/old"), "old");
	sql(DB, "INSERT INTO trash (account, bytes, oldest, added) VALUES (?, 3, ?, ?)", A, Math.floor((now - 15 * DAY) / 1000), Math.floor((now - 15 * DAY) / 1000));
	globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
		const r = await real(input, init);
		if (new URL(String(input)).searchParams.get("list-type") === "2") {
			s3.put(trash.trashKey(A, Date.now(), "data/new"), "new!");
			await trash.added(DB, A, 4, Date.now());
		}
		return r;
	}) as typeof fetch;
	await trash.purge(env, now);
	globalThis.fetch = real;
	let [t] = sql(DB, "SELECT bytes, oldest FROM trash");
	assert.notEqual(t.oldest, null);
	assert.equal(t.bytes, 4);
	await trash.purge(env, now + 15 * DAY);
	assert.deepEqual(trashed(), []);

	// a counting walk (the sweep asks for one daily) sets the total from the listing
	s3.put(trash.trashKey(A, now - DAY, "data/x"), "xx");
	sql(DB, "UPDATE trash SET bytes = 999, oldest = 0, added = 0, recount = 1");
	await trash.purge(env, now);
	[t] = sql(DB, "SELECT bytes, oldest FROM trash");
	assert.equal(t.bytes, 2);
	assert.equal(t.oldest, Math.floor((now - DAY) / 1000));

	// trash D1 lost track of is found by the daily sweep
	const B = "u00000000000000bb";
	s3.put(trash.trashKey(B, now - 15 * DAY, "data/lost"), "lost");
	await trash.purge(env, now);
	assert.ok(trashed().some((k) => k.startsWith(`.trash/${B}/`)));
	await trash.sweep(env, now);
	await trash.purge(env, now);
	assert.ok(!trashed().some((k) => k.startsWith(`.trash/${B}/`)));
	assert.deepEqual({ ...sql(DB, "SELECT bytes, oldest FROM trash WHERE account = ?", B)[0] }, { bytes: 0, oldest: null });
	// once a day
	s3.calls.length = 0;
	await trash.sweep(env, now + 1000);
	assert.equal(s3.calls.length, 0);
});

test("an account whose listing fails holds up no other, and its walk starts over", async () => {
	const { env, DB, s3, trashed } = setup();
	const B = "u00000000000000bb";
	const now = Date.now();
	const s = Math.floor((now - 15 * DAY) / 1000);
	for (const acct of [A, B]) {
		s3.put(trash.trashKey(acct, now - 15 * DAY, "data/old"), "old");
		sql(DB, "INSERT INTO trash (account, bytes, oldest, added) VALUES (?, 3, ?, ?)", acct, s, s);
	}
	// A is mid-walk, at a place in the listing the bucket no longer takes
	sql(DB, "UPDATE trash SET recount = 1, walk_from = ?, walk_token = 'stale', walk_sum = 42 WHERE account = ?", s, A);
	const real = s3.fetch;
	globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
		const u = new URL(String(input));
		if (u.searchParams.get("prefix") === `${trash.TRASH}${A}/`) return new Response("<Error><Code>InvalidArgument</Code></Error>", { status: 400 });
		return real(input, init);
	}) as typeof fetch;
	await trash.purge(env, now);
	globalThis.fetch = real;
	assert.ok(!trashed().some((k) => k.startsWith(`.trash/${B}/`)), "B purged though A failed");
	const [a] = sql(DB, "SELECT recount, walk_from, walk_token, walk_sum FROM trash WHERE account = ?", A);
	assert.deepEqual({ ...a }, { recount: 1, walk_from: null, walk_token: null, walk_sum: 0 });
	// next run, A's walk starts over and finishes
	await trash.purge(env, now);
	assert.deepEqual(trashed(), []);
});
