import assert from "node:assert/strict";
import { afterEach, test } from "node:test";

import * as db from "../src/db.ts";
import { back, credentials, start } from "../src/signin.ts";
import { sha256Hex } from "../src/sigv4.ts";
import { count, openUploads } from "../src/usage.ts";
import * as waitlist from "../src/waitlist.ts";
import { d1 } from "./d1.ts";

const realFetch = globalThis.fetch;
afterEach(() => {
	globalThis.fetch = realFetch;
});

interface Sent {
	to: string;
	subject: string;
	text: string;
	html: string;
}

/** A test environment: a fresh database, an outbox, and limiters that say `limited`. */
function setup(over: Partial<Record<string, unknown>> = {}) {
	const DB = d1();
	const outbox: Sent[] = [];
	const limited = { now: false };
	const limiter = { limit: async () => ({ success: !limited.now }) };
	const env = {
		DB,
		PUBLIC_URL: "https://omacloud.computer",
		STORAGE_HOST: "storage.omacloud.computer",
		REGION: "omacloud",
		SIGNUPS: "invite",
		DEFAULT_QUOTA: "1000",
		GOOGLE_CLIENT_ID: "client",
		GOOGLE_SECRET: "secret",
		GOOGLE_TOKEN_URL: "https://google.test/token",
		MASTER_KEY: "ab".repeat(32),
		ADMIN_EMAIL: "admin@example.com",
		TURNSTILE_SITE_KEY: "site",
		TURNSTILE_SECRET: "turnstile-secret",
		TURNSTILE_VERIFY_URL: "https://turnstile.test/siteverify",
		UPSTREAM_ENDPOINT: "https://bucket.test",
		UPSTREAM_REGION: "r",
		UPSTREAM_BUCKET: "everyone",
		UPSTREAM_KEY_ID: "k",
		UPSTREAM_SECRET: "s",
		EMAIL: {
			send: async (m: { to: string; subject: string; text: string; html: string }) => {
				if (fail.now || fail.to === m.to) throw Object.assign(new Error("nope"), { code: "E_RATE_LIMIT_EXCEEDED" });
				outbox.push({ to: m.to, subject: m.subject, text: m.text, html: m.html });
				return { messageId: "m" };
			},
		},
		SIGNIN_LIMIT: limiter,
		CREDENTIALS_LIMIT: limiter,
		WAITLIST_LIMIT: limiter,
		...over,
	} as unknown as Env;
	const fail: { now: boolean; to?: string } = { now: false };
	const waits: Promise<unknown>[] = [];
	const ctx = { waitUntil: (p: Promise<unknown>) => waits.push(p) } as unknown as ExecutionContext;
	const settle = () => Promise.all(waits.splice(0));
	return { env, DB, outbox, limited, fail, ctx, settle };
}

const sql = (DB: D1Database, q: string, ...p: unknown[]) =>
	(DB as unknown as { sqlite: import("node:sqlite").DatabaseSync }).sqlite.prepare(q).all(...(p as never[]));

/** Google and Turnstile, standing in: `who` signs in, Turnstile says `human`. */
function stubFetch(who: { sub: string; email: string }, human = true) {
	globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
		const url = String(input);
		if (url.startsWith("https://google.test/token")) {
			const claims = { iss: "https://accounts.google.com", aud: "client", email_verified: true, ...who };
			const part = Buffer.from(JSON.stringify(claims)).toString("base64url");
			return Response.json({ id_token: `e30.${part}.sig` });
		}
		if (url.startsWith("https://turnstile.test/")) {
			const body = JSON.parse(String(init?.body));
			assert.equal(body.secret, "turnstile-secret");
			return Response.json(
				human && body.response === "ok"
					? { success: true, hostname: "omacloud.computer" }
					: { success: false, "error-codes": ["invalid-input-response"] },
			);
		}
		throw new Error(`unexpected fetch ${url}`);
	}) as typeof fetch;
}

/** A Google sign-in coming back from Google: where it sends the browser. */
async function signIn(env: Env, sub: string, email: string): Promise<URL> {
	stubFetch({ sub, email });
	const id = await db.startSignin(env.DB, 5000, "aa".repeat(8), "bb".repeat(32));
	const r = await back(new URL(`https://omacloud.computer/auth/google/callback?state=${id}&code=c`), env);
	assert.equal(r.status, 303);
	return new URL(r.headers.get("location")!);
}

