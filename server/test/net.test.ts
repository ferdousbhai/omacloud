import assert from "node:assert/strict";
import { createHash, createPublicKey, generateKeyPairSync, randomBytes, sign, verify, type KeyObject } from "node:crypto";
import { test } from "node:test";

import worker from "../src/index.ts";
import { prefixOf } from "../src/net/auth.ts";
import { d1 } from "./d1.ts";

const rsa = generateKeyPairSync("rsa", { modulusLength: 2048 });
const CREDIT_KEY = JSON.stringify(rsa.privateKey.export({ format: "jwk" }));

function setup(starter = "10") {
	const DB = d1();
	const env = { PUBLIC_URL: "https://omacloud.computer", DB, CREDIT_KEY, STARTER_TOKENS: starter } as unknown as Env;
	return { env, sqlite: DB.sqlite };
}

type Init = { method?: string; body?: unknown; headers?: Record<string, string>; ip?: string; asn?: number };

function request(path: string, init: Init = {}): Request {
	const headers = new Headers(init.headers);
	headers.set("cf-connecting-ip", init.ip ?? "203.0.113.7");
	const body = init.body === undefined ? undefined : typeof init.body === "string" ? init.body : JSON.stringify(init.body);
	const req = new Request(`https://omacloud.computer${path}`, { method: init.method ?? "GET", headers, body });
	if (init.asn !== undefined) Object.defineProperty(req, "cf", { value: { asn: init.asn } });
	return req;
}

const call = (env: Env, path: string, init: Init = {}) => worker.fetch(request(path, init) as never, env);

const sha256 = (data: string | Uint8Array) => createHash("sha256").update(data).digest("hex");
const now = () => Math.floor(Date.now() / 1000);

// a storage node: its Ed25519 key, signing the way the spec says
function node() {
	const { privateKey, publicKey } = generateKeyPairSync("ed25519");
	const id = Buffer.from(publicKey.export({ format: "jwk" }).x as string, "base64url").toString("hex");
	return { id, privateKey };
}

function signed(n: { id: string; privateKey: KeyObject }, method: string, path: string, body: string, time = now()) {
	const text = `omacloud-node-v1\n${method}\n${path}\n${time}\n${sha256(body)}`;
	return { "x-node": n.id, "x-time": String(time), "x-sig": sign(null, Buffer.from(text), n.privateKey).toString("hex") };
}

function heartbeat(env: Env, n: ReturnType<typeof node>, init: { capacity?: number; ip?: string; asn?: number; time?: number } = {}) {
	const body = JSON.stringify({ addr: { id: n.id, addrs: [{ Ip: "198.51.100.1:4433" }] }, capacity: init.capacity ?? 1e12 });
	return call(env, "/api/net/nodes", { method: "POST", body, headers: signed(n, "POST", "/api/net/nodes", body, init.time), ip: init.ip, asn: init.asn });
}

test("prefixes: a /24 for IPv4, a /48 for IPv6", () => {
	assert.equal(prefixOf("203.0.113.7"), "203.0.113.0/24");
	assert.equal(prefixOf("2001:db8:12:34::1"), "2001:db8:12::/48");
	assert.equal(prefixOf("2001:0DB8:0000:ffff:1:2:3:4"), "2001:db8:0::/48");
	assert.equal(prefixOf("::1"), "0:0:0::/48");
	assert.equal(prefixOf("fe80::1%eth0"), "fe80:0:0::/48");
	assert.equal(prefixOf("::ffff:198.51.100.9"), "198.51.100.0/24");
	assert.equal(prefixOf("64:ff9b::198.51.100.9"), "64:ff9b:0::/48");
	for (const bad of ["", "unknown", "1.2.3", "1.2.3.256", "1::2::3", "1:2:3:4:5:6:7:8:9", "12345::"]) assert.equal(prefixOf(bad), null, bad);
});

