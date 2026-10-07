// Accounts, their keys and sign-ins, in D1 (schema: migrations/).

import { hex, hmacHex, same, sha256Hex } from "./sigv4.ts";

export interface Account {
	/** `u` and 16 hex digits: the folder in the bucket, and the bucket name the account's computers use. */
	id: string;
	email: string;
	quota: number;
	used: number;
	disabled: number;
}

const ACCOUNT = "SELECT id, email, quota, used, disabled FROM accounts";
/** A sign-in has ten minutes. */
const SIGNIN_SECONDS = 600;
/** Keys an account may hold at once: one per computer, and some to spare. */
export const MAX_KEYS = 20;
/** A waitlist confirmation link works for two days. */
export const CONFIRM_SECONDS = 2 * 86400;
/** A new confirmation email only once the last one's link has expired, so no link is cut short. */
const RESEND_SECONDS = CONFIRM_SECONDS;

export const now = () => Math.floor(Date.now() / 1000);

export function randomHex(bytes: number): string {
	return hex(crypto.getRandomValues(new Uint8Array(bytes)));
}

/** The master key's bytes, from its hex secret. */
export function masterKey(env: Env): Uint8Array {
	const h = env.MASTER_KEY.trim();
	if (!/^([0-9a-f]{2}){32,}$/i.test(h)) throw new Error("MASTER_KEY needs 32 bytes or more of hex");
	return new Uint8Array(h.match(/../g)!.map((b) => parseInt(b, 16)));
}

/** A key's secret: derived from the master key, never stored. */
export function secretOf(master: Uint8Array, accessKeyId: string): Promise<string> {
	return hmacHex(master, `omacloud s3 secret\0${accessKeyId}`);
}

export function account(db: D1Database, id: string): Promise<Account | null> {
	return db.prepare(`${ACCOUNT} WHERE id = ?`).bind(id).first<Account>();
}

/** The account a key that isn't revoked belongs to. */
export function keyAccount(db: D1Database, accessKeyId: string): Promise<Account | null> {
	return db
		.prepare(
			"SELECT a.id, a.email, a.quota, a.used, a.disabled FROM keys k " +
				"JOIN accounts a ON a.id = k.account WHERE k.id = ? AND k.revoked IS NULL",
		)
		.bind(accessKeyId)
		.first<Account>();
}

/** The account of a Google identity; made now if `quota` says how much space it gets. */
export async function accountForGoogle(
	db: D1Database,
	sub: string,
	email: string,
	quota: () => Promise<number | null>,
): Promise<Account | null> {
	const found = await db.prepare(`${ACCOUNT} WHERE google_sub = ?`).bind(sub).first<Account>();
	if (found) {
		if (found.email !== email) {
			await db.prepare("UPDATE accounts SET email = ? WHERE id = ?").bind(email, found.id).run();
		}
		return { ...found, email };
	}
	const q = await quota();
	if (q === null) return null;
	const id = `u${randomHex(8)}`;
	await db
		.prepare("INSERT INTO accounts (id, google_sub, email, created, quota) VALUES (?, ?, ?, ?, ?)")
		.bind(id, sub, email, now(), q)
		.run();
	return account(db, id);
}

export async function invited(db: D1Database, email: string): Promise<boolean> {
	const row = await db
		.prepare("SELECT 1 AS yes FROM invites WHERE email = ?")
		.bind(email.toLowerCase())
		.first();
	return row !== null;
}

/**
 * A new key for the account. With `evict`, the oldest keys go to keep the
 * account at MAX_KEYS: a Google sign-in is the account's owner, who may have
 * lost every computer, so it always gets a key; how many is the limit.
 * Returns how many went.
 */
export async function newKey(
	db: D1Database,
	accountId: string,
	master: Uint8Array,
	evict = false,
): Promise<{ accessKeyId: string; secret: string; evicted: number }> {
	// like AWS's: 20 characters, upper case letters and digits (32 of them,
	// so a random byte maps without bias)
	const chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
	const id = "OC" + Array.from(crypto.getRandomValues(new Uint8Array(18)), (b) => chars[b % 32]).join("");
	const insert = db.prepare("INSERT INTO keys (id, account, created) VALUES (?, ?, ?)").bind(id, accountId, now());
	let evicted = 0;
	if (evict) {
		const [, r] = await db.batch([
			insert,
			db
				.prepare(
					"UPDATE keys SET revoked = ?1 WHERE id IN (SELECT id FROM keys WHERE account = ?2 AND revoked IS NULL " +
						"AND id != ?3 ORDER BY created DESC, rowid DESC LIMIT -1 OFFSET ?4)",
				)
				.bind(now(), accountId, id, MAX_KEYS - 1),
		]);
		evicted = r.meta.changes;
	} else await insert.run();
	return { accessKeyId: id, secret: await secretOf(master, id), evicted };
}