function form(email: string, turnstile = "ok", ip = "203.0.113.9"): Request {
	const body = new URLSearchParams({ email, "cf-turnstile-response": turnstile }).toString();
	return new Request("https://omacloud.computer/waitlist", {
		method: "POST",
		headers: {
			"content-type": "application/x-www-form-urlencoded",
			"content-length": String(body.length),
			"cf-connecting-ip": ip,
		},
		body,
	});
}

const CHECK = "Check your inbox";

/** A confirmation link opened (as a mail scanner would), then its button pressed. */
async function follow(env: Env, link: string): Promise<Response> {
	const shown = waitlist.confirmPage(new URL(link));
	const token = (await shown.text()).match(/name=token value="([0-9a-f]{64})"/)?.[1] ?? "";
	const body = new URLSearchParams({ token }).toString();
	return waitlist.confirmed(
		new Request("https://omacloud.computer/waitlist/confirm", {
			method: "POST",
			headers: {
				"content-type": "application/x-www-form-urlencoded",
				"content-length": String(body.length),
				"cf-connecting-ip": "203.0.113.9",
			},
			body,
		}),
		env,
	);
}

test("an uninvited Google sign-in is put on the waitlist, confirmed", async () => {
	const { env, DB } = setup();
	const to = await signIn(env, "g1", "Carol@Example.com");
	assert.equal(to.searchParams.get("error"), "invite");
	assert.equal(to.searchParams.get("waitlist"), "1");
	const [row] = sql(DB, "SELECT * FROM waitlist");
	assert.equal(row.email, "carol@example.com");
	assert.equal(row.google_sub, "g1");
	assert.ok(row.confirmed);
	// again: still one row, seen again
	await signIn(env, "g1", "carol@example.com");
	assert.equal(sql(DB, "SELECT * FROM waitlist").length, 1);
	// invited: an account, and a code for the computer
	sql(DB, "INSERT INTO invites (email, created) VALUES ('alice@example.com', 0)");
	const ok = await signIn(env, "g2", "alice@example.com");
	assert.ok(ok.searchParams.get("code"));
	assert.equal(ok.searchParams.get("waitlist"), null);
});

test("the form: Turnstile, a confirmation link, and the list only once it's followed", async () => {
	const { env, DB, outbox, ctx, settle } = setup();
	stubFetch({ sub: "x", email: "x" });
	const r = await waitlist.request(form(" Dave@Example.com "), env, ctx);
	await settle();
	assert.equal(r.status, 200);
	assert.match(await r.text(), new RegExp(CHECK));
	const [row] = sql(DB, "SELECT * FROM waitlist");
	assert.equal(row.email, "dave@example.com");
	assert.equal(row.confirmed, null);
	assert.equal(outbox.length, 1);
	assert.equal(outbox[0].to, "dave@example.com");
	const link = outbox[0].text.match(/https:\/\/omacloud\.computer\/waitlist\/confirm\?token=([0-9a-f]{64})/)!;
	assert.ok(link);
	// only the token's hash is kept
	assert.notEqual(row.token_hash, link[1]);
	assert.ok(!JSON.stringify(sql(DB, "SELECT * FROM waitlist")).includes(link[1]));
	// not on the list yet: the digest doesn't count it
	assert.equal(await db.waitingSince(DB, 0, db.now() + 1), 0);
	// opening the link alone confirms nothing
	assert.equal(waitlist.confirmPage(new URL(link[0])).status, 200);
	assert.equal(sql(DB, "SELECT confirmed FROM waitlist")[0].confirmed, null);
	const ok = await follow(env, link[0]);
	assert.equal(ok.status, 200);
	assert.equal(sql(DB, "SELECT confirmed IS NOT NULL AS c FROM waitlist")[0].c, 1);
	assert.equal(await db.waitingSince(DB, 0, db.now() + 1), 1);
	// a link works once
	const again = await follow(env, link[0]);
	assert.equal(again.status, 400);
	const bogus = await follow(env, `https://omacloud.computer/waitlist/confirm?token=${"0".repeat(64)}`);
	assert.equal(bogus.status, 400);
	assert.equal(await bogus.text(), await again.text());
	assert.equal(waitlist.confirmPage(new URL("https://omacloud.computer/waitlist/confirm?token=<b>")).status, 400);
});

