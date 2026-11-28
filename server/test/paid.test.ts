import assert from "node:assert/strict";
import { createHmac } from "node:crypto";
import { test } from "node:test";

import { type Connector, type Paid, type Passkey, type Store, type Subscriber, paid } from "../src/paid.ts";

// What the site does for accounts (src/paid.ts), with a map for the database and stand-ins for Stripe and Cloudflare.

const site = "https://omacloud.computer";
const sha = async (words: string) => Buffer.from(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(words)));
const key = "a".repeat(64);
const id = (await sha(key)).toString("hex");

function memory(): Store {
	const subscribers = new Map<string, Subscriber>();
	const connectors = new Map<string, Connector>();
	const passkeys = new Map<string, Passkey>();
	return {
		subscriber: async (i) => subscribers.get(i) ?? null,
		bySubscription: async (s) => [...subscribers.values()].find((x) => x.subscription === s) ?? null,
		putSubscriber: async (s) => void subscribers.set(s.id, s),
		connectors: async (i) => [...connectors.values()].filter((c) => c.subscriber === i),
		putConnector: async (c) => void connectors.set(`${c.subscriber}/${c.name}`, c),
		dropConnector: async (i, name) => void connectors.delete(`${i}/${name}`),
		passkey: async (c) => passkeys.get(c) ?? null,
		passkeys: async (i) => [...passkeys.values()].filter((k) => k.subscriber === i),
		putPasskey: async (k) => void passkeys.set(k.credential, k),
		dropPasskeys: async (i, credential) => {
			for (const k of [...passkeys.values()]) if (k.subscriber === i && (!credential || k.credential === credential)) passkeys.delete(k.credential);
		},
	};
}

// What was asked of Stripe and Cloudflare, and plausible answers.
function tier() {
	const asked: { url: string; method: string; body: unknown }[] = [];
	const net = (async (input: RequestInfo | URL, init?: RequestInit) => {
		const url = String(input);
		asked.push({ url, method: init?.method ?? "GET", body: init?.body });
		if (url.endsWith("/checkout/sessions")) return Response.json({ url: "https://checkout.stripe.com/c/pay/one" });
		if (url.endsWith("/billing_portal/sessions")) return Response.json({ url: "https://billing.stripe.com/p/one" });
		if (url.endsWith("/workers/subdomain")) return Response.json({ success: true, result: { subdomain: "omacloud" } });
		if (url.includes("/workers/scripts/")) return Response.json({ success: true, result: {} });
		return new Response("{}", { status: 404 });
	}) as typeof fetch;
	const p: Paid = {
		store: memory(),
		site,
		stripeKey: "sk_test",
		stripeWebhookSecret: "whsec",
		stripePrice: "price_one",
		secret: "challenge secret",
		cloud: { token: "cf", account: "acct", module: async (name) => new TextEncoder().encode(`// ${name}`).buffer as ArrayBuffer },
		fetch: net,
		now: () => 1_800_000_000,
	};
	return { p, asked };
}

const said = async (r: Response) => (await r.json()) as any;
const call = (p: Paid, path: string, init: RequestInit & { key?: string } = {}) =>
	paid(p, new Request(site + path, { ...init, headers: { ...(init.key ? { authorization: `Bearer ${init.key}` } : {}), ...(init.headers as object) } })) as Promise<Response>;

function fromStripe(p: Paid, type: string, object: object, secret = p.stripeWebhookSecret) {
	const body = JSON.stringify({ type, data: { object } });
	const t = p.now();
	const v1 = createHmac("sha256", secret).update(`${t}.${body}`).digest("hex");
	return call(p, "/api/stripe", { method: "POST", body, headers: { "stripe-signature": `t=${t},v1=${v1}` } });
}
const subscribe = (p: Paid) => fromStripe(p, "checkout.session.completed", { client_reference_id: id, customer: "cus_1", subscription: "sub_1" });
const settings = JSON.stringify({ v: 2, sealed: "c2VhbGVk", folders: ["Notes"], write: true, secret: "s", passphrase_hash: "h" });

