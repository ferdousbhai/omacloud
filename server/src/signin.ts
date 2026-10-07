// Signing in with Google, from the Omacloud app.
//
// The computer listens on a local port and opens the browser at /signin,
// with a random state and the hash of a secret it keeps (the challenge).
// Google sends the browser back here; the server learns who it is and sends
// the browser on to the computer's port with a one-time code. The computer
// trades that code and its secret for a storage key at /api/credentials.
// Another program that sees the code can't use it without the secret.
//
// The same sign-in fetches the account's trusted contact pad, for a
// computer recovering with a contact's card, or keeps a new one
// (/api/contact): the pad only ever goes to whoever can sign in with the
// account's Google identity, never to a storage key.

import * as db from "./db.ts";
import { allowed, clientIp } from "./limit.ts";
import { encode } from "./sigv4.ts";
import { escapeXml } from "./upstream.ts";

const hexOf = (s: string | null, min: number): s is string =>
	s !== null && s.length >= min && s.length <= 128 && /^[0-9a-fA-F]+$/.test(s);

export function page(status: number, title: string, text: string): Response {
	return new Response(
		`<!doctype html><meta charset=utf-8><meta name=viewport content="width=device-width">` +
			`<title>Omacloud</title><body style="font:16px system-ui;max-width:32em;margin:4em auto;padding:0 1em">` +
			`<h1>${escapeXml(title)}</h1><p>${escapeXml(text)}</p>`,
		{ status, headers: { "content-type": "text/html; charset=utf-8" } },
	);
}

const callback = (env: Env) => `${env.PUBLIC_URL}/auth/google/callback`;

export async function start(req: Request, url: URL, env: Env): Promise<Response> {
	const port = Number(url.searchParams.get("port"));
	const state = url.searchParams.get("state");
	const challenge = url.searchParams.get("challenge");
	if (!Number.isInteger(port) || port < 1024 || port > 65535 || !hexOf(state, 16) || !hexOf(challenge, 64))
		return page(400, "That didn't work", "Start again from the Omacloud app.");
	// told to the computer too, which would otherwise wait for a sign-in that never comes
	if (!(await allowed(env.SIGNIN_LIMIT, clientIp(req))))
		return Response.redirect(`http://127.0.0.1:${port}/callback?state=${state}&error=busy`, 303);
	const id = await db.startSignin(env.DB, port, state, challenge);
	const to =
		`${env.GOOGLE_AUTH_URL}?client_id=${encode(env.GOOGLE_CLIENT_ID)}&redirect_uri=${encode(callback(env))}` +
		`&response_type=code&scope=openid%20email&state=${id}&prompt=select_account`;
	return Response.redirect(to, 303);
}

interface Claims {
	iss: string;
	aud: string;
	sub: string;
	email: string;
	email_verified?: boolean;
}

async function googleIdentity(env: Env, code: string): Promise<Claims> {
	const r = await fetch(env.GOOGLE_TOKEN_URL, {
		method: "POST",
		headers: { "content-type": "application/x-www-form-urlencoded" },
		body: new URLSearchParams({
			code,
			client_id: env.GOOGLE_CLIENT_ID,
			client_secret: env.GOOGLE_SECRET,
			redirect_uri: callback(env),
			grant_type: "authorization_code",
		}),
	});
	if (!r.ok) throw new Error(`token exchange: ${r.status}`);
	const { id_token } = await r.json<{ id_token: string }>();
	// straight from Google over TLS, so its signature needn't be checked
	const payload = id_token.split(".")[1];
	if (!payload) throw new Error("no token payload");
	const json = atob(payload.replace(/-/g, "+").replace(/_/g, "/"));
	const claims = JSON.parse(new TextDecoder().decode(Uint8Array.from(json, (c) => c.charCodeAt(0)))) as Claims;
	if (claims.iss !== "https://accounts.google.com" && claims.iss !== "accounts.google.com")
		throw new Error(`token from ${claims.iss}`);
	if (claims.aud !== env.GOOGLE_CLIENT_ID) throw new Error("token for another client");
	if (!claims.email_verified) throw new Error("email not verified");
	return claims;
}