test("Turnstile refusing, a missing secret, and the rate limit", async () => {
	const { env, DB, outbox, ctx, limited } = setup();
	stubFetch({ sub: "x", email: "x" }, false);
	assert.equal((await waitlist.request(form("eve@example.com"), env, ctx)).status, 403);
	stubFetch({ sub: "x", email: "x" });
	assert.equal((await waitlist.request(form("eve@example.com", ""), env, ctx)).status, 403);
	assert.equal(sql(DB, "SELECT * FROM waitlist").length, 0);
	// Turnstile from another site
	globalThis.fetch = (async () => Response.json({ success: true, hostname: "evil.test" })) as unknown as typeof fetch;
	assert.equal((await waitlist.request(form("eve@example.com"), env, ctx)).status, 403);
	const bare = setup({ TURNSTILE_SECRET: undefined });
	assert.equal((await waitlist.request(form("eve@example.com"), bare.env, bare.ctx)).status, 503);
	limited.now = true;
	assert.equal((await waitlist.request(form("eve@example.com"), env, ctx)).status, 429);
	assert.equal((await follow(env, `https://omacloud.computer/waitlist/confirm?token=${"0".repeat(64)}`)).status, 429);
	assert.equal(outbox.length, 0);
});

test("the form answers the same whoever the address is, and emails again only once a link expires", async () => {
	const { env, DB, outbox, ctx, settle } = setup();
	stubFetch({ sub: "x", email: "x" });
	sql(DB, "INSERT INTO invites (email, created) VALUES ('inv@example.com', 0)");
	sql(DB, "INSERT INTO accounts (id, google_sub, email, created, quota) VALUES ('u1', 's', 'Has@Example.com', 0, 1)");
	await signIn(env, "g3", "waiting@example.com");
	stubFetch({ sub: "x", email: "x" });
	const bodies = new Set<string>();
	for (const e of ["inv@example.com", "has@example.com", "waiting@example.com", "new@example.com", "new@example.com"]) {
		const r = await waitlist.request(form(e), env, ctx);
		assert.equal(r.status, 200);
		const text = await r.text();
		assert.ok(!text.includes(e.split("@")[0] + "@"), "the address isn't echoed");
		bodies.add(text);
	}
	await settle();
	assert.equal(bodies.size, 1);
	assert.deepEqual(
		outbox.map((m) => m.to),
		["new@example.com"],
	);
	// a day later the first link still works, so no second one cuts it short
	sql(DB, "UPDATE waitlist SET token_sent = token_sent - 86400 WHERE email = 'new@example.com'");
	await waitlist.request(form("new@example.com"), env, ctx);
	await settle();
	assert.equal(outbox.length, 1);
	// once it has expired the address may ask again
	sql(DB, "UPDATE waitlist SET token_sent = token_sent - 86400 WHERE email = 'new@example.com'");
	await waitlist.request(form("new@example.com"), env, ctx);
	await settle();
	assert.equal(outbox.length, 2);
});

test("bad addresses are refused and never echoed", async () => {
	const { env, ctx } = setup();
	for (const bad of ["<script>@x.com", "a@b", "a b@c.com", "a@b.com@c.com", ".a@b.com", "a..b@c.com", "a@-b.com", "x".repeat(250) + "@e.com"]) {
		assert.equal(waitlist.normalEmail(bad), null, bad);
		const r = await waitlist.request(form(bad), env, ctx);
		assert.equal(r.status, 400);
		assert.ok(!(await r.text()).includes("<script>"));
	}
	assert.equal(waitlist.normalEmail(" A.B+c@Mail.Example.org "), "a.b+c@mail.example.org");
});

test("confirmation links expire, and expired requests are forgotten", async () => {
	const { env, DB, outbox, ctx, settle } = setup();
	stubFetch({ sub: "x", email: "x" });
	await waitlist.request(form("late@example.com"), env, ctx);
	await settle();
	const link = outbox[0].text.match(/https:\S+token=[0-9a-f]{64}/)![0];
	sql(DB, "UPDATE waitlist SET token_sent = token_sent - ?, created = created - ?", db.CONFIRM_SECONDS, db.CONFIRM_SECONDS);
	assert.equal((await follow(env, link)).status, 400);
	await waitlist.purge(env);
	assert.equal(sql(DB, "SELECT * FROM waitlist").length, 0);
});

test("a Google sign-in confirms an address waiting for its link", async () => {
	const { env, DB, outbox, ctx, settle } = setup();
	stubFetch({ sub: "x", email: "x" });
	await waitlist.request(form("frank@example.com"), env, ctx);
	await settle();
	const link = outbox[0].text.match(/https:\S+token=[0-9a-f]{64}/)![0];
	await signIn(env, "g4", "Frank@example.com");
	const rows = sql(DB, "SELECT * FROM waitlist");
	assert.equal(rows.length, 1);
	assert.equal(rows[0].google_sub, "g4");
	assert.ok(rows[0].confirmed);
	assert.equal(rows[0].token_hash, null);
	// the old link is spent; a purge leaves the confirmed entry
	assert.equal((await follow(env, link)).status, 400);
	sql(DB, "UPDATE waitlist SET created = 0");
	await waitlist.purge(env);
	assert.equal(sql(DB, "SELECT * FROM waitlist").length, 1);
});