/** How many of an account's keys aren't revoked. */
export async function activeKeys(db: D1Database, accountId: string): Promise<number> {
	const row = await db
		.prepare("SELECT count(*) AS n FROM keys WHERE account = ? AND revoked IS NULL")
		.bind(accountId)
		.first<{ n: number }>();
	return row?.n ?? 0;
}

/** Revoke every key of the account but `keep`; how many went. */
export async function retireOthers(db: D1Database, accountId: string, keep: string): Promise<number> {
	const r = await db
		.prepare("UPDATE keys SET revoked = ? WHERE account = ? AND id != ? AND revoked IS NULL")
		.bind(now(), accountId, keep)
		.run();
	return r.meta.changes;
}

/**
 * Room for `bytes` about to be written, taken from the quota at once (so
 * writes side by side can't both fit in the same space); false if it's
 * full. The next count puts `used` right, as overwrites and failed writes
 * leave it high.
 */
export async function reserve(db: D1Database, accountId: string, bytes: number): Promise<boolean> {
	const r = await db
		.prepare("UPDATE accounts SET used = used + ?2, dirty = 1 WHERE id = ?1 AND used + ?2 <= quota")
		.bind(accountId, bytes)
		.run();
	return r.meta.changes === 1;
}

/** Room reserved for a write that didn't happen. */
export function release(db: D1Database, accountId: string, bytes: number): Promise<unknown> {
	return db
		.prepare("UPDATE accounts SET used = max(used - ?2, 0), dirty = 1 WHERE id = ?1")
		.bind(accountId, bytes)
		.run();
}

/** Written to or deleted from: due a count. */
export function touched(db: D1Database, accountId: string): Promise<unknown> {
	return db.prepare("UPDATE accounts SET dirty = 1 WHERE id = ?").bind(accountId).run();
}

/** The account's trusted contact pad, if it has one. */
export async function contactPad(db: D1Database, accountId: string): Promise<string | null> {
	const row = await db
		.prepare("SELECT contact_pad FROM accounts WHERE id = ?")
		.bind(accountId)
		.first<{ contact_pad: string | null }>();
	return row?.contact_pad ?? null;
}

/** Keep a new pad for the account's trusted contact (an earlier card stops working), or none. */
export function setContactPad(db: D1Database, accountId: string, pad: string | null): Promise<unknown> {
	return db.prepare("UPDATE accounts SET contact_pad = ? WHERE id = ?").bind(pad, accountId).run();
}

/**
 * Waiting for an invitation: a Google identity that signed in without one.
 * Google checked the address, so it's confirmed now, and an address typed
 * on the website for the same email waits no more for its link.
 */
export function waitFromGoogle(db: D1Database, email: string, sub: string): Promise<unknown> {
	return db
		.prepare(
			"INSERT INTO waitlist (email, google_sub, created, last_seen, confirmed) VALUES (?1, ?2, ?3, ?3, ?3) " +
				"ON CONFLICT (email) DO UPDATE SET google_sub = ?2, last_seen = ?3, " +
				"confirmed = coalesce(confirmed, ?3), token_hash = NULL, token_sent = NULL",
		)
		.bind(email.toLowerCase(), sub, now())
		.run();
}

/**
 * An address typed on the website: a token for its confirmation link, or
 * null if no email should go (invited already, has an account, confirmed
 * already, or sent one in the last day). Only the token's hash is kept.
 */
export async function waitFromForm(db: D1Database, email: string): Promise<string | null> {
	const known = await db
		.prepare(
			"SELECT (SELECT 1 FROM invites WHERE email = ?1) AS invited, " +
				"(SELECT 1 FROM accounts WHERE lower(email) = ?1) AS joined",
		)
		.bind(email)
		.first<{ invited: number | null; joined: number | null }>();
	if (known?.invited || known?.joined) return null;
	const token = randomHex(32);
	const t = now();
	const r = await db
		.prepare(
			"INSERT INTO waitlist (email, created, last_seen, token_hash, token_sent) VALUES (?1, ?2, ?2, ?3, ?2) " +
				"ON CONFLICT (email) DO UPDATE SET last_seen = ?2, token_hash = ?3, token_sent = ?2 " +
				"WHERE confirmed IS NULL AND (token_sent IS NULL OR token_sent <= ?4)",
		)
		.bind(email, t, await sha256Hex(token), t - RESEND_SECONDS)
		.run();
	return r.meta.changes === 1 ? token : null;
}

/** A confirmation email that didn't go: the address may ask again now. */
export function unsent(db: D1Database, email: string): Promise<unknown> {
	return db
		.prepare("UPDATE waitlist SET token_sent = NULL WHERE email = ? AND confirmed IS NULL")
		.bind(email)
		.run();
}