test("a node registers with a good signature, and not a bad or stale one", async () => {
	const { env, sqlite } = setup();
	const n = node();
	const res = await heartbeat(env, n, { ip: "2001:db8:aa:1::5", asn: 64500 });
	assert.equal(res.status, 200);
	assert.deepEqual(await res.json(), { asn: 64500, prefix: "2001:db8:aa::/48" });
	// without cf, the ASN is 0
	assert.deepEqual(await (await heartbeat(env, n)).json(), { asn: 0, prefix: "203.0.113.0/24" });

	const body = JSON.stringify({ addr: {}, capacity: 1 });
	const other = node();
	// signed by another key
	const forged = { ...signed(other, "POST", "/api/net/nodes", body), "x-node": n.id };
	assert.equal((await call(env, "/api/net/nodes", { method: "POST", body, headers: forged })).status, 401);
	// signed over another body
	const swapped = signed(n, "POST", "/api/net/nodes", "{}");
	assert.equal((await call(env, "/api/net/nodes", { method: "POST", body, headers: swapped })).status, 401);
	// stale, and from the future
	assert.equal((await heartbeat(env, n, { time: now() - 301 })).status, 401);
	assert.equal((await heartbeat(env, n, { time: now() + 301 })).status, 401);
	assert.equal((await call(env, "/api/net/nodes", { method: "POST", body })).status, 401);

	// no address kept, only the prefix
	const rows = sqlite.prepare("SELECT * FROM net_nodes").all();
	assert.equal(rows.length, 1);
	assert.doesNotMatch(JSON.stringify(sqlite.prepare("SELECT * FROM net_counts").all()), /203\.0\.113\.7/);
});

test("at most 20 new nodes per prefix a day; known ones always heartbeat", async () => {
	const { env } = setup();
	const first = node();
	for (let i = 0; i < 20; i++) assert.equal((await heartbeat(env, i === 0 ? first : node())).status, 200);
	assert.equal((await heartbeat(env, node())).status, 429);
	assert.equal((await heartbeat(env, first)).status, 200);
	assert.equal((await heartbeat(env, node(), { ip: "203.0.114.1" })).status, 200);
});

test("the directory: uptime, probation, and nodes seen in the last 72 hours with room", async () => {
	const { env, sqlite } = setup();
	const [fresh, old, gone, full] = [node(), node(), node(), node()];
	for (const n of [fresh, old, gone]) await heartbeat(env, n);
	await heartbeat(env, full, { capacity: 0 });
	const t = now();
	const hour = Math.floor(t / 3600);
	// `old` first seen 20 days ago, seen in 10 hours since besides this one
	sqlite.prepare("UPDATE net_nodes SET first_seen = ? WHERE id = ?").run(t - 20 * 86400, old.id);
	for (let i = 1; i <= 10; i++) sqlite.prepare("INSERT INTO net_node_hours (node, hour) VALUES (?, ?)").run(old.id, hour - i * 5);
	// and an hour from before the 30 days, which doesn't count
	sqlite.prepare("INSERT INTO net_node_hours (node, hour) VALUES (?, ?)").run(old.id, hour - 800);
	sqlite.prepare("UPDATE net_nodes SET last_seen = ? WHERE id = ?").run(t - 72 * 3600 - 5, gone.id);

	const { nodes } = (await (await call(env, "/api/net/nodes")).json()) as { nodes: Record<string, unknown>[] };
	const byId = new Map(nodes.map((n) => [n.id, n]));
	assert.deepEqual([...byId.keys()].sort(), [fresh.id, old.id].sort());
	const f = byId.get(fresh.id)!;
	assert.equal(f.uptime, 1);
	assert.equal(f.probation, true);
	assert.equal(f.asn, 0);
	assert.equal(f.prefix, "203.0.113.0/24");
	assert.deepEqual(f.addr, { id: fresh.id, addrs: [{ Ip: "198.51.100.1:4433" }] });
	const o = byId.get(old.id)!;
	const possible = hour - Math.floor((t - 20 * 86400) / 3600) + 1;
	assert.equal(o.uptime, 11 / possible);
	assert.equal(o.probation, false);
	// a node up longer than 30 days is counted over 720 hours
	sqlite.prepare("UPDATE net_nodes SET first_seen = ? WHERE id = ?").run(t - 90 * 86400, old.id);
	const again = (await (await call(env, "/api/net/nodes")).json()) as { nodes: { id: string; uptime: number }[] };
	assert.equal(again.nodes.find((n) => n.id === old.id)!.uptime, 11 / 720);
});