test("invitation emails: sent once, marked only when sent", async () => {
	const { env, DB, outbox, fail } = setup();
	sql(DB, "INSERT INTO invites (email, created, notified) VALUES ('old@example.com', 0, 0)");
	sql(DB, "INSERT INTO invites (email, created) VALUES ('a@example.com', 1), ('b@example.com', 2), ('joined@example.com', 3)");
	sql(DB, "INSERT INTO accounts (id, google_sub, email, created, quota) VALUES ('u1', 's', 'Joined@example.com', 0, 1)");
	fail.now = true;
	await waitlist.notifyInvites(env);
	// the one who already has an account needs no email; the others wait for theirs
	assert.equal(sql(DB, "SELECT * FROM invites WHERE notified IS NULL").length, 2);
	fail.now = false;
	// not due yet: a failed email waits before it's tried again
	await waitlist.notifyInvites(env);
	assert.equal(outbox.length, 0);
	sql(DB, "UPDATE invites SET notify_after = 0");
	await waitlist.notifyInvites(env);
	assert.deepEqual(
		outbox.map((m) => m.to),
		["a@example.com", "b@example.com"],
	);
	assert.match(outbox[0].text, /sign in with Google as a@example\.com/);
	assert.match(outbox[0].text, /install\.sh/);
	assert.match(outbox[0].html, /https:\/\/omacloud\.computer/);
	assert.equal(sql(DB, "SELECT * FROM invites WHERE notified IS NULL").length, 0);
	await waitlist.notifyInvites(env);
	assert.equal(outbox.length, 2);
});

test("an address that can't be emailed holds up no other invitation", async () => {
	const { env, DB, outbox, fail } = setup();
	sql(DB, "INSERT INTO invites (email, created) VALUES ('bad@example.com', 1), ('good@example.com', 2)");
	fail.to = "bad@example.com";
	await waitlist.notifyInvites(env);
	assert.deepEqual(
		outbox.map((m) => m.to),
		["good@example.com"],
	);
	const [bad] = sql(DB, "SELECT * FROM invites WHERE email = 'bad@example.com'");
	assert.equal(bad.notified, null);
	assert.equal(bad.notify_tries, 1);
	assert.ok((bad.notify_after as number) > Date.now() / 1000, "tried again later");
});

test("the admin's digest: daily, only with news, counting confirmed entries", async () => {
	const { env, DB, outbox, fail } = setup();
	await waitlist.digest(env);
	assert.equal(outbox.length, 0);
	await signIn(env, "g5", "one@example.com");
	await signIn(env, "g6", "two@example.com");
	sql(DB, "INSERT INTO waitlist (email, created, last_seen, token_hash, token_sent) VALUES ('typed@example.com', 0, 0, 'h', ?)", db.now());
	fail.now = true;
	await waitlist.digest(env);
	assert.equal(await db.lastRan(DB, "digest"), 0);
	fail.now = false;
	await waitlist.digest(env);
	assert.equal(outbox.length, 1);
	assert.equal(outbox[0].to, "admin@example.com");
	assert.match(outbox[0].subject, /2 new/);
	assert.ok(!outbox[0].text.includes("one@example.com"));
	// not again today, even with news
	sql(DB, "UPDATE waitlist SET confirmed = ? WHERE email = 'one@example.com'", db.now() + 10);
	await waitlist.digest(env);
	assert.equal(outbox.length, 1);
	sql(DB, "UPDATE jobs SET ran = ran - 86400");
	await waitlist.digest(env);
	assert.equal(outbox.length, 2);
	assert.match(outbox[1].subject, /1 new/);
});

