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

const now = () => Math.floor(Date.now() / 1000);

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

/** A new key for the account. */
export async function newKey(
	db: D1Database,
	accountId: string,
	master: Uint8Array,
): Promise<{ accessKeyId: string; secret: string }> {
	// like AWS's: 20 characters, upper case letters and digits (32 of them,
	// so a random byte maps without bias)
	const chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
	const id = "OC" + Array.from(crypto.getRandomValues(new Uint8Array(18)), (b) => chars[b % 32]).join("");
	await db
		.prepare("INSERT INTO keys (id, account, created) VALUES (?, ?, ?)")
		.bind(id, accountId, now())
		.run();
	return { accessKeyId: id, secret: await secretOf(master, id) };
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

/** Bytes just written: counted now, and counted properly at the next scan. */
export function wrote(db: D1Database, accountId: string, bytes: number): Promise<unknown> {
	return db
		.prepare("UPDATE accounts SET used = used + ?, dirty = 1 WHERE id = ?")
		.bind(bytes, accountId)
		.run();
}

export function deleted(db: D1Database, accountId: string): Promise<unknown> {
	return db.prepare("UPDATE accounts SET dirty = 1 WHERE id = ?").bind(accountId).run();
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