test("the index is scoped: another scope sees nothing, and rows hold only the scope's hash", async () => {
	const { env, sqlite } = setup();
	const scope = "0123456789abcdef0123456789abcdef";
	const other = "fedcba9876543210fedcba9876543210";
	const id = "a".repeat(32);
	const h = { "x-scope": scope };
	assert.equal((await call(env, `/api/net/index/${id}`, { headers: h })).status, 404);
	assert.equal((await call(env, `/api/net/index/${id}`, { method: "PUT", headers: h, body: { sealed: "c2VhbGVk" } })).status, 204);
	assert.deepEqual(await (await call(env, `/api/net/index/${id}`, { headers: h })).json(), { sealed: "c2VhbGVk" });
	assert.equal((await call(env, `/api/net/index/${id}`, { method: "PUT", headers: h, body: { sealed: "bmV3" } })).status, 204);
	assert.deepEqual(await (await call(env, "/api/net/index", { headers: h })).json(), { entries: [{ id, sealed: "bmV3" }] });

	assert.deepEqual(await (await call(env, "/api/net/index", { headers: { "x-scope": other } })).json(), { entries: [] });
	assert.equal((await call(env, `/api/net/index/${id}`, { headers: { "x-scope": other } })).status, 404);
	assert.equal((await call(env, `/api/net/index/${id}`, { method: "DELETE", headers: { "x-scope": other } })).status, 204);

	const rows = sqlite.prepare("SELECT scope FROM net_index").all();
	assert.deepEqual(rows.map((r) => r.scope), [sha256(scope)]);
	assert.doesNotMatch(JSON.stringify(sqlite.prepare("SELECT * FROM net_index").all()), new RegExp(scope));

	assert.equal((await call(env, `/api/net/index/${id}`, { method: "DELETE", headers: h })).status, 204);
	assert.equal((await call(env, `/api/net/index/${id}`, { method: "DELETE", headers: h })).status, 204);
	assert.deepEqual(await (await call(env, "/api/net/index", { headers: h })).json(), { entries: [] });

	// what's refused
	assert.equal((await call(env, "/api/net/index")).status, 400);
	assert.equal((await call(env, "/api/net/index/xyz", { headers: h })).status, 400);
	assert.equal((await call(env, `/api/net/index/${id}`, { method: "PUT", headers: h, body: { sealed: "not base64!" } })).status, 400);
	const big = Buffer.alloc(64 * 1024 + 1).toString("base64");
	assert.equal((await call(env, `/api/net/index/${id}`, { method: "PUT", headers: h, body: { sealed: big } })).status, 413);
	const max = Buffer.alloc(64 * 1024).toString("base64");
	assert.equal((await call(env, `/api/net/index/${id}`, { method: "PUT", headers: h, body: { sealed: max } })).status, 204);
});