test("a Google sign-in past MAX_KEYS retires the oldest key; credentials are rate limited", async () => {
	const { env, DB, limited } = setup();
	sql(DB, "INSERT INTO invites (email, created) VALUES ('k@example.com', 0)");
	const verifier = "v".repeat(43);
	const challenge = await sha256Hex(verifier);
	const ask = async () => {
		stubFetch({ sub: "gk", email: "k@example.com" });
		const id = await db.startSignin(DB, 5000, "aa".repeat(8), challenge);
		const to = new URL(
			(await back(new URL(`https://omacloud.computer/auth/google/callback?state=${id}&code=c`), env)).headers.get("location")!,
		);
		const body = JSON.stringify({ code: to.searchParams.get("code"), verifier });
		return credentials(
			new Request("https://omacloud.computer/api/credentials", {
				method: "POST",
				headers: { "content-length": String(body.length) },
				body,
			}),
			env,
		);
	};
	const first = await (await ask()).json<{ access_key_id: string; bucket: string }>();
	for (let i = 0; i < db.MAX_KEYS + 2; i++) assert.equal((await ask()).status, 200);
	assert.equal(await db.activeKeys(DB, first.bucket), db.MAX_KEYS);
	assert.ok(sql(DB, "SELECT revoked FROM keys WHERE id = ?", first.access_key_id)[0].revoked, "the oldest went");
	limited.now = true;
	const r = await ask();
	assert.equal(r.status, 429);
	assert.match((await r.json<{ error: string }>()).error, /wait a minute/);
});

test("quota is reserved before writing, so writes side by side can't overrun it", async () => {
	const { DB } = setup();
	sql(DB, "INSERT INTO accounts (id, google_sub, email, created, quota) VALUES ('u1', 's', 'q@example.com', 0, 100)");
	const tries = await Promise.all(Array.from({ length: 5 }, () => db.reserve(DB, "u1", 30)));
	assert.equal(tries.filter(Boolean).length, 3);
	assert.equal(sql(DB, "SELECT used FROM accounts")[0].used, 90);
	await db.release(DB, "u1", 30);
	assert.ok(await db.reserve(DB, "u1", 40));
	assert.ok(!(await db.reserve(DB, "u1", 1)));
	await db.release(DB, "u1", 1000);
	assert.equal(sql(DB, "SELECT used FROM accounts")[0].used, 0);
});

test("open multipart uploads are counted, and abandoned ones aborted", async () => {
	const { env } = setup();
	const now = Date.parse("2026-10-07T12:00:00Z");
	const calls: string[] = [];
	globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
		const url = new URL(String(input));
		calls.push(`${init?.method} ${url.pathname}?${url.search.slice(1)}`);
		if (url.searchParams.has("uploads"))
			return new Response(
				"<ListMultipartUploadsResult><IsTruncated>false</IsTruncated>" +
					"<Upload><Key>u1/data/old</Key><UploadId>OLD</UploadId><Initiated>2026-10-05T12:00:00.000Z</Initiated></Upload>" +
					"<Upload><Key>u1/data/new&amp;x</Key><UploadId>NEW</UploadId><Initiated>2026-10-07T11:00:00.000Z</Initiated></Upload>" +
					"</ListMultipartUploadsResult>",
			);
		if (init?.method === "DELETE") return new Response(null, { status: 204 });
		return new Response(
			"<ListPartsResult><IsTruncated>false</IsTruncated><Part><PartNumber>1</PartNumber><Size>7</Size></Part>" +
				"<Part><PartNumber>2</PartNumber><Size>5</Size></Part></ListPartsResult>",
		);
	}) as typeof fetch;
	const r = await openUploads(env, "u1/", now);
	assert.deepEqual(r, { bytes: 12, open: true });
	assert.ok(calls.some((c) => c === "DELETE /everyone/u1/data/old?uploadId=OLD"), calls.join("\n"));
	assert.ok(calls.some((c) => c === "GET /everyone/u1/data/new%26x?uploadId=NEW"), calls.join("\n"));
});

test("a sign-in past the limit is told to the computer, not left waiting", async () => {
	const { env, limited } = setup();
	limited.now = true;
	const state = "ab".repeat(8);
	const url = new URL(`https://omacloud.computer/signin?port=4321&state=${state}&challenge=${"cd".repeat(32)}`);
	const r = await start(new Request(url), url, env);
	assert.equal(r.status, 303);
	assert.equal(r.headers.get("location"), `http://127.0.0.1:4321/callback?state=${state}&error=busy`);
});

test("a usage count that fails is tried again, so reserved space can't stick", async () => {
	const { env, DB } = setup();
	sql(DB, "INSERT INTO accounts (id, google_sub, email, created, quota, used, dirty) VALUES ('u1', 's', 'u@example.com', 0, 100, 100, 1)");
	globalThis.fetch = (async () => new Response("busy", { status: 503 })) as typeof fetch;
	await count(env);
	const [a] = sql(DB, "SELECT used, dirty FROM accounts WHERE id = 'u1'");
	assert.equal(a.dirty, 1, "still due a count");
	assert.equal(a.used, 100);
});