export async function back(url: URL, env: Env): Promise<Response> {
	const id = url.searchParams.get("state") ?? "";
	const open = await db.signin(env.DB, id);
	if (!open) return page(400, "This sign-in has expired", "Start again from the Omacloud app.");
	const toComputer = (what: string) =>
		Response.redirect(`http://127.0.0.1:${open.port}/callback?state=${open.state}&${what}`, 303);
	const code = url.searchParams.get("code");
	if (!code || url.searchParams.has("error")) return toComputer("error=cancelled");
	let claims: Claims;
	try {
		claims = await googleIdentity(env, code);
	} catch (e) {
		console.error(JSON.stringify({ event: "google sign-in failed", error: String(e) }));
		return toComputer("error=google");
	}
	const quota = Number(env.DEFAULT_QUOTA);
	// a setting, whatever the config says today
	const signups: string = env.SIGNUPS;
	const account = await db.accountForGoogle(env.DB, claims.sub, claims.email, async () =>
		signups === "open" || (await db.invited(env.DB, claims.email)) ? quota : null,
	);
	if (!account) {
		// on the waitlist: Google checked the address. `error=invite` is what
		// computers before 0.0.13 know.
		await db.waitFromGoogle(env.DB, claims.email, claims.sub);
		console.log(JSON.stringify({ event: "sign-in without an invitation: on the waitlist" }));
		return toComputer("error=invite&waitlist=1");
	}
	if (account.disabled) return toComputer("error=closed");
	return toComputer(`code=${await db.grant(env.DB, id, account.id)}`);
}

/** A pad as computers make them: 28 characters of the recovery code's alphabet. */
export const validPad = (p: unknown): p is string =>
	typeof p === "string" && /^[23456789abcdefghjkmnpqrstuvwxyz]{28}$/.test(p);

/** A computer's request after signing in: its JSON, with the one-time code and its secret. */
async function signedIn(req: Request): Promise<(Record<string, unknown> & { code: string; verifier: string }) | Response> {
	const length = Number(req.headers.get("content-length") ?? "0");
	if (!(length > 0 && length < 4096)) return Response.json({ error: "bad request" }, { status: 400 });
	let r: Record<string, unknown>;
	try {
		r = await req.json();
	} catch {
		return Response.json({ error: "bad request" }, { status: 400 });
	}
	if (typeof r !== "object" || r === null || typeof r.code !== "string" || typeof r.verifier !== "string")
		return Response.json({ error: "bad request" }, { status: 400 });
	return r as Record<string, unknown> & { code: string; verifier: string };
}

const tooMany = () =>
	Response.json({ error: "too many sign-ins for this account: wait a minute and sign in again" }, { status: 429 });

/** POST /api/credentials: a storage key for the account that signed in, and its contact pad if `contact` asks. */
export async function credentials(req: Request, env: Env): Promise<Response> {
	const r = await signedIn(req);
	if (r instanceof Response) return r;
	const accountId = await db.redeem(env.DB, r.code, r.verifier);
	if (!accountId) return Response.json({ error: "sign in again" }, { status: 403 });
	if (!(await allowed(env.CREDENTIALS_LIMIT, accountId))) return tooMany();
	const account = await db.account(env.DB, accountId);
	if (!account || account.disabled) return Response.json({ error: "closed" }, { status: 403 });
	// the account's oldest key goes past MAX_KEYS: refusing would lock out
	// whoever lost every computer, and a computer whose key went signs in
	// again
	const key = await db.newKey(env.DB, accountId, db.masterKey(env), true);
	console.log(
		JSON.stringify({ event: "new key", account: accountId, key: key.accessKeyId, evicted: key.evicted }),
	);
	const scheme = env.PUBLIC_URL.startsWith("http://") ? "http" : "https";
	let contact = {};
	if (r.contact === true) {
		contact = { contact_pad: await db.contactPad(env.DB, accountId) };
		console.log(JSON.stringify({ event: "contact pad fetched", account: accountId }));
	}
	return Response.json({
		endpoint: `${scheme}://${env.STORAGE_HOST}`,
		region: env.REGION,
		bucket: accountId,
		access_key_id: key.accessKeyId,
		secret_access_key: key.secret,
		email: account.email,
		...contact,
	});
}

/**
 * POST /api/contact: keep a new pad for the account's trusted contact, or
 * none (`pad: null`). `account` is the account the computer belongs to, so
 * signing in with another Google identity changes nothing.
 */
export async function contact(req: Request, env: Env): Promise<Response> {
	const r = await signedIn(req);
	if (r instanceof Response) return r;
	if (typeof r.account !== "string" || !(r.pad === null || validPad(r.pad)))
		return Response.json({ error: "bad request" }, { status: 400 });
	const accountId = await db.redeem(env.DB, r.code, r.verifier);
	if (!accountId) return Response.json({ error: "sign in again" }, { status: 403 });
	if (!(await allowed(env.CREDENTIALS_LIMIT, accountId))) return tooMany();
	const account = await db.account(env.DB, accountId);
	if (!account || account.disabled) return Response.json({ error: "closed" }, { status: 403 });
	if (accountId !== r.account)
		return Response.json({ error: "another account", email: account.email }, { status: 409 });
	await db.setContactPad(env.DB, accountId, r.pad);
	console.log(JSON.stringify({ event: r.pad ? "contact pad set" : "contact pad removed", account: accountId }));
	return Response.json({ email: account.email });
}