test("coordination: create-only, capabilities, the marker", async () => {
	const { env, sqlite } = setup();
	const acct = "b".repeat(32);
	const base = `/api/net/coord/${acct}`;
	const cap = { "x-cap": "secret-one" };
	const marker = async () => ((await (await call(env, `${base}/marker`)).json()) as { version: number }).version;

	assert.equal((await call(env, `${base}/marker`)).status, 404);
	assert.equal((await call(env, base, { method: "PUT" })).status, 401);
	assert.equal((await call(env, base, { method: "PUT", headers: cap })).status, 201);
	assert.equal((await call(env, base, { method: "PUT", headers: { "x-cap": "another" } })).status, 409);
	assert.deepEqual(sqlite.prepare("SELECT cap FROM net_accounts").all().map((r) => r.cap), [sha256("secret-one")]);
	let v = await marker();

	// logs: create-only positions, read after n
	assert.equal((await call(env, `${base}/heads/0`, { method: "PUT", body: { body: "h0" } })).status, 401);
	assert.equal((await call(env, `${base}/heads/0`, { method: "PUT", headers: { "x-cap": "wrong" }, body: { body: "h0" } })).status, 403);
	assert.equal(await marker(), v);
	assert.equal((await call(env, `${base}/heads/0`, { method: "PUT", headers: cap, body: { body: "h0" } })).status, 201);
	assert.ok((await marker()) > v);
	v = await marker();
	assert.equal((await call(env, `${base}/heads/0`, { method: "PUT", headers: cap, body: { body: "other" } })).status, 409);
	assert.equal(await marker(), v);
	assert.equal((await call(env, `${base}/heads/1`, { method: "PUT", headers: cap, body: { body: "h1" } })).status, 201);
	assert.equal((await call(env, `${base}/devices/0`, { method: "PUT", headers: cap, body: { body: "d0" } })).status, 201);
	assert.deepEqual(await (await call(env, `${base}/heads`)).json(), {
		records: [
			{ seq: 0, body: "h0" },
			{ seq: 1, body: "h1" },
		],
	});
	assert.deepEqual(await (await call(env, `${base}/heads?after=0`)).json(), { records: [{ seq: 1, body: "h1" }] });
	assert.deepEqual(await (await call(env, `${base}/epochs`)).json(), { records: [] });
	assert.equal((await call(env, `${base}/other`)).status, 404);
	assert.equal((await call(env, `${base}/heads/x`, { method: "PUT", headers: cap, body: { body: "" } })).status, 400);

	// the anchor
	assert.equal((await call(env, `${base}/anchor`)).status, 404);
	assert.equal((await call(env, `${base}/anchor`, { method: "PUT", headers: cap, body: { body: "a" } })).status, 201);
	assert.equal((await call(env, `${base}/anchor`, { method: "PUT", headers: cap, body: { body: "b" } })).status, 409);
	assert.deepEqual(await (await call(env, `${base}/anchor`)).json(), { body: "a" });

	// grants, by name prefix
	v = await marker();
	for (const name of ["dev-1", "dev-2", "other"]) {
		assert.equal((await call(env, `${base}/grants/${name}`, { method: "PUT", headers: cap, body: { body: name } })).status, 204);
	}
	assert.ok((await marker()) > v);
	assert.equal((await call(env, `${base}/grants/x`, { method: "PUT", body: { body: "x" } })).status, 401);
	assert.deepEqual(await (await call(env, `${base}/grants?name=dev-`)).json(), {
		records: [
			{ name: "dev-1", body: "dev-1" },
			{ name: "dev-2", body: "dev-2" },
		],
	});
	assert.equal(((await (await call(env, `${base}/grants`)).json()) as { records: unknown[] }).records.length, 3);
	assert.equal((await call(env, `${base}/grants/bad.name`, { method: "PUT", headers: cap, body: { body: "x" } })).status, 400);

	// the account's computers' node ids: written with the capability only
	assert.equal((await call(env, `${base}/nodes/dev-1`, { method: "PUT", body: { body: "n1" } })).status, 401);
	assert.equal((await call(env, `${base}/nodes/dev-1`, { method: "PUT", headers: cap, body: { body: "n1" } })).status, 204);
	assert.deepEqual(await (await call(env, `${base}/nodes`)).json(), { records: [{ name: "dev-1", body: "n1" }] });
	assert.equal((await call(env, `${base}/nodes/dev-1`, { method: "DELETE", headers: cap })).status, 204);
	assert.deepEqual(await (await call(env, `${base}/nodes`)).json(), { records: [] });

	// a new capability: the old one stops working
	assert.equal((await call(env, `${base}/cap`, { method: "PUT", headers: { "x-cap": "wrong" }, body: { cap: "secret-two" } })).status, 403);
	v = await marker();
	assert.equal((await call(env, `${base}/cap`, { method: "PUT", headers: cap, body: { cap: "secret-two" } })).status, 204);
	assert.ok((await marker()) > v);
	assert.equal((await call(env, `${base}/heads/2`, { method: "PUT", headers: cap, body: { body: "h2" } })).status, 403);
	assert.equal((await call(env, `${base}/heads/2`, { method: "PUT", headers: { "x-cap": "secret-two" }, body: { body: "h2" } })).status, 201);

	// bodies past 256 KiB
	const big = "x".repeat(256 * 1024 + 1);
	assert.equal((await call(env, `${base}/heads/3`, { method: "PUT", headers: { "x-cap": "secret-two" }, body: { body: big } })).status, 413);

	// another account's reads see nothing of this one
	assert.equal((await call(env, `/api/net/coord/${"c".repeat(32)}/heads`)).status, 404);
	assert.equal((await call(env, "/api/net/coord/nothex/heads")).status, 400);
});