test("subscribing goes to Stripe's checkout, and only Stripe says who has paid", async () => {
	const { p, asked } = tier();
	assert.equal(await paid(p, new Request(`${site}/privacy`)), null);
	const sent = await call(p, `/subscribe/${id}`);
	assert.equal(sent.status, 303);
	assert.equal(sent.headers.get("location"), "https://checkout.stripe.com/c/pay/one");
	const checkout = asked[0].body as URLSearchParams;
	assert.equal(checkout.get("client_reference_id"), id);
	assert.equal(checkout.get("line_items[0][price]"), "price_one");
	assert.equal(checkout.get("subscription_data[metadata][subscriber]"), id);
	// the payment page itself says who needn't pay
	assert.match(checkout.get("custom_text[submit][message]") ?? "", /Workers Paid plan\? You don't need this/);
	// a promotion code is taken, and with nothing to pay no card is asked for
	assert.equal(checkout.get("allow_promotion_codes"), "true");
	assert.equal(checkout.get("payment_method_collection"), "if_required");
	// an address that isn't one is that, subscribed or not
	assert.equal((await call(p, "/api/nothing", { key })).status, 404);

	assert.deepEqual(await said(await call(p, "/api/subscription", { key })), { active: false, status: "none", until: 0, connectors: [], passkeys: 0 });
	assert.equal((await call(p, "/api/subscription")).status, 401);
	// a notice anyone could send does nothing
	assert.equal((await fromStripe(p, "checkout.session.completed", { client_reference_id: id, customer: "c", subscription: "s" }, "guess")).status, 400);
	assert.equal((await said(await call(p, "/api/subscription", { key }))).active, false);

	assert.equal((await subscribe(p)).status, 200);
	await fromStripe(p, "customer.subscription.updated", { id: "sub_1", status: "active", items: { data: [{ current_period_end: 1_802_000_000 }] } });
	const now = await said(await call(p, "/api/subscription", { key }));
	assert.equal(now.active, true);
	assert.equal(now.until, 1_802_000_000);
	// once paid, the same address goes nowhere near checkout
	assert.equal((await call(p, `/subscribe/${id}`)).headers.get("location"), `${site}/subscribed`);
	assert.equal((await said(await call(p, "/api/portal", { method: "POST", key }))).url, "https://billing.stripe.com/p/one");
});

test("a subscriber's connector runs here, with its keys sealed, until they stop paying", async () => {
	const { p, asked } = tier();
	const put = (body: string, k = key) => call(p, "/api/connectors/agents", { method: "PUT", body, key: k });
	assert.equal((await put(settings)).status, 402);
	await subscribe(p);
	// keys this could read are never taken
	assert.equal((await put(JSON.stringify({ ...JSON.parse(settings), key: { encrypt: "x" } }))).status, 400);
	assert.equal((await put(JSON.stringify({ folders: [] }))).status, 400);
	assert.equal((await put("x".repeat(6000))).status, 400);
	assert.equal(asked.filter((a) => a.url.includes("cloudflare")).length, 0);

	const made = await put(settings);
	const script = `oc-${id.slice(0, 16)}-agents`;
	assert.deepEqual(await made.json(), { url: `https://${script}.omacloud.workers.dev` });
	const upload = asked.find((a) => a.method === "PUT" && a.url.endsWith(`/accounts/acct/workers/scripts/${script}`));
	const form = upload?.body as FormData;
	const metadata = JSON.parse(await (form.get("metadata") as File).text());
	assert.equal(metadata.main_module, "worker.js");
	assert.deepEqual(metadata.bindings, [{ type: "secret_text", name: "OMACLOUD", text: settings }]);
	assert.equal((form.get("omacloud_web.wasm") as File).type, "application/wasm");
	assert.equal((form.get("files.js") as File).type, "application/javascript+module");
	assert.deepEqual((await said(await call(p, "/api/subscription", { key }))).connectors, [{ name: "agents", url: `https://${script}.omacloud.workers.dev` }]);
	// another subscription's key reaches none of it
	assert.deepEqual((await said(await call(p, "/api/subscription", { key: "b".repeat(64) }))).connectors, []);

	await fromStripe(p, "customer.subscription.deleted", { id: "sub_1", status: "canceled" });
	assert.ok(asked.some((a) => a.method === "DELETE" && a.url.endsWith(`/workers/scripts/${script}`)));
	const after = await said(await call(p, "/api/subscription", { key }));
	assert.equal(after.active, false);
	assert.deepEqual(after.connectors, []);
});

// A passkey as a browser makes one, and its proof of a challenge.
async function passkey() {
	const pair = (await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, ["sign", "verify"])) as CryptoKeyPair;
	const b64 = (bytes: ArrayBuffer | Uint8Array) => Buffer.from(bytes as ArrayBuffer).toString("base64url");
	const public_key = b64((await crypto.subtle.exportKey("spki", pair.publicKey)) as ArrayBuffer);
	const der = (raw: Uint8Array) => {
		const int = (n: Uint8Array) => {
			let i = 0;
			while (i < n.length - 1 && n[i] === 0) i++;
			const digits = n[i] & 0x80 ? [0, ...n.subarray(i)] : [...n.subarray(i)];
			return [0x02, digits.length, ...digits];
		};
		const body = [...int(raw.subarray(0, 32)), ...int(raw.subarray(32))];
		return new Uint8Array([0x30, body.length, ...body]);
	};
	const prove = async (challenge: string, origin = site, host = "omacloud.computer") => {
		const client = new TextEncoder().encode(JSON.stringify({ type: "webauthn.get", challenge, origin }));
		const auth = new Uint8Array([...(await sha(host)), 0x05, 0, 0, 0, 1]);
		const over = new Uint8Array([...auth, ...new Uint8Array(await crypto.subtle.digest("SHA-256", client))]);
		const signature = der(new Uint8Array(await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, pair.privateKey, over)));
		return { credential: "cred-one", authenticator_data: b64(auth), client_data: b64(client), signature: b64(signature) };
	};
	return { public_key, prove };
}

