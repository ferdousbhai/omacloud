// The waitlist: Omacloud storage is by invitation, and whoever isn't invited
// can wait for one. Signing in from the app puts the Google account's
// address on it at once (signin.ts). The home page's form takes a typed
// address, behind Turnstile and a rate limit, and emails it a link: only the
// button on the link's page puts it on the list, and the answer is the same whatever
// the address, so the form says nothing about who's invited.
//
// The scheduled job sends invitations made with invite.sh, the admin's
// daily count of new waitlist entries, and forgets typed addresses whose
// link expired.

import * as db from "./db.ts";
import * as email from "./email.ts";
import { allowed, clientIp } from "./limit.ts";
import { page } from "./signin.ts";

/** Invitation emails one run sends. */
const INVITES_PER_RUN = 20;
const DAY = 86400;

/** An email address as the waitlist keeps it (trimmed, lower case), or null if it doesn't look like one. */
export function normalEmail(v: unknown): string | null {
	if (typeof v !== "string") return null;
	const e = v.trim().toLowerCase();
	if (e.length > 254) return null;
	const m = e.match(/^([a-z0-9.!#$%&'*+/=?^_`{|}~-]{1,64})@([a-z0-9-]{1,63}(?:\.[a-z0-9-]{1,63})+)$/);
	if (!m) return null;
	if (m[1].startsWith(".") || m[1].endsWith(".") || m[1].includes("..")) return null;
	if (m[2].split(".").some((l) => l.startsWith("-") || l.endsWith("-"))) return null;
	return e;
}

/** Whether Turnstile says a person sent the form, for this site. */
export async function human(env: Env, token: unknown, ip: string): Promise<boolean> {
	if (typeof token !== "string" || token.length === 0 || token.length > 2048) return false;
	try {
		const r = await fetch(env.TURNSTILE_VERIFY_URL, {
			method: "POST",
			headers: { "content-type": "application/json" },
			body: JSON.stringify({
				secret: env.TURNSTILE_SECRET,
				response: token,
				...(ip === "unknown" ? {} : { remoteip: ip }),
			}),
		});
		const out = await r.json<{ success?: boolean; hostname?: string; "error-codes"?: string[] }>();
		if (out.success === true && out.hostname === new URL(env.PUBLIC_URL).hostname) return true;
		console.log(JSON.stringify({ event: "turnstile refused", codes: out["error-codes"], hostname: out.hostname }));
		return false;
	} catch (e) {
		console.error(JSON.stringify({ event: "turnstile failed", error: String(e) }));
		return false;
	}
}

const sent = () =>
	page(
		200,
		"Check your inbox",
		"If that address can join the waitlist, an email is on its way with a link to confirm it. The link works for two days.",
	);

/**
 * POST /waitlist: an address typed on the home page. Without Turnstile's
 * secret and site key it takes nothing (there's no way to run it unchecked):
 * a deploy that forgot them shows no form and answers 503.
 */
export async function request(req: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
	if (!env.TURNSTILE_SITE_KEY || !env.TURNSTILE_SECRET) {
		console.error(JSON.stringify({ event: "waitlist form without Turnstile" }));
		return page(503, "Not taking requests just now", "Sign in from the Omacloud app to join the waitlist.");
	}
	const ip = clientIp(req);
	if (!(await allowed(env.WAITLIST_LIMIT, ip))) return page(429, "Too many requests", "Wait a minute and try again.");
	const length = Number(req.headers.get("content-length") ?? "0");
	if (!(length > 0 && length < 8192)) return page(400, "That didn't work", "Go back and try again.");
	let form: FormData;
	try {
		form = await req.formData();
	} catch {
		return page(400, "That didn't work", "Go back and try again.");
	}
	const address = normalEmail(form.get("email"));
	if (!address) return page(400, "That isn't an email address", "Go back and check it.");
	if (!(await human(env, form.get("cf-turnstile-response"), ip)))
		return page(403, "That didn't work", "Go back, let the page check you're a person, and send it again.");
	const token = await db.waitFromForm(env.DB, address);
	console.log(JSON.stringify({ event: "invitation asked for", emailed: token !== null }));
	// sent after answering, so how long the answer takes says nothing either
	if (token) ctx.waitUntil(confirm(env, address, token));
	return sent();
}

async function confirm(env: Env, address: string, token: string): Promise<void> {
	const link = `${env.PUBLIC_URL}/waitlist/confirm?token=${token}`;
	if (!(await email.send(env, address, "waitlist confirmation", email.confirmation(link))))
		await db.unsent(env.DB, address);
}

const expired = () =>
	page(400, "This link has expired", "A link works once, for two days. Ask again at omacloud.computer.");

/**
 * GET /waitlist/confirm: the link from the confirmation email, which only
 * asks. Mail scanners open links on their own, so it takes the button's
 * POST to confirm.
 */
export function confirmPage(url: URL): Response {
	const token = url.searchParams.get("token") ?? "";
	if (!/^[0-9a-f]{64}$/.test(token)) return expired();
	return new Response(
		`<!doctype html><meta charset=utf-8><meta name=viewport content="width=device-width">` +
			`<meta name=referrer content=no-referrer><title>Omacloud</title>` +
			`<body style="font:16px system-ui;max-width:32em;margin:4em auto;padding:0 1em">` +
			`<h1>Join the Omacloud waitlist</h1><p>Confirm this address, and you'll get an email when there's room.</p>` +
			`<form method=post action="/waitlist/confirm"><input type=hidden name=token value="${token}">` +
			`<button style="font:inherit;padding:.5em 1em">Confirm</button></form>`,
		{ headers: { "content-type": "text/html; charset=utf-8", "cache-control": "no-store" } },
	);
}

/** POST /waitlist/confirm: the button on that page. */
export async function confirmed(req: Request, env: Env): Promise<Response> {
	if (!(await allowed(env.WAITLIST_LIMIT, clientIp(req)))) return page(429, "Too many requests", "Wait a minute and try again.");
	const length = Number(req.headers.get("content-length") ?? "0");
	if (!(length > 0 && length < 1024)) return expired();
	let token: unknown;
	try {
		token = (await req.formData()).get("token");
	} catch {
		return expired();
	}
	if (typeof token === "string" && /^[0-9a-f]{64}$/.test(token) && (await db.confirmWait(env.DB, token))) {
		console.log(JSON.stringify({ event: "waitlist confirmed" }));
		return page(
			200,
			"You're on the waitlist",
			"You'll get an email when there's room. Until then, Omacloud works with a storage bucket of your own.",
		);
	}
	return expired();
}

/** Invitations made since the last run, emailed; marked only once sent, else tried again later. */
export async function notifyInvites(env: Env): Promise<void> {
	for (const i of await db.unnotified(env.DB, INVITES_PER_RUN)) {
		if (i.joined || (await email.send(env, i.email, "invitation", email.invitation(i.email)))) {
			await db.notified(env.DB, i.email);
			continue;
		}
		const tries = await db.notifyLater(env.DB, i.email);
		// the address isn't in the log; `invite.sh list` shows who's still waiting
		if (tries === 5) console.error(JSON.stringify({ event: "an invitation email keeps failing", tries }));
	}
}

/** Once a day, how many joined the waitlist since the last count, if any did. */
export async function digest(env: Env): Promise<void> {
	const now = db.now();
	const last = await db.lastRan(env.DB, "digest");
	if (now - last < DAY) return;
	const n = await db.waitingSince(env.DB, last, now);
	if (n === 0) return;
	if (await email.send(env, env.ADMIN_EMAIL, "digest", email.digest(n, last))) await db.ran(env.DB, "digest", now);
}

/** Typed addresses whose link expired unfollowed. */
export async function purge(env: Env): Promise<void> {
	const n = await db.purgeUnconfirmed(env.DB);
	if (n) console.log(JSON.stringify({ event: "unconfirmed waitlist requests forgotten", count: n }));
}