test("join requests: open to anyone, limited per prefix, removed with the capability", async () => {
	const { env } = setup();
	const acct = "d".repeat(32);
	const base = `/api/net/coord/${acct}`;
	const cap = { "x-cap": "cap" };
	assert.equal((await call(env, `${base}/requests/r0`, { method: "PUT", body: { body: "x" } })).status, 404);
	assert.equal((await call(env, base, { method: "PUT", headers: cap })).status, 201);
	const v = ((await (await call(env, `${base}/marker`)).json()) as { version: number }).version;
	for (let i = 0; i < 10; i++) {
		assert.equal((await call(env, `${base}/requests/r${i}`, { method: "PUT", body: { body: `join ${i}` } })).status, 204);
	}
	assert.ok(((await (await call(env, `${base}/marker`)).json()) as { version: number }).version > v);
	assert.equal((await call(env, `${base}/requests/r10`, { method: "PUT", body: { body: "x" } })).status, 429);
	assert.equal((await call(env, `${base}/requests/r10`, { method: "PUT", body: { body: "x" }, ip: "2001:db8::1" })).status, 204);
	const { records } = (await (await call(env, `${base}/requests`)).json()) as { records: { name: string; body: string }[] };
	assert.equal(records.length, 11);
	assert.deepEqual(records[0], { name: "r0", body: "join 0" });

	assert.equal((await call(env, `${base}/requests/r0`, { method: "DELETE" })).status, 401);
	assert.equal((await call(env, `${base}/requests/r0`, { method: "DELETE", headers: cap })).status, 204);
	assert.equal(((await (await call(env, `${base}/requests`)).json()) as { records: unknown[] }).records.length, 10);
});

// RFC 9474's client side, enough to get a token signed blind

const big = (b: Uint8Array) => BigInt(`0x${Buffer.from(b).toString("hex") || "0"}`);
const bytesOf = (x: bigint, len: number) => Buffer.from(x.toString(16).padStart(2 * len, "0"), "hex");

function modPow(b: bigint, e: bigint, m: bigint): bigint {
	let r = 1n;
	b %= m;
	for (; e > 0n; e >>= 1n) {
		if (e & 1n) r = (r * b) % m;
		b = (b * b) % m;
	}
	return r;
}

function modInverse(a: bigint, m: bigint): bigint {
	let [r0, r1, s0, s1] = [a % m, m, 1n, 0n];
	while (r1 !== 0n) {
		const q = r0 / r1;
		[r0, r1] = [r1, r0 - q * r1];
		[s0, s1] = [s1, s0 - q * s1];
	}
	assert.equal(r0, 1n);
	return ((s0 % m) + m) % m;
}

const sha384 = (...parts: Uint8Array[]) => {
	const h = createHash("sha384");
	for (const p of parts) h.update(p);
	return h.digest();
};

function mgf1(seed: Uint8Array, len: number): Buffer {
	const out: Buffer[] = [];
	for (let c = 0; out.length * 48 < len; c++) {
		const counter = Buffer.alloc(4);
		counter.writeUInt32BE(c);
		out.push(sha384(seed, counter));
	}
	return Buffer.concat(out).subarray(0, len);
}

