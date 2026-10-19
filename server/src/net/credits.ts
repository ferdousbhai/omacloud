// Storage credits: RFC 9474 blind RSA signatures (RSABSSA-SHA384-PSS-
// Randomized). The Worker signs messages it can't see, so a token it later
// takes back can't be tied to the request that got it signed.

import { requestPrefix } from "./auth.ts";
import { take } from "./counts.ts";
import { HttpError, fromHex, json, readObject, sha256Hex, toHex } from "./http.ts";

const MONTH = 30 * 24 * 3600;
/** Blind messages or tokens in one request. */
const MAX_BATCH = 100;

type Key = { n: bigint; e: bigint; p: bigint; q: bigint; dp: bigint; dq: bigint; qi: bigint; bytes: number; verify: CryptoKey; spki: string };

let cached: { jwk: string; key: Promise<Key> } | undefined;

/** The signing key, from the `CREDIT_KEY` secret (a private RSA JWK). */
function creditKey(env: Env): Promise<Key> {
	const jwk = env.CREDIT_KEY;
	if (!jwk) throw new HttpError(503, "credits are not set up");
	if (cached?.jwk !== jwk) cached = { jwk, key: load(jwk) };
	return cached.key;
}

async function load(text: string): Promise<Key> {
	const j = JSON.parse(text) as Record<string, string>;
	if (j.kty !== "RSA") throw new Error("CREDIT_KEY is not an RSA JWK");
	const big = (name: string) => {
		if (typeof j[name] !== "string") throw new Error(`CREDIT_KEY lacks ${name}`);
		return bytesToBig(b64url(j[name]));
	};
	const verify = await crypto.subtle.importKey(
		"jwk",
		{ kty: "RSA", n: j.n, e: j.e, ext: true },
		{ name: "RSA-PSS", hash: "SHA-384" },
		true,
		["verify"],
	);
	const spki = new Uint8Array((await crypto.subtle.exportKey("spki", verify)) as ArrayBuffer);
	const n = big("n");
	return {
		n,
		e: big("e"),
		p: big("p"),
		q: big("q"),
		dp: big("dp"),
		dq: big("dq"),
		qi: big("qi"),
		bytes: (n.toString(16).length + 1) >> 1,
		verify,
		spki: btoa(String.fromCharCode(...spki)),
	};
}

function b64url(s: string): Uint8Array {
	const b = atob(s.replace(/-/g, "+").replace(/_/g, "/") + "=".repeat((4 - (s.length % 4)) % 4));
	return Uint8Array.from(b, (c) => c.charCodeAt(0));
}

const bytesToBig = (b: Uint8Array): bigint => (b.length === 0 ? 0n : BigInt(`0x${toHex(b)}`));

function bigToHex(x: bigint, bytes: number): string {
	return x.toString(16).padStart(2 * bytes, "0");
}

function modPow(base: bigint, exp: bigint, mod: bigint): bigint {
	let result = 1n;
	let b = base % mod;
	let e = exp;
	while (e > 0n) {
		if (e & 1n) result = (result * b) % mod;
		b = (b * b) % mod;
		e >>= 1n;
	}
	return result;
}

/** m^d mod n, by the CRT, and checked before it leaves. */
function sign(key: Key, m: bigint): string {
	const s1 = modPow(m, key.dp, key.p);
	const s2 = modPow(m, key.dq, key.q);
	const h = (key.qi * (((s1 - s2) % key.p) + key.p)) % key.p;
	const s = s2 + h * key.q;
	// a faulty signature could give the key away: never send one
	if (modPow(s, key.e, key.n) !== m) throw new HttpError(500, "signing failed");
	return bigToHex(s, key.bytes);
}

/** The blind messages asked to be signed, each the modulus' length and below it. */
function blinds(key: Key, value: unknown): bigint[] {
	if (!Array.isArray(value) || value.length > MAX_BATCH) throw new HttpError(400, `blind must be a list of at most ${MAX_BATCH}`);
	return value.map((h) => {
		const b = typeof h === "string" ? fromHex(h) : null;
		if (!b || b.length !== key.bytes) throw new HttpError(400, "a blind message is not the modulus' length");
		const m = bytesToBig(b);
		if (m >= key.n) throw new HttpError(400, "a blind message is not below the modulus");
		return m;
	});
}

export async function publicKey(env: Env): Promise<Response> {
	return json({ spki: (await creditKey(env)).spki });
}

/** POST /credits/starter: the free tokens a network gets each month. */
export async function starter(req: Request, env: Env, now: number): Promise<Response> {
	const key = await creditKey(env);
	const ms = blinds(key, (await readObject(req, 128 * 1024)).blind);
	if (ms.length === 0) return json({ signed: [] });
	const limit = Number(env.STARTER_TOKENS ?? 10);
	if (!(await take(env.DB, "starter", requestPrefix(req), ms.length, limit, MONTH, now))) {
		throw new HttpError(429, "this network's starter tokens are used up");
	}
	return json({ signed: ms.map((m) => sign(key, m)) });
}

/** The spent id of a token whose signature holds, or null. */
async function tokenId(key: Key, t: unknown): Promise<string | null> {
	if (typeof t !== "object" || t === null) return null;
	const { msg, randomizer, sig } = t as Record<string, unknown>;
	const m = typeof msg === "string" ? fromHex(msg) : null;
	const r = typeof randomizer === "string" ? fromHex(randomizer) : null;
	const s = typeof sig === "string" ? fromHex(sig) : null;
	if (!m || m.length === 0 || m.length > 1024 || !r || r.length !== 32 || !s || s.length !== key.bytes) return null;
	const signed = new Uint8Array(r.length + m.length);
	signed.set(r);
	signed.set(m, r.length);
	const ok = await crypto.subtle.verify({ name: "RSA-PSS", saltLength: 48 }, key.verify, s, signed);
	return ok ? sha256Hex(m) : null;
}

/** POST /credits/exchange: good unspent tokens for as many fresh signatures. */
export async function exchange(req: Request, env: Env): Promise<Response> {
	const key = await creditKey(env);
	const body = await readObject(req, 512 * 1024);
	const ms = blinds(key, body.blind);
	if (!Array.isArray(body.tokens) || body.tokens.length > MAX_BATCH) throw new HttpError(400, `tokens must be a list of at most ${MAX_BATCH}`);
	// a token given twice is one token
	const ids = [...new Set((await Promise.all(body.tokens.map((t) => tokenId(key, t)))).filter((id) => id !== null))];
	if (ms.length > ids.length) throw new HttpError(400, "more blind messages than good tokens");
	const signed = ms.map((m) => sign(key, m));
	if (ids.length === 0) return json({ signed, spent: 0 });
	const db = env.DB;
	// spent ids are taken create-only, so of two requests with one token
	// only one gets it
	const results = await db.batch(ids.map((id) => db.prepare("INSERT OR IGNORE INTO net_spent (id) VALUES (?)").bind(id)));
	const fresh = ids.filter((_, i) => (results[i].meta.changes ?? 0) > 0);
	if (ms.length > fresh.length) {
		// refused: what this request just marked spent is unspent again
		if (fresh.length) await db.batch(fresh.map((id) => db.prepare("DELETE FROM net_spent WHERE id = ?").bind(id)));
		throw new HttpError(400, "more blind messages than unspent tokens");
	}
	return json({ signed, spent: fresh.length });
}
