// What omacloud.computer does for an account that asks, and the one place
// this site keeps anything. An account here is a key one of its computers
// made (this knows only its hash, `id`).
//
// - For anyone, free: a passkey opens their files in a browser on any device
//   (`/api/passkeys`). This keeps what the browser sealed with the passkey,
//   when its owner chose to leave it here, and hands it back to whoever
//   proves the passkey. It can't open it.
// - For a subscriber (a key Stripe says is paid for): their connector for
//   agents runs in Omacloud's Cloudflare account, not their own
//   (`/api/connectors`). Its settings hold their keys sealed under the
//   connector's passphrase, which never comes here: see connector/oauth.js.
//
// No email, name or cookie: Stripe holds who paid, this holds that an id did.

import { type Cloud, deployConnector, removeConnector } from "./hosting.ts";

export interface Subscriber {
	id: string;
	customer: string;
	subscription: string;
	status: string;
	until: number;
}
export interface Connector {
	subscriber: string;
	name: string;
	script: string;
	url: string;
}
export interface Passkey {
	credential: string;
	subscriber: string;
	public_key: string;
	alg: number;
	sealed: string;
}

// What's kept, however it's kept (D1 when deployed; a map in tests).
export interface Store {
	subscriber(id: string): Promise<Subscriber | null>;
	bySubscription(subscription: string): Promise<Subscriber | null>;
	putSubscriber(s: Subscriber): Promise<void>;
	connectors(id: string): Promise<Connector[]>;
	putConnector(c: Connector): Promise<void>;
	dropConnector(id: string, name: string): Promise<void>;
	passkey(credential: string): Promise<Passkey | null>;
	passkeys(id: string): Promise<Passkey[]>;
	putPasskey(p: Passkey): Promise<void>;
	dropPasskeys(id: string, credential?: string): Promise<void>;
}

export interface Paid {
	store: Store;
	site: string;
	stripeKey: string;
	stripeWebhookSecret: string;
	stripePrice: string;
	// signs passkey challenges
	secret: string;
	cloud: Cloud;
	fetch: typeof fetch;
	now: () => number;
}

const NOT_NEEDED =
	"Already on Cloudflare's Workers Paid plan? You don't need this: `omacloud agents connect` runs your connector in your own account, free. This is for having Omacloud run it for you.";
const MOST_CONNECTORS = 5;
// few: nothing is paid for these, and a person has only so many devices
const MOST_PASSKEYS = 5;
// a Worker's secret is at most 5 KB
const MOST_SETTINGS = 5000;
const MOST_SEALED = 8192;
const CHALLENGE_VALID = 300;