/** EMSA-PSS-ENCODE with SHA-384 and a 48-byte salt (RFC 8017 9.1.1). */
function pssEncode(message: Uint8Array, emBits: number): Buffer {
	const emLen = Math.ceil(emBits / 8);
	const salt = randomBytes(48);
	const h = sha384(Buffer.alloc(8), sha384(message), salt);
	const db = Buffer.concat([Buffer.alloc(emLen - 48 - 48 - 2), Buffer.from([1]), salt]);
	const mask = mgf1(h, emLen - 48 - 1);
	for (let i = 0; i < db.length; i++) db[i] ^= mask[i];
	db[0] &= 0xff >> (8 * emLen - emBits);
	return Buffer.concat([db, h, Buffer.from([0xbc])]);
}

const pub = rsa.publicKey.export({ format: "jwk" });
const N = big(Buffer.from(pub.n as string, "base64url"));
const E = big(Buffer.from(pub.e as string, "base64url"));
const K = Buffer.from(pub.n as string, "base64url").length;

/** A token's message blinded: what's sent, and what finishes it. */
function blind() {
	const msg = randomBytes(32);
	const randomizer = randomBytes(32);
	const m = big(pssEncode(Buffer.concat([randomizer, msg]), N.toString(2).length - 1));
	let r: bigint;
	do r = big(randomBytes(K)) % N;
	while (r < 2n);
	const z = (m * modPow(r, E, N)) % N;
	return {
		blind: bytesOf(z, K).toString("hex"),
		finish: (signedHex: string) => {
			const s = (big(Buffer.from(signedHex, "hex")) * modInverse(r, N)) % N;
			const sig = bytesOf(s, K);
			return { msg: msg.toString("hex"), randomizer: randomizer.toString("hex"), sig: sig.toString("hex") };
		},
	};
}

/** A token signed outright with the credit key, as a finished blind one is. */
function directToken() {
	const msg = randomBytes(32);
	const randomizer = randomBytes(32);
	const sig = sign("sha384", Buffer.concat([randomizer, msg]), { key: rsa.privateKey, padding: 6, saltLength: 48 });
	return { msg: msg.toString("hex"), randomizer: randomizer.toString("hex"), sig: sig.toString("hex") };
}

const tokenValid = (t: { msg: string; randomizer: string; sig: string }, key: KeyObject = rsa.publicKey) =>
	verify("sha384", Buffer.concat([Buffer.from(t.randomizer, "hex"), Buffer.from(t.msg, "hex")]), { key, padding: 6, saltLength: 48 }, Buffer.from(t.sig, "hex"));

test("credits: the key, and a blind signature that unblinds to a token", async () => {
	const { env } = setup();
	const { spki } = (await (await call(env, "/api/net/credits/key")).json()) as { spki: string };
	const key = createPublicKey({ key: Buffer.from(spki, "base64"), format: "der", type: "spki" });
	assert.equal(key.export({ format: "jwk" }).n, pub.n);

	const b = blind();
	const res = await call(env, "/api/net/credits/starter", { method: "POST", body: { blind: [b.blind] } });
	assert.equal(res.status, 200);
	const { signed } = (await res.json()) as { signed: string[] };
	assert.equal(signed[0].length, 2 * K);
	assert.equal(modPow(big(Buffer.from(signed[0], "hex")), E, N), big(Buffer.from(b.blind, "hex")));
	const token = b.finish(signed[0]);
	assert.ok(tokenValid(token, key));

	// and the Worker takes it
	const next = blind();
	const ex = await call(env, "/api/net/credits/exchange", { method: "POST", body: { tokens: [token], blind: [next.blind] } });
	assert.equal(ex.status, 200);
	const out = (await ex.json()) as { signed: string[]; spent: number };
	assert.equal(out.spent, 1);
	assert.ok(tokenValid(next.finish(out.signed[0])));
});