test("a passkey gets back what it sealed, and nothing else does; it needs no subscription", async () => {
	const { p } = tier();
	const mine = await passkey();
	const keep = (k = key) =>
		call(p, "/api/passkeys", { method: "POST", key: k, body: JSON.stringify({ credential: "cred-one", public_key: mine.public_key, alg: -7, sealed: "c2VhbGVkLW5vdGU" }) });
	assert.equal((await keep()).status, 200);
	// nobody else takes over a passkey that's kept, and it needs a key to keep one
	assert.equal((await keep("b".repeat(64))).status, 409);
	assert.equal((await call(p, "/api/passkeys", { method: "POST", body: "{}" })).status, 401);

	const challenge = async () => (await said(await call(p, "/api/passkeys/challenge", { method: "POST" }))).challenge as string;
	const open = (proof: object) => call(p, "/api/passkeys/open", { method: "POST", body: JSON.stringify(proof) });
	const opened = await open(await mine.prove(await challenge()));
	assert.deepEqual(await opened.json(), { sealed: "c2VhbGVkLW5vdGU" });

	// a challenge this didn't make, another site's proof, another passkey's, and an old one
	assert.equal((await open(await mine.prove(Buffer.from("1900000000.aa.bb").toString("base64url")))).status, 403);
	assert.equal((await open(await mine.prove(await challenge(), "https://elsewhere.example"))).status, 403);
	assert.equal((await open(await mine.prove(await challenge(), site, "elsewhere.example"))).status, 403);
	assert.equal((await open(await (await passkey()).prove(await challenge()))).status, 403);
	assert.equal((await open({ ...(await mine.prove(await challenge())), credential: "nobody" })).status, 403);
	const old = await challenge();
	const later: Paid = { ...p, now: () => p.now() + 600 };
	assert.equal((await paid(later, new Request(`${site}/api/passkeys/open`, { method: "POST", body: JSON.stringify(await mine.prove(old)) })))?.status, 403);

	// a subscription coming and going leaves it be; its owner forgets it
	await subscribe(p);
	await fromStripe(p, "customer.subscription.deleted", { id: "sub_1", status: "canceled" });
	assert.equal((await open(await mine.prove(await challenge()))).status, 200);
	assert.equal((await call(p, "/api/passkeys/cred-one", { method: "DELETE", key })).status, 200);
	assert.equal((await open(await mine.prove(await challenge()))).status, 403);
	// only so many are kept for one account
	for (const n of [1, 2, 3, 4, 5]) await p.store.putPasskey({ credential: `c${n}`, subscriber: id, public_key: mine.public_key, alg: -7, sealed: "eA" });
	assert.equal((await keep()).status, 409);
});
