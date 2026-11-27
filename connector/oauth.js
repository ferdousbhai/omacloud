// Signing an agent in, with nothing stored. claude.ai and others take a
// connector that either has no sign-in or does OAuth (authorization code,
// PKCE, clients that register themselves), so this does OAuth, and keeps no
// database to do it: a client's registration, the code and the tokens are
// each what they say plus this connector's own mark on it (an HMAC under a
// secret only it holds), checked when they come back. What lets someone in
// is the passphrase `omacloud agents connect` showed its owner, typed once
// on the page this serves. Changing the secret (connecting again) ends every
// sign-in at once.

const encode = (bytes) => btoa(String.fromCharCode(...bytes)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
const decode = (words) => Uint8Array.from(atob(words.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0));
const utf8 = new TextEncoder();
const hex = (bytes) => [...new Uint8Array(bytes)].map((b) => b.toString(16).padStart(2, "0")).join("");
const now = () => Math.floor(Date.now() / 1000);

const CODE_VALID = 120;
const ACCESS_VALID = 3600;
const REFRESH_VALID = 90 * 24 * 3600;

async function mark(secret, words) {
	const key = await crypto.subtle.importKey("raw", utf8.encode(secret), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
	return encode(new Uint8Array(await crypto.subtle.sign("HMAC", key, utf8.encode(words))));
}

// Compared without stopping at the first difference.
function same(a, b) {
	if (a.length !== b.length) return false;
	let differ = 0;
	for (let i = 0; i < a.length; i++) differ |= a.charCodeAt(i) ^ b.charCodeAt(i);
	return differ === 0;
}

// `said` (an object) with this connector's mark: opaque to whoever carries it.
export async function seal(secret, said) {
	const body = encode(utf8.encode(JSON.stringify(said)));
	return `${body}.${await mark(secret, body)}`;
}

// What a sealed thing says, if the mark is this connector's, it's of `kind`
// and it hasn't run out; else `null`.
export async function unseal(secret, sealed, kind) {
	const [body, given] = String(sealed ?? "").split(".");
	if (!body || !given || !same(given, await mark(secret, body))) return null;
	try {
		const said = JSON.parse(new TextDecoder().decode(decode(body)));
		return said.t === kind && (!said.exp || said.exp > now()) ? said : null;
	} catch {
		return null;
	}
}

// Where an agent may be sent back to: its own https address, or this
// computer (Claude Code and the like listen on one for a moment).
function returnable(uri) {
	try {
		const url = new URL(uri);
		return url.protocol === "https:" || (url.protocol === "http:" && ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname));
	} catch {
		return false;
	}
}
// A loopback address is the same whatever port it's on (RFC 8252).
function sameReturn(registered, given) {
	try {
		const [a, b] = [new URL(registered), new URL(given)];
		if (a.protocol === "http:" && b.protocol === "http:" && a.hostname === b.hostname && a.pathname === b.pathname) return true;
		return registered === given;
	} catch {
		return false;
	}
}

const json = (body, status = 200, headers = {}) =>
	new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json", "cache-control": "no-store", ...headers } });
const refuse = (code, description, status = 400) => json({ error: code, error_description: description }, status);

const escape = (words) => String(words).replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

// The page that asks for the passphrase.
function page({ client, back, folders, write, fields, wrong }) {
	const hidden = Object.entries(fields)
		.map(([name, value]) => `<input type="hidden" name="${escape(name)}" value="${escape(value)}">`)
		.join("");
	return new Response(
		`<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Omacloud</title>
<style>body{font:16px/1.5 system-ui,sans-serif;max-width:28rem;margin:10vh auto;padding:0 1rem}input[type=password]{font:inherit;width:100%;padding:.5rem;box-sizing:border-box}button{font:inherit;padding:.5rem 1rem;margin-top:1rem}.wrong{color:#b00020}small{color:#666}</style>
<h1>Omacloud</h1>
<p><strong>${escape(client)}</strong> is asking to ${write ? "read and write" : "read"} these folders of your files:</p>
<p>${folders.length ? folders.map(escape).join("<br>") : "none"}</p>
<p><small>If you allow it, you go back to ${escape(back)}. What it reads there is plain text to whoever runs it.</small></p>
<form method="post" action="/authorize">${hidden}
<label>The passphrase <code>omacloud agents connect</code> showed you
<input type="password" name="passphrase" autocomplete="off" autofocus required></label>
${wrong ? '<p class="wrong">That isn\'t the passphrase.</p>' : ""}
<button type="submit">Allow</button>
</form></html>`,
		{
			status: wrong ? 403 : 200,
			headers: {
				"content-type": "text/html; charset=utf-8",
				"cache-control": "no-store",
				"content-security-policy": "default-src 'none'; style-src 'unsafe-inline'; form-action 'self' https: http://localhost:* http://127.0.0.1:*; frame-ancestors 'none'; base-uri 'none'",
				"referrer-policy": "no-referrer",
				"x-frame-options": "DENY",
			},
		},
	);
}

// The sign-in's addresses; `null` if the request isn't for one of them.
// `config` holds `secret`, `passphrase_hash` (hex of its SHA-256), `folders`
// and `write`.
export async function signin(request, config) {
	const url = new URL(request.url);
	const origin = url.origin;
	if (url.pathname.startsWith("/.well-known/oauth-protected-resource")) {
		return json({ resource: `${origin}/mcp`, authorization_servers: [origin], bearer_methods_supported: ["header"] });
	}
	if (url.pathname === "/.well-known/oauth-authorization-server" || url.pathname === "/.well-known/openid-configuration") {
		return json({
			issuer: origin,
			authorization_endpoint: `${origin}/authorize`,
			token_endpoint: `${origin}/token`,
			registration_endpoint: `${origin}/register`,
			response_types_supported: ["code"],
			grant_types_supported: ["authorization_code", "refresh_token"],
			code_challenge_methods_supported: ["S256"],
			token_endpoint_auth_methods_supported: ["none"],
		});
	}
	if (url.pathname === "/register" && request.method === "POST") {
		let asked;
		try {
			asked = await request.json();
		} catch {
			return refuse("invalid_client_metadata", "not JSON");
		}
		const uris = Array.isArray(asked?.redirect_uris) ? asked.redirect_uris.filter((u) => typeof u === "string") : [];
		if (!uris.length || uris.length > 8 || !uris.every(returnable)) return refuse("invalid_redirect_uri", "an https address, or this computer's");
		const name = typeof asked.client_name === "string" ? asked.client_name.slice(0, 80) : "An agent";
		const client = await seal(config.secret, { t: "client", r: uris, n: name });
		return json(
			{ client_id: client, client_name: name, redirect_uris: uris, token_endpoint_auth_method: "none", grant_types: ["authorization_code", "refresh_token"], response_types: ["code"] },
			201,
		);
	}
	if (url.pathname === "/authorize" && (request.method === "GET" || request.method === "POST")) {
		const asked = request.method === "GET" ? url.searchParams : new URLSearchParams(await request.text());
		const fields = Object.fromEntries(["response_type", "client_id", "redirect_uri", "code_challenge", "code_challenge_method", "state", "scope", "resource"].map((k) => [k, asked.get(k) ?? ""]));
		const client = await unseal(config.secret, fields.client_id, "client");
		// said here, never sent back to an address nobody registered
		if (!client) return new Response("This agent isn't one that registered with this connector. Add the connector again.", { status: 400 });
		if (!client.r.some((r) => sameReturn(r, fields.redirect_uri))) return new Response("This agent asked to go back somewhere it didn't register.", { status: 400 });
		const back = new URL(fields.redirect_uri);
		const send = (params) => {
			for (const [k, v] of Object.entries(params)) if (v) back.searchParams.set(k, v);
			return new Response(null, { status: 302, headers: { location: back.href, "cache-control": "no-store" } });
		};
		if (fields.response_type !== "code" || fields.code_challenge_method !== "S256" || !fields.code_challenge) {
			return send({ error: "invalid_request", error_description: "an authorization code with PKCE (S256)", state: fields.state });
		}
		const shown = { client: client.n, back: back.host, folders: config.folders ?? [], write: !!config.write, fields };
		if (request.method === "GET") return page(shown);
		const given = hex(await crypto.subtle.digest("SHA-256", utf8.encode((asked.get("passphrase") ?? "").replace(/[\s-]/g, "").toLowerCase())));
		if (!same(given, config.passphrase_hash)) return page({ ...shown, wrong: true });
		const code = await seal(config.secret, { t: "code", c: fields.client_id, r: fields.redirect_uri, ch: fields.code_challenge, exp: now() + CODE_VALID });
		return send({ code, state: fields.state });
	}
	if (url.pathname === "/token" && request.method === "POST") {
		const asked = new URLSearchParams(await request.text());
		const tokens = async () =>
			json({
				access_token: await seal(config.secret, { t: "access", exp: now() + ACCESS_VALID }),
				token_type: "Bearer",
				expires_in: ACCESS_VALID,
				refresh_token: await seal(config.secret, { t: "refresh", exp: now() + REFRESH_VALID }),
			});
		if (asked.get("grant_type") === "authorization_code") {
			const code = await unseal(config.secret, asked.get("code"), "code");
			if (!code || code.c !== asked.get("client_id") || code.r !== asked.get("redirect_uri")) return refuse("invalid_grant", "the code is wrong, used elsewhere or run out");
			const proof = encode(new Uint8Array(await crypto.subtle.digest("SHA-256", utf8.encode(asked.get("code_verifier") ?? ""))));
			if (!same(proof, code.ch)) return refuse("invalid_grant", "the code's proof doesn't match");
			return tokens();
		}
		if (asked.get("grant_type") === "refresh_token") {
			if (!(await unseal(config.secret, asked.get("refresh_token"), "refresh"))) return refuse("invalid_grant", "sign in again");
			return tokens();
		}
		return refuse("unsupported_grant_type", "authorization_code or refresh_token");
	}
	return null;
}

// Whether a request to the tools carries a token this connector gave.
export async function allowed(request, config) {
	const given = request.headers.get("authorization")?.match(/^Bearer\s+(.+)$/i)?.[1];
	return !!given && !!(await unseal(config.secret, given, "access"));
}

// What a request without one is told: where to sign in.
export function unauthorized(request) {
	const origin = new URL(request.url).origin;
	return new Response(JSON.stringify({ error: "unauthorized" }), {
		status: 401,
		headers: {
			"content-type": "application/json",
			"www-authenticate": `Bearer resource_metadata="${origin}/.well-known/oauth-protected-resource"`,
		},
	});
}