test("credits: the starter grant per prefix", async () => {
	const { env, sqlite } = setup("3");
	const ask = (n: number, ip?: string) =>
		call(env, "/api/net/credits/starter", { method: "POST", body: { blind: Array.from({ length: n }, () => blind().blind) }, ip });
	assert.equal((await ask(4)).status, 429);
	assert.equal((await ask(2)).status, 200);
	assert.equal((await ask(2)).status, 429);
	assert.equal((await ask(1)).status, 200);
	assert.equal((await ask(1)).status, 429);
	assert.equal((await ask(3, "198.51.100.1")).status, 200);
	// a month on, the prefix gets it again
	sqlite.prepare("UPDATE net_counts SET until = ? WHERE kind = 'starter'").run(now() - 1);
	assert.equal((await ask(3)).status, 200);

	// blind messages not the modulus' length, or not below it
	const n = bytesOf(N, K).toString("hex");
	for (const bad of ["00", n, "f".repeat(2 * K), "zz"]) {
		assert.equal((await call(env, "/api/net/credits/starter", { method: "POST", body: { blind: [bad] }, ip: "192.0.2.1" })).status, 400);
	}
});

test("credits: exchange spends good tokens once, refuses forged ones, and spends nothing when refused", async () => {
	const { env, sqlite } = setup();
	const exchange = (tokens: unknown[], n: number) =>
		call(env, "/api/net/credits/exchange", { method: "POST", body: { tokens, blind: Array.from({ length: n }, () => blind().blind) } });
	const [a, b, c] = [directToken(), directToken(), directToken()];

	// more blind messages than tokens: refused, nothing spent
	assert.equal((await exchange([a], 2)).status, 400);
	assert.equal(sqlite.prepare("SELECT COUNT(*) AS n FROM net_spent").get()!.n, 0);
	// the same token twice is one
	assert.equal((await exchange([a, a], 2)).status, 400);
	assert.equal(sqlite.prepare("SELECT COUNT(*) AS n FROM net_spent").get()!.n, 0);

	const res = await exchange([a, b], 2);
	assert.equal(res.status, 200);
	const out = (await res.json()) as { signed: string[]; spent: number };
	assert.equal(out.spent, 2);
	assert.equal(out.signed.length, 2);
	assert.deepEqual(
		(sqlite.prepare("SELECT id FROM net_spent ORDER BY id").all() as { id: string }[]).map((r) => r.id),
		[sha256(Buffer.from(a.msg, "hex")), sha256(Buffer.from(b.msg, "hex"))].sort(),
	);

	// a spent token again: refused, and the unspent one with it stays unspent
	assert.equal((await exchange([a, c], 2)).status, 400);
	assert.equal(sqlite.prepare("SELECT COUNT(*) AS n FROM net_spent").get()!.n, 2);
	assert.equal((await exchange([a], 1)).status, 400);

	// forged: a signature by another key, or over another message
	const other = generateKeyPairSync("rsa", { modulusLength: 2048 });
	const msg = randomBytes(32);
	const randomizer = randomBytes(32);
	const forged = {
		msg: msg.toString("hex"),
		randomizer: randomizer.toString("hex"),
		sig: sign("sha384", Buffer.concat([randomizer, msg]), { key: other.privateKey, padding: 6, saltLength: 48 }).toString("hex"),
	};
	assert.equal((await exchange([forged], 1)).status, 400);
	assert.equal((await exchange([{ ...c, msg: randomBytes(32).toString("hex") }], 1)).status, 400);
	assert.equal((await exchange([{ ...c, randomizer: "00" }], 1)).status, 400);

	// a forged one beside a good one: only the good one counts
	const mixed = await exchange([forged, c], 1);
	assert.equal(mixed.status, 200);
	assert.equal(((await mixed.json()) as { spent: number }).spent, 1);
	assert.equal(sqlite.prepare("SELECT COUNT(*) AS n FROM net_spent").get()!.n, 3);
});

test("unknown paths under /api/net/ answer JSON 404s", async () => {
	const { env } = setup();
	for (const path of ["/api/net/", "/api/net/nope", "/api/net/nodes/x", "/api/net/credits/key/x"]) {
		const res = await call(env, path);
		assert.equal(res.status, 404, path);
		assert.ok(((await res.json()) as { error: string }).error);
	}
	assert.equal((await call(env, "/api/net/credits/key", { method: "POST" })).status, 404);
});