/** A confirmation link followed: true if it was good (and is now spent). */
export async function confirmWait(db: D1Database, token: string): Promise<boolean> {
	const r = await db
		.prepare(
			"UPDATE waitlist SET confirmed = ?1, token_hash = NULL, token_sent = NULL " +
				"WHERE token_hash = ?2 AND confirmed IS NULL AND token_sent > ?3",
		)
		.bind(now(), await sha256Hex(token), now() - CONFIRM_SECONDS)
		.run();
	return r.meta.changes === 1;
}

/** Typed addresses never confirmed, once their link has expired. */
export async function purgeUnconfirmed(db: D1Database): Promise<number> {
	const r = await db
		.prepare("DELETE FROM waitlist WHERE confirmed IS NULL AND coalesce(token_sent, created) <= ?")
		.bind(now() - CONFIRM_SECONDS)
		.run();
	return r.meta.changes;
}

/** Invitations whose email hasn't gone yet, oldest first; `joined` if the address has an account already. */
export async function unnotified(db: D1Database, limit: number): Promise<{ email: string; joined: number }[]> {
	const r = await db
		.prepare(
			"SELECT i.email, EXISTS (SELECT 1 FROM accounts a WHERE lower(a.email) = i.email) AS joined " +
				"FROM invites i WHERE i.notified IS NULL AND i.notify_after <= ? ORDER BY i.created, i.email LIMIT ?",
		)
		.bind(now(), limit)
		.all<{ email: string; joined: number }>();
	return r.results;
}

export function notified(db: D1Database, email: string): Promise<unknown> {
	return db.prepare("UPDATE invites SET notified = ? WHERE email = ?").bind(now(), email).run();
}

/** An invitation email that didn't go: try again after 15 minutes, doubling each time, at most a day. Returns the tries so far. */
export async function notifyLater(db: D1Database, email: string): Promise<number> {
	const r = await db
		.prepare(
			"UPDATE invites SET notify_tries = notify_tries + 1, " +
				"notify_after = ?2 + min(86400, 900 * (1 << min(notify_tries, 7))) WHERE email = ?1 RETURNING notify_tries",
		)
		.bind(email, now())
		.first<{ notify_tries: number }>();
	return r?.notify_tries ?? 0;
}

/** When a scheduled job last did its thing (0: never). */
export async function lastRan(db: D1Database, job: string): Promise<number> {
	const row = await db.prepare("SELECT ran FROM jobs WHERE name = ?").bind(job).first<{ ran: number }>();
	return row?.ran ?? 0;
}

export function ran(db: D1Database, job: string, at: number): Promise<unknown> {
	return db
		.prepare("INSERT INTO jobs (name, ran) VALUES (?1, ?2) ON CONFLICT (name) DO UPDATE SET ran = ?2")
		.bind(job, at)
		.run();
}

/** Confirmed on the waitlist after `since` (and up to `until`). */
export async function waitingSince(db: D1Database, since: number, until: number): Promise<number> {
	const row = await db
		.prepare("SELECT count(*) AS n FROM waitlist WHERE confirmed > ? AND confirmed <= ?")
		.bind(since, until)
		.first<{ n: number }>();
	return row?.n ?? 0;
}

/** A sign-in started by a computer listening on `port`; its id goes to Google as the state. */
export async function startSignin(
	db: D1Database,
	port: number,
	state: string,
	challenge: string,
): Promise<string> {
	const id = randomHex(16);
	await db.batch([
		db.prepare("DELETE FROM signins WHERE created < ?").bind(now() - SIGNIN_SECONDS),
		db
			.prepare("INSERT INTO signins (id, port, state, challenge, created) VALUES (?, ?, ?, ?, ?)")
			.bind(id, port, state, challenge, now()),
	]);
	return id;
}

/** The port and the computer's state of an open sign-in. */
export function signin(db: D1Database, id: string): Promise<{ port: number; state: string } | null> {
	return db
		.prepare(
			"SELECT port, state FROM signins WHERE id = ? AND grant_code IS NULL AND created >= ?",
		)
		.bind(id, now() - SIGNIN_SECONDS)
		.first();
}

/** Google said who signed in: a one-time code for the computer. */
export async function grant(db: D1Database, id: string, accountId: string): Promise<string> {
	const code = randomHex(32);
	await db
		.prepare("UPDATE signins SET grant_code = ?, account = ? WHERE id = ?")
		.bind(code, accountId, id)
		.run();
	return code;
}

/**
 * The account a one-time code was granted for, if the verifier matches the
 * sign-in's challenge. The code is spent either way, in one statement, so
 * it can't be redeemed twice.
 */
export async function redeem(db: D1Database, code: string, verifier: string): Promise<string | null> {
	const row = await db
		.prepare(
			"UPDATE signins SET used = 1 WHERE grant_code = ? AND used = 0 AND created >= ? " +
				"RETURNING challenge, account",
		)
		.bind(code, now() - SIGNIN_SECONDS)
		.first<{ challenge: string; account: string }>();
	if (!row) return null;
	return same(await sha256Hex(verifier), row.challenge) ? row.account : null;
}