const utf8 = new TextEncoder();
const hex = (bytes: ArrayBuffer | Uint8Array) => [...new Uint8Array(bytes as ArrayBuffer)].map((b) => b.toString(16).padStart(2, "0")).join("");
const encode = (bytes: ArrayBuffer | Uint8Array) =>
	btoa(String.fromCharCode(...new Uint8Array(bytes as ArrayBuffer)))
		.replace(/\+/g, "-")
		.replace(/\//g, "_")
		.replace(/=+$/, "");
const decode = (words: string) => Uint8Array.from(atob(words.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0));
const sha256 = (bytes: Uint8Array) => crypto.subtle.digest("SHA-256", bytes as BufferSource);

const json = (body: unknown, status = 200) =>
	new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json", "cache-control": "no-store" } });
const refuse = (status: number, code: string, message: string) => json({ error: { code, message } }, status);

function same(a: string, b: string): boolean {
	if (a.length !== b.length) return false;
	let differ = 0;
	for (let i = 0; i < a.length; i++) differ |= a.charCodeAt(i) ^ b.charCodeAt(i);
	return differ === 0;
}

async function hmac(secret: string, words: string): Promise<string> {
	const key = await crypto.subtle.importKey("raw", utf8.encode(secret), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
	return hex(await crypto.subtle.sign("HMAC", key, utf8.encode(words)));
}

// Whether a subscriber's standing still gets them the service: Stripe keeps
// trying a failed payment for a while (`past_due`) before it gives up.
export const active = (s: Subscriber | null): s is Subscriber => !!s && ["active", "trialing", "past_due"].includes(s.status);

// The id of the subscriber a request's key names, if it carries one.
async function who(req: Request): Promise<string | null> {
	const key = req.headers.get("authorization")?.match(/^Bearer\s+([0-9a-f]{64})$/)?.[1];
	return key ? hex(await sha256(utf8.encode(key))) : null;
}

async function stripe(p: Paid, path: string, params: Record<string, string>): Promise<Record<string, unknown>> {
	const response = await p.fetch(`https://api.stripe.com/v1/${path}`, {
		method: "POST",
		headers: { authorization: `Bearer ${p.stripeKey}`, "content-type": "application/x-www-form-urlencoded" },
		body: new URLSearchParams(params),
	});
	const said = (await response.json()) as Record<string, unknown>;
	if (!response.ok) throw new Error(`Stripe refused ${path}: ${(said.error as { message?: string })?.message ?? response.status}`);
	return said;
}

// A subscriber who stopped paying keeps nothing running here. (Their
// passkeys stay: those were never what the subscription was for.)
async function end(p: Paid, id: string): Promise<void> {
	for (const c of await p.store.connectors(id)) {
		await removeConnector(p.cloud, p.fetch, c.script);
		await p.store.dropConnector(id, c.name);
	}
}

// What Stripe tells this about a subscription, checked to be Stripe's
// (https://docs.stripe.com/webhooks#verify-manually).
async function webhook(p: Paid, req: Request): Promise<Response> {
	const body = await req.text();
	const header = req.headers.get("stripe-signature") ?? "";
	const at = header.match(/(?:^|,)t=(\d+)/)?.[1] ?? "";
	const given = [...header.matchAll(/(?:^|,)v1=([0-9a-f]+)/g)].map((m) => m[1]);
	const expected = await hmac(p.stripeWebhookSecret, `${at}.${body}`);
	if (!at || Math.abs(p.now() - Number(at)) > 300 || !given.some((g) => same(g, expected))) return refuse(400, "not_stripe", "this isn't signed by Stripe");
	const event = JSON.parse(body) as { type: string; data: { object: Record<string, any> } };
	const o = event.data.object;
	if (event.type === "checkout.session.completed" && /^[0-9a-f]{64}$/.test(o.client_reference_id ?? "") && o.subscription) {
		const was = await p.store.subscriber(o.client_reference_id);
		await p.store.putSubscriber({ id: o.client_reference_id, customer: String(o.customer), subscription: String(o.subscription), status: "active", until: was?.until ?? 0 });
	}
	if (event.type === "customer.subscription.updated" || event.type === "customer.subscription.deleted") {
		const s = (await p.store.bySubscription(String(o.id))) ?? (o.metadata?.subscriber ? await p.store.subscriber(o.metadata.subscriber) : null);
		if (s) {
			const status = event.type.endsWith("deleted") ? "canceled" : String(o.status);
			const until = Number(o.current_period_end ?? o.items?.data?.[0]?.current_period_end ?? s.until);
			const now = { ...s, subscription: String(o.id), status, until };
			await p.store.putSubscriber(now);
			if (!active(now)) await end(p, s.id);
		}
	}
	return json({ received: true });
}

const challengeFor = async (p: Paid, exp: number, nonce: string) => `${exp}.${nonce}.${await hmac(p.secret, `passkey challenge ${exp}.${nonce}`)}`;

// A passkey's proof (WebAuthn's assertion) checked against the key kept at
// its registration: that it answers this site's own fresh challenge, here.
export async function proves(p: Paid, key: Passkey, proof: { authenticator_data: string; client_data: string; signature: string }): Promise<boolean> {
	try {
		const clientBytes = decode(proof.client_data);
		const client = JSON.parse(new TextDecoder().decode(clientBytes)) as { type: string; challenge: string; origin: string };
		const [exp, nonce, mark] = new TextDecoder().decode(decode(client.challenge)).split(".");
		if (client.type !== "webauthn.get" || client.origin !== new URL(p.site).origin) return false;
		if (Number(exp) < p.now() || !same(await challengeFor(p, Number(exp), nonce), `${exp}.${nonce}.${mark}`)) return false;
		const auth = decode(proof.authenticator_data);
		const site = new Uint8Array(await sha256(utf8.encode(new URL(p.site).hostname)));
		// the site it's for, and that someone was there and checked (UP, UV)
		if (auth.length < 37 || !same(hex(auth.subarray(0, 32)), hex(site)) || (auth[32] & 0x05) !== 0x05) return false;
		const signedOver = new Uint8Array([...auth, ...new Uint8Array(await sha256(clientBytes))]);
		let signature = decode(proof.signature);
		const spki = decode(key.public_key);
		if (key.alg === -7) {
			// ECDSA comes as DER (two integers); WebCrypto takes them side by side
			const part = (at: number) => {
				const length = signature[at + 1];
				const digits = signature.subarray(at + 2, at + 2 + length);
				const trimmed = digits.length > 32 ? digits.subarray(digits.length - 32) : digits;
				const padded = new Uint8Array(32);
				padded.set(trimmed, 32 - trimmed.length);
				return { padded, next: at + 2 + length };
			};
			const r = part(signature[1] & 0x80 ? 2 + (signature[1] & 0x7f) : 2);
			const s = part(r.next);
			signature = new Uint8Array([...r.padded, ...s.padded]);
			const k = await crypto.subtle.importKey("spki", spki as BufferSource, { name: "ECDSA", namedCurve: "P-256" }, false, ["verify"]);
			return await crypto.subtle.verify({ name: "ECDSA", hash: "SHA-256" }, k, signature as BufferSource, signedOver as BufferSource);
		}
		if (key.alg === -257) {
			const k = await crypto.subtle.importKey("spki", spki as BufferSource, { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" }, false, ["verify"]);
			return await crypto.subtle.verify("RSASSA-PKCS1-v1_5", k, signature as BufferSource, signedOver as BufferSource);
		}
		return false;
	} catch {
		return false;
	}
}

const b64url = (v: unknown, most: number): v is string => typeof v === "string" && v.length > 0 && v.length <= most && /^[A-Za-z0-9_-]+$/.test(v);

// What the site does for an account, by address; `null` if the request isn't for one of them.
export async function paid(p: Paid, req: Request): Promise<Response | null> {
	const url = new URL(req.url);
	const path = url.pathname;
	const subscribing = path.match(/^\/subscribe\/([0-9a-f]{64})$/);
	if (subscribing && req.method === "GET") {
		const id = subscribing[1];
		if (active(await p.store.subscriber(id))) return Response.redirect(`${p.site}/subscribed`, 303);
		const session = await stripe(p, "checkout/sessions", {
			mode: "subscription",
			"line_items[0][price]": p.stripePrice,
			"line_items[0][quantity]": "1",
			client_reference_id: id,
			"subscription_data[metadata][subscriber]": id,
			success_url: `${p.site}/subscribed`,
			cancel_url: `${p.site}/`,
			// said where the paying is done: nobody should pay who needn't
			"custom_text[submit][message]": NOT_NEEDED,
			// a code of Stripe's making (Product catalog, Coupons) takes the
			// price off, and then no card is asked for
			allow_promotion_codes: "true",
			payment_method_collection: "if_required",
		});
		return Response.redirect(String(session.url), 303);
	}
	if (!path.startsWith("/api/")) return null;
	if (path === "/api/stripe" && req.method === "POST") return webhook(p, req);

	if (path === "/api/passkeys/challenge" && req.method === "POST") {
		const challenge = await challengeFor(p, p.now() + CHALLENGE_VALID, hex(crypto.getRandomValues(new Uint8Array(16))));
		return json({ challenge: encode(utf8.encode(challenge)) });
	}
	if (path === "/api/passkeys/open" && req.method === "POST") {
		const asked = (await req.json().catch(() => ({}))) as Record<string, string>;
		const key = b64url(asked.credential, 1400) ? await p.store.passkey(asked.credential) : null;
		// one answer for every way of not getting in
		const no = () => refuse(403, "not_signed_in", "this passkey doesn't open an account here");
		if (!key || !(await proves(p, key, asked as never))) return no();
		return json({ sealed: key.sealed });
	}

	const id = await who(req);
	if (!id) return refuse(401, "no_key", "this needs the account's key");
	const subscriber = await p.store.subscriber(id);
	if (path === "/api/subscription" && req.method === "GET") {
		return json({
			active: active(subscriber),
			status: subscriber?.status ?? "none",
			until: subscriber?.until ?? 0,
			connectors: (await p.store.connectors(id)).map((c) => ({ name: c.name, url: c.url })),
			passkeys: (await p.store.passkeys(id)).length,
		});
	}
	if (path === "/api/portal" && req.method === "POST") {
		if (!subscriber) return refuse(402, "not_subscribed", "there's no subscription to manage");
		const session = await stripe(p, "billing_portal/sessions", { customer: subscriber.customer, return_url: `${p.site}/` });
		return json({ url: session.url });
	}
	// taking things away needs no subscription: what ended may still be there
	const connector = path.match(/^\/api\/connectors\/([a-z0-9-]{1,40})$/);
	if (connector && req.method === "DELETE") {
		const found = (await p.store.connectors(id)).find((c) => c.name === connector[1]);
		if (found) {
			await removeConnector(p.cloud, p.fetch, found.script);
			await p.store.dropConnector(id, found.name);
		}
		return json({ removed: connector[1] });
	}
	const passkey = path.match(/^\/api\/passkeys(?:\/([A-Za-z0-9_-]{1,1400}))?$/);
	if (passkey && req.method === "DELETE") {
		await p.store.dropPasskeys(id, passkey[1]);
		return json({ removed: true });
	}
	if (passkey && !passkey[1] && req.method === "POST") {
		const asked = (await req.json().catch(() => ({}))) as Record<string, unknown>;
		if (!b64url(asked.credential, 1400) || !b64url(asked.public_key, 1000) || !b64url(asked.sealed, MOST_SEALED) || (asked.alg !== -7 && asked.alg !== -257)) {
			return refuse(400, "bad_passkey", "a passkey's id, its public key and what it sealed");
		}
		const held = await p.store.passkey(asked.credential);
		if (held && held.subscriber !== id) return refuse(409, "taken", "this passkey is another account's");
		if (!held && (await p.store.passkeys(id)).length >= MOST_PASSKEYS) return refuse(409, "too_many", `an account keeps at most ${MOST_PASSKEYS} passkeys here`);
		await p.store.putPasskey({ credential: asked.credential, subscriber: id, public_key: asked.public_key, alg: asked.alg, sealed: asked.sealed });
		return json({ kept: true });
	}

	if (connector && req.method === "PUT") {
		if (!active(subscriber)) return refuse(402, "not_subscribed", "this needs a subscription: `omacloud subscribe`");
		const settings = await req.text();
		let said: Record<string, unknown>;
		try {
			said = JSON.parse(settings);
		} catch {
			return refuse(400, "bad_settings", "a connector's settings are JSON");
		}
		// only settings whose keys are sealed: this never holds one it could open
		if (settings.length > MOST_SETTINGS || typeof said.sealed !== "string" || "storage" in said || "key" in said || "upload" in said) {
			return refuse(400, "bad_settings", "a connector's settings hold its keys sealed, and nothing else of them");
		}
		const known = await p.store.connectors(id);
		if (known.length >= MOST_CONNECTORS && !known.some((c) => c.name === connector[1])) return refuse(409, "too_many", `a subscription runs at most ${MOST_CONNECTORS} connectors`);
		const script = `oc-${id.slice(0, 16)}-${connector[1]}`;
		const address = await deployConnector(p.cloud, p.fetch, script, settings);
		await p.store.putConnector({ subscriber: id, name: connector[1], script, url: address });
		return json({ url: address });
	}
	return refuse(404, "not_found", "nothing is here");
}

// The store in D1. Its tables are made by the first request that needs them.
export function d1Store(db: D1Database): Store {
	let ready: Promise<unknown> | undefined;
	const made = () =>
		(ready ??= db.batch([
			db.prepare("CREATE TABLE IF NOT EXISTS subscribers (id TEXT PRIMARY KEY, customer TEXT NOT NULL, subscription TEXT NOT NULL, status TEXT NOT NULL, until INTEGER NOT NULL)"),
			db.prepare("CREATE INDEX IF NOT EXISTS subscribers_subscription ON subscribers (subscription)"),
			db.prepare("CREATE TABLE IF NOT EXISTS connectors (subscriber TEXT NOT NULL, name TEXT NOT NULL, script TEXT NOT NULL, url TEXT NOT NULL, PRIMARY KEY (subscriber, name))"),
			db.prepare("CREATE TABLE IF NOT EXISTS passkeys (credential TEXT PRIMARY KEY, subscriber TEXT NOT NULL, public_key TEXT NOT NULL, alg INTEGER NOT NULL, sealed TEXT NOT NULL)"),
			db.prepare("CREATE INDEX IF NOT EXISTS passkeys_subscriber ON passkeys (subscriber)"),
		]));
	const one = async <T>(sql: string, ...args: unknown[]) => (await made(), await db.prepare(sql).bind(...args).first<T>());
	const all = async <T>(sql: string, ...args: unknown[]) => (await made(), (await db.prepare(sql).bind(...args).all<T>()).results);
	const run = async (sql: string, ...args: unknown[]) => void (await made(), await db.prepare(sql).bind(...args).run());
	return {
		subscriber: (id) => one<Subscriber>("SELECT * FROM subscribers WHERE id = ?", id),
		bySubscription: (s) => one<Subscriber>("SELECT * FROM subscribers WHERE subscription = ?", s),
		putSubscriber: (s) => run("INSERT OR REPLACE INTO subscribers (id, customer, subscription, status, until) VALUES (?, ?, ?, ?, ?)", s.id, s.customer, s.subscription, s.status, s.until),
		connectors: (id) => all<Connector>("SELECT * FROM connectors WHERE subscriber = ? ORDER BY name", id),
		putConnector: (c) => run("INSERT OR REPLACE INTO connectors (subscriber, name, script, url) VALUES (?, ?, ?, ?)", c.subscriber, c.name, c.script, c.url),
		dropConnector: (id, name) => run("DELETE FROM connectors WHERE subscriber = ? AND name = ?", id, name),
		passkey: (c) => one<Passkey>("SELECT * FROM passkeys WHERE credential = ?", c),
		passkeys: (id) => all<Passkey>("SELECT * FROM passkeys WHERE subscriber = ?", id),
		putPasskey: (k) => run("INSERT OR REPLACE INTO passkeys (credential, subscriber, public_key, alg, sealed) VALUES (?, ?, ?, ?, ?)", k.credential, k.subscriber, k.public_key, k.alg, k.sealed),
		dropPasskeys: (id, credential) =>
			credential ? run("DELETE FROM passkeys WHERE subscriber = ? AND credential = ?", id, credential) : run("DELETE FROM passkeys WHERE subscriber = ?", id),
	};
}
