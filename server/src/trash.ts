// Deleted files, kept 14 days. Anyone holding one of an account's keys (a
// removed computer before its key is retired, someone in the owner's Google
// account) could otherwise delete or overwrite everything the account
// stores, its history with it. So before the gateway deletes or overwrites
// an object, it copies it, server side, into the bucket's trash:
//
//   .trash/<account>/<when, ms, 13 digits>.<random>/<key>
//
// outside every account's folder (folders are account ids, `u` and hex),
// where no key reaches: a request names its own account's bucket, and its
// keys can't climb out of the folder (gateway.ts validKey). Each account's
// entries list oldest first, so purging old ones reads only what goes, and
// a restore reads only what came after the time it goes back to.
//
// The trash isn't counted in the quota, but is capped at it. Nothing in it
// goes before its 14 days: an account whose trash has reached its quota can
// create objects but not delete or overwrite any until some expires (and
// the admin hears of it), so nobody can churn real deletions out of the
// trash early. D1 keeps each account's total and oldest entry
// (migrations/0004_trash.sql), so the cap costs no listing and the
// scheduled job knows whom to purge.

import * as email from "./email.ts";
import { encodePath, sha256Hex } from "./sigv4.ts";
import { EMPTY_SHA256, escapeXml, unescapeXml, upstream } from "./upstream.ts";

export const TRASH = ".trash/";
/** How long the trash keeps what's in it. */
export const KEEP_MS = 14 * 86400 * 1000;
/** List pages (a thousand entries each) one purge reads, over all accounts. */
const PURGE_PAGES = 50;
/** Trash entries one run of a restore looks at. */
const RESTORE_ENTRIES = 1000;
/** Runs in a row a restore may fail before it's given up on. */
export const RESTORE_TRIES = 5;
/** Account folders (a thousand a page) the daily sweep reads. */
const SWEEP_PAGES = 20;
/** The largest object a copy can make, and so the largest the gateway lets anyone make: 5 GiB. */
export const MAX_OBJECT = 5 * 1024 ** 3;
/** Requests to the bucket at once (Workers keep six connections open). */
const AT_ONCE = 6;

/**
 * Objects that are rewritten or deleted all the time and hold nothing worth
 * restoring, so they skip the trash:
 *
 * - `omacloud-coordination/changed` (crates/omacloud-core/src/bucket.rs):
 *   random bytes rewritten on every coordination write, only so idle
 *   computers notice; its old values mean nothing.
 * - restic's `locks/<id>`, at a repository's root (the account's folder, or
 *   `omacloud-e<n>/` after a rotation): restic refreshes them every few
 *   minutes and deletes them when done. Omacloud's own engine (rustic_core)
 *   takes none, but restic run on the repository (`omacloud export`) does.
 *
 * - `omacloud-coordination/requests/<device>.json`: a computer asking to
 *   join, deleted once it's approved. Kept, a restore would bring back a
 *   request already answered; and refused at the cap, approving a computer
 *   would fail after it had gone through.
 *
 * Everything else is written once (restic's files are named by their
 * contents; coordination appends are create-only) or rarely (a computer's
 * grant or key acknowledgment), so keeping it costs little.
 */
export function exempt(key: string): boolean {
	return (
		/(^|\/)omacloud-coordination\/(changed|requests\/[^/]+\.json)$/.test(key) || /(^|\/)locks\/[0-9a-f]{64}$/.test(key)
	);
}

const stamp = (ms: number) => String(Math.max(0, Math.floor(ms))).padStart(13, "0");

/** Where the copy of an account's `key` made at `ms` goes. */
export function trashKey(account: string, ms: number, key: string): string {
	const nonce = Array.from(crypto.getRandomValues(new Uint8Array(3)), (b) => b.toString(16).padStart(2, "0")).join("");
	return `${TRASH}${account}/${stamp(ms)}.${nonce}/${key}`;
}

/** A trash entry's account, time and key. */
export function parseTrashKey(full: string): { account: string; ms: number; key: string } | null {
	const m = full.match(/^\.trash\/([^/]+)\/(\d{13})\.[0-9a-f]+\/(.+)$/s);
	return m ? { account: m[1], ms: Number(m[2]), key: m[3] } : null;
}

/**
 * `f` over `items`, AT_ONCE at a time. After a failure no more start, but
 * those under way finish before it's thrown, so what they did is known.
 */
async function each<T>(items: T[], f: (t: T) => Promise<void>): Promise<void> {
	let i = 0;
	let failed: { e: unknown } | null = null;
	const worker = async () => {
		while (!failed && i < items.length) {
			try {
				await f(items[i++]);
			} catch (e) {
				failed ??= { e };
			}
		}
	};
	await Promise.all(Array.from({ length: Math.min(AT_ONCE, items.length) }, worker));
	if (failed) throw (failed as { e: unknown }).e;
}

const unquote = (etag: string | null) => (etag ?? "").replace(/^(W\/)?"|"$/g, "");

/** What's at `full` in the bucket behind: its size and ETag, or null if nothing is. */
export async function stat(env: Env, full: string): Promise<{ size: number; etag: string } | null> {
	const r = await upstream(env, "HEAD", encodePath(full), [], [], EMPTY_SHA256, null);
	if (r.status === 404) return null;
	if (!r.ok) throw new Error(`looking at an object: ${r.status}`);
	return { size: Number(r.headers.get("content-length") ?? 0), etag: unquote(r.headers.get("etag")) };
}

/**
 * Copy `from` to `to` inside the bucket behind, server side; false if
 * nothing was at `from` (deleted meanwhile, by a request that kept it
 * itself, or a folder some buckets show as an object), which is nothing to
 * copy.
 */
export async function copy(env: Env, from: string, to: string): Promise<boolean> {
	const source = `/${env.UPSTREAM_BUCKET}/${encodePath(from)}`;
	const r = await upstream(env, "PUT", encodePath(to), [], [["x-amz-copy-source", source]], EMPTY_SHA256, null);
	// a copy can fail after its 200 has gone, saying so in the body
	const text = await r.text();
	if (r.status === 404 && text.includes("NoSuchKey")) return false;
	if (!r.ok || text.includes("<Error>")) throw new Error(`copying an object: ${r.status} ${text.match(/<Code>([^<]*)/)?.[1] ?? ""}`);
	return true;
}

/**
 * Bytes copied into an account's trash in entries made at `ms`, counted once
 * they're there (`added` is when, by D1's clock); the account's trash total
 * after. A counting walk under way counts them too if they're too new for it.
 */
export async function added(db: D1Database, account: string, bytes: number, ms: number): Promise<number> {
	const s = Math.floor(ms / 1000);
	const row = await db
		.prepare(
			"INSERT INTO trash (account, bytes, oldest, added) VALUES (?1, ?2, ?3, unixepoch()) " +
				"ON CONFLICT (account) DO UPDATE SET bytes = bytes + ?2, oldest = coalesce(oldest, ?3), added = unixepoch(), " +
				"walk_added = walk_added + CASE WHEN walk_from IS NOT NULL AND ?3 >= walk_from THEN ?2 ELSE 0 END " +
				"RETURNING bytes",
		)
		.bind(account, bytes, s)
		.first<{ bytes: number }>();
	return row?.bytes ?? bytes;
}

/** The account's trash has reached its cap, its quota. */
export class Full extends Error {}
/** An object too large to copy (none can be made through the gateway now). */
export class TooLarge extends Error {}

/** Whether an account's trash has reached its cap. */
export async function full(db: D1Database, account: string, quota: number): Promise<boolean> {
	const row = await db.prepare("SELECT bytes FROM trash WHERE account = ?").bind(account).first<{ bytes: number }>();
	return (row?.bytes ?? 0) >= quota;
}

/**
 * Copy what's at each of an account's `keys` into its trash, ahead of
 * deleting or overwriting it; throws if one can't be kept (Full if the
 * trash is at its cap). Returns the ETag of what was kept for each key
 * (null: nothing there), and whether the trash is at its cap now.
 */
export async function preserve(
	env: Env,
	account: { id: string; quota: number },
	keys: string[],
	now = Date.now(),
): Promise<{ kept: Map<string, string | null>; full: boolean }> {
	const kept = new Map<string, string | null>();
	const found: { key: string; size: number }[] = [];
	await each([...new Set(keys)], async (key) => {
		const s = await stat(env, `${account.id}/${key}`);
		kept.set(key, s ? s.etag : null);
		if (s) found.push({ key, size: s.size });
	});
	if (found.length === 0) return { kept, full: false };
	if (found.some((f) => f.size > MAX_OBJECT)) throw new TooLarge();
	if (await full(env.DB, account.id, account.quota)) throw new Full();
	let bytes = 0;
	let total = 0;
	try {
		await each(found, async (f) => {
			if (await copy(env, `${account.id}/${f.key}`, trashKey(account.id, now, f.key))) bytes += f.size;
		});
	} finally {
		// the copies made are in the trash even if another failed
		if (bytes) total = await added(env.DB, account.id, bytes, now);
	}
	return { kept, full: total >= account.quota };
}

/** Tell the admin (once a day an account) that its trash reached its cap. */
export async function alertFull(env: Env, account: string, now = Date.now()): Promise<void> {
	const s = Math.floor(now / 1000);
	const r = await env.DB.prepare(
		"UPDATE trash SET alerted = ?2 WHERE account = ?1 AND coalesce(alerted, 0) <= ?2 - 86400",
	)
		.bind(account, s)
		.run();
	if (r.meta.changes !== 1) return;
	console.error(JSON.stringify({ event: "trash full", account }));
	const sent = await email.send(
		env,
		env.ADMIN_EMAIL,
		"trash full",
		email.alert(`Omacloud: account ${account} is deleting a lot`, [
			`Account ${account} has deleted or replaced as much as its quota in the last 14 days, so Omacloud storage keeps it from deleting or replacing more until some of that expires. It can still add files.`,
			`If that may not be its owner (a removed computer, someone in its Google account), close the account and put it back as it was: server/restore.sh ${account} <time before it started>.`,
		]),
	);
	// told next time instead
	if (!sent) await env.DB.prepare("UPDATE trash SET alerted = NULL WHERE account = ?").bind(account).run();
}

interface Entry {
	key: string;
	size: number;
	etag: string;
}

/** A page of the bucket behind under `prefix`. */
async function listPage(
	env: Env,
	prefix: string,
	from: { token?: string | null; after?: string | null; max?: number },
): Promise<{ entries: Entry[]; next: string | null }> {
	const q: [string, string][] = [
		["list-type", "2"],
		["prefix", prefix],
	];
	if (from.token) q.push(["continuation-token", from.token]);
	else if (from.after) q.push(["start-after", from.after]);
	if (from.max) q.push(["max-keys", String(from.max)]);
	const r = await upstream(env, "GET", "", q, [], EMPTY_SHA256, null);
	if (!r.ok) throw new Error(`listing the trash: ${r.status}`);
	const text = await r.text();
	const entries: Entry[] = [];
	for (const [, c] of text.matchAll(/<Contents>([\s\S]*?)<\/Contents>/g)) {
		const key = unescapeXml(c.match(/<Key>([^<]*)<\/Key>/)?.[1] ?? "");
		if (!key) continue;
		const etag = unescapeXml(c.match(/<ETag>([^<]*)<\/ETag>/)?.[1] ?? "") ?? "";
		entries.push({ key, size: Number(c.match(/<Size>(\d+)<\/Size>/)?.[1] ?? 0), etag: unquote(etag) });
	}
	const truncated = /<IsTruncated>true<\/IsTruncated>/.test(text);
	const token = text.match(/<NextContinuationToken>([^<]*)<\/NextContinuationToken>/)?.[1];
	return { entries, next: truncated && token ? unescapeXml(token) : null };
}

/** Delete up to a thousand objects of the bucket behind; the keys that weren't. */
async function deleteAll(env: Env, keys: string[]): Promise<Set<string>> {
	const xml =
		`<?xml version="1.0" encoding="UTF-8"?><Delete xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Quiet>true</Quiet>` +
		keys.map((k) => `<Object><Key>${escapeXml(k)}</Key></Object>`).join("") +
		"</Delete>";
	const body = new TextEncoder().encode(xml);
	const md5 = btoa(String.fromCharCode(...new Uint8Array(await crypto.subtle.digest("MD5", body))));
	const r = await upstream(env, "POST", "", [["delete", ""]], [["content-md5", md5]], await sha256Hex(body), body);
	const text = await r.text();
	if (!r.ok) throw new Error(`deleting from the trash: ${r.status}`);
	const kept = new Set<string>();
	for (const [, e] of text.matchAll(/<Error>([\s\S]*?)<\/Error>/g)) {
		const k = unescapeXml(e.match(/<Key>([^<]*)<\/Key>/)?.[1] ?? "");
		// an error for no one key: none can be known gone
		if (!k) return new Set(keys);
		kept.add(k);
	}
	return kept;
}

/**
 * The scheduled purge: entries older than 14 days go, and nothing younger.
 * An account being restored is left alone until the restore is done or has
 * failed. Usually a purge reads only as far as the first young entry; an
 * account the sweep asked to be counted is walked to the end, over as many
 * runs as it takes, and its total and oldest entry set from the listing.
 */
export async function purge(env: Env, now = Date.now()): Promise<void> {
	let pages = PURGE_PAGES;
	const cutoff = now - KEEP_MS;
	const due = await env.DB.prepare(
		"SELECT account, recount, walk_from, walk_token, walk_sum, walk_oldest FROM trash t " +
			"WHERE (oldest <= ?1 OR recount = 1) " +
			"AND NOT EXISTS (SELECT 1 FROM restores r WHERE r.account = t.account AND r.done IS NULL AND r.failed IS NULL) " +
			"ORDER BY walk_from IS NULL, oldest LIMIT 100",
	)
		.bind(Math.floor(cutoff / 1000))
		.all<{
			account: string;
			recount: number;
			walk_from: number | null;
			walk_token: string | null;
			walk_sum: number;
			walk_oldest: number | null;
		}>();
	for (const t of due.results) {
		if (pages <= 0) return;
		const counting = t.recount === 1;
		// D1's clock, as `added` is
		const started = (await env.DB.prepare("SELECT unixepoch() AS s").first<{ s: number }>())!.s;
		let from = t.walk_from;
		if (counting && from === null) {
			// a new walk counts what was made before `from`, the gateway what's
			// made from then on: none twice (a copy still landing as a small
			// account is walked can go uncounted, until the next day's walk)
			from = started;
			await env.DB.prepare(
				"UPDATE trash SET walk_from = ?2, walk_token = NULL, walk_sum = 0, walk_oldest = NULL, walk_added = 0 WHERE account = ?1",
			)
				.bind(t.account, from)
				.run();
		}
		let token = counting ? t.walk_token : null;
		let sum = counting && token ? t.walk_sum : 0;
		let walkOldest = counting && token ? t.walk_oldest : null;
		let freed = 0;
		let old = 0;
		let oldest: number | null = null;
		let exhausted = false;
		let failed = false;
		try {
			while (pages-- > 0) {
				const page = await listPage(env, `${TRASH}${t.account}/`, { token });
				const doomed: { key: string; size: number }[] = [];
				for (const e of page.entries) {
					const ms = parseTrashKey(e.key)?.ms ?? 0;
					if (ms < cutoff) {
						doomed.push(e);
						continue;
					}
					const s = Math.floor(ms / 1000);
					oldest ??= s;
					walkOldest = walkOldest === null ? s : Math.min(walkOldest, s);
					if (from !== null && s < from) sum += e.size;
				}
				if (doomed.length) {
					const left = await deleteAll(env, doomed.map((d) => d.key));
					// only what's known gone; a later walk counts the rest
					for (const d of doomed)
						if (!left.has(d.key)) {
							freed += d.size;
							old++;
						} else {
							// still there: still counted, and due again
							const s = Math.floor((parseTrashKey(d.key)?.ms ?? 0) / 1000);
							// older than any young entry of this page, which came first
							oldest = oldest === null ? s : Math.min(oldest, s);
							walkOldest = walkOldest === null ? s : Math.min(walkOldest, s);
							if (from !== null && s < from) sum += d.size;
						}
				}
				token = page.next;
				if (!token) {
					exhausted = true;
					break;
				}
				// past the old entries, the rest is read only to count it
				if (oldest !== null && !counting) break;
			}
		} catch (e) {
			// this account waits for the next run; the others go ahead
			failed = true;
			console.error(JSON.stringify({ event: "trash purge failed", account: t.account, error: String(e) }));
		} finally {
			if (failed)
				// only what's known gone comes off; a counting walk starts over
				// (its place in the listing may be what failed)
				await env.DB.prepare(
					"UPDATE trash SET bytes = max(bytes - ?2, 0), " +
						"walk_from = NULL, walk_token = NULL, walk_sum = 0, walk_oldest = NULL, walk_added = 0 WHERE account = ?1",
				)
					.bind(t.account, freed)
					.run();
			else if (counting && exhausted)
				// counted: what the walk saw, and what was added since `from`;
				// none of either, and nothing added since it began, is empty
				await env.DB.prepare(
					"UPDATE trash SET bytes = ?2 + walk_added, " +
						"oldest = CASE WHEN ?3 IS NOT NULL AND walk_added = 0 THEN ?3 " +
						"WHEN ?3 IS NOT NULL THEN min(?3, walk_from) " +
						"WHEN walk_added > 0 OR added >= ?4 THEN walk_from ELSE NULL END, " +
						"recount = 0, walk_from = NULL, walk_token = NULL, walk_sum = 0, walk_oldest = NULL, walk_added = 0 " +
						"WHERE account = ?1",
				)
					.bind(t.account, sum, walkOldest, started)
					.run();
			else if (counting)
				await env.DB.prepare(
					"UPDATE trash SET bytes = max(bytes - ?2, 0), oldest = coalesce(?3, oldest), " +
						"walk_token = ?4, walk_sum = ?5, walk_oldest = ?6 WHERE account = ?1",
				)
					.bind(t.account, freed, oldest, token, sum, walkOldest)
					.run();
			else
				// walked to the end, it's empty if nothing was seen and nothing
				// added since it began; otherwise the total only loses what went
				await env.DB.prepare(
					"UPDATE trash SET bytes = CASE WHEN ?4 = 1 AND ?3 IS NULL AND added < ?5 THEN 0 ELSE max(bytes - ?2, 0) END, " +
						"oldest = CASE WHEN ?3 IS NOT NULL THEN ?3 WHEN ?4 = 1 AND added < ?5 THEN NULL " +
						"WHEN ?4 = 1 THEN min(coalesce(oldest, added), added) ELSE oldest END " +
						"WHERE account = ?1",
				)
					.bind(t.account, freed, oldest, exhausted ? 1 : 0, started)
					.run();
		}
		if (old) console.log(JSON.stringify({ event: "trash purged", account: t.account, objects: old }));
	}
}

/**
 * Once a day, every account folder of the trash is made known to D1 and
 * due a counting walk (purge), so no count drifts for long and trash whose
 * count was lost (a copy made, then D1 failing) is purged too.
 */
export async function sweep(env: Env, now = Date.now()): Promise<void> {
	const s = Math.floor(now / 1000);
	const last = await env.DB.prepare("SELECT ran FROM jobs WHERE name = 'trash sweep'").first<{ ran: number }>();
	if (last && s - last.ran < 86400) return;
	let token: string | null = null;
	const accounts: string[] = [];
	for (let pages = SWEEP_PAGES; pages > 0; pages--) {
		const q: [string, string][] = [
			["list-type", "2"],
			["prefix", TRASH],
			["delimiter", "/"],
		];
		if (token) q.push(["continuation-token", token]);
		const r = await upstream(env, "GET", "", q, [], EMPTY_SHA256, null);
		if (!r.ok) throw new Error(`listing the trash: ${r.status}`);
		const text = await r.text();
		for (const [, p] of text.matchAll(/<Prefix>([^<]*)<\/Prefix>/g)) {
			const m = (unescapeXml(p) ?? "").match(/^\.trash\/([^/]+)\/$/);
			if (m) accounts.push(m[1]);
		}
		const next = /<IsTruncated>true<\/IsTruncated>/.test(text)
			? text.match(/<NextContinuationToken>([^<]*)<\/NextContinuationToken>/)?.[1]
			: undefined;
		if (!next) break;
		token = unescapeXml(next);
	}
	await env.DB.batch([
		env.DB.prepare(
			"INSERT INTO trash (account, oldest, recount) SELECT value, 0, 1 FROM json_each(?1) WHERE true " +
				"ON CONFLICT (account) DO UPDATE SET recount = 1",
		).bind(JSON.stringify(accounts)),
		env.DB.prepare("INSERT INTO jobs (name, ran) VALUES ('trash sweep', ?1) ON CONFLICT (name) DO UPDATE SET ran = ?1").bind(s),
	]);
}

interface Restore {
	id: number;
	account: string;
	at: number;
	requested: number;
	cursor: string | null;
}

/**
 * The oldest restore asked for, a batch further: every object deleted or
 * overwritten since `at` (up to when the restore was asked for) is put back
 * as it was before its first change since `at`, its oldest trash entry from
 * then. What it replaces goes to the trash itself, so a restore can be
 * undone too. Objects made since `at` stay: restic ignores files nothing
 * refers to, and removing coordination records a computer has seen would
 * look to it like history rolled back. A restore that fails RESTORE_TRIES
 * runs in a row is given up on (the admin hears), and the next goes ahead.
 */
export async function restore(env: Env, now = Date.now(), limit = RESTORE_ENTRIES): Promise<void> {
	const r = await env.DB.prepare(
		"SELECT id, account, at, requested, cursor FROM restores WHERE done IS NULL AND failed IS NULL " +
			"ORDER BY requested, id LIMIT 1",
	).first<Restore>();
	if (!r) return;
	try {
		await restoreBatch(env, r, now, limit);
	} catch (e) {
		const row = await env.DB.prepare(
			"UPDATE restores SET attempts = attempts + 1, failed = CASE WHEN attempts + 1 >= ?2 THEN ?3 END " +
				"WHERE id = ?1 RETURNING attempts, failed",
		)
			.bind(r.id, RESTORE_TRIES, Math.floor(now / 1000))
			.first<{ attempts: number; failed: number | null }>();
		const failed = row?.failed != null;
		console.error(
			JSON.stringify({
				event: failed ? "restore failed" : "restore run failed",
				restore: r.id,
				account: r.account,
				attempts: row?.attempts,
				error: String(e),
			}),
		);
		if (failed)
			await email.send(
				env,
				env.ADMIN_EMAIL,
				"restore failed",
				email.alert(`Omacloud: restore ${r.id} failed`, [
					`Restoring account ${r.account} to ${new Date(r.at * 1000).toISOString()} failed ${RESTORE_TRIES} times in a row, the last with: ${String(e)}`,
					"It's given up on, and the account's trash purges again. The worker's logs say more; server/restore.sh status lists restores, and asking again starts it over.",
				]),
			);
	}
}

async function restoreBatch(env: Env, r: Restore, now: number, limit: number): Promise<void> {
	const prefix = `${TRASH}${r.account}/`;
	// from the first entry made at `at` or later
	const after = r.cursor ?? `${prefix}${stamp(r.at * 1000 - 1)}~`;
	const until = (r.requested + 1) * 1000;
	const page = await listPage(env, prefix, { after, max: limit });
	let done = !page.next;
	const firsts = new Map<string, Entry>();
	let cursor = r.cursor;
	for (const e of page.entries) {
		const entry = parseTrashKey(e.key);
		if (!entry || entry.account !== r.account) continue;
		if (entry.ms >= until) {
			done = true;
			break;
		}
		cursor = e.key;
		if (!firsts.has(entry.key)) firsts.set(entry.key, e);
	}
	// a version put back by an earlier batch is the one to keep
	const seen = await env.DB.prepare(
		"SELECT value AS key FROM json_each(?2) WHERE value IN (SELECT key FROM restored WHERE restore = ?1)",
	)
		.bind(r.id, JSON.stringify([...firsts.keys()]))
		.all<{ key: string }>();
	for (const s of seen.results) firsts.delete(s.key);

	// each put back unless it's as it was; what's there now goes to the
	// trash first, unless it's too large to copy: then it stays as it is
	const replaced: { key: string; size: number }[] = [];
	const back: [string, Entry][] = [];
	let skipped = 0;
	await each([...firsts], async ([key, e]) => {
		const current = await stat(env, `${r.account}/${key}`);
		if (current && current.etag === e.etag && current.size === e.size) return;
		if (current && current.size > MAX_OBJECT) {
			skipped++;
			return;
		}
		if (current) replaced.push({ key, size: current.size });
		back.push([key, e]);
	});
	let bytes = 0;
	try {
		await each(replaced, async (f) => {
			if (await copy(env, `${r.account}/${f.key}`, trashKey(r.account, now, f.key))) bytes += f.size;
		});
	} finally {
		if (bytes) await added(env.DB, r.account, bytes, now);
	}
	await each(back, async ([key, e]) => {
		await copy(env, e.key, `${r.account}/${key}`);
	});
	await env.DB.batch([
		env.DB.prepare("INSERT OR IGNORE INTO restored (restore, key) SELECT ?1, value FROM json_each(?2)").bind(
			r.id,
			JSON.stringify([...firsts.keys()]),
		),
		env.DB.prepare(
			"UPDATE restores SET cursor = ?2, restored = restored + ?3, done = ?4, attempts = 0 WHERE id = ?1",
		).bind(r.id, cursor, back.length, done ? Math.floor(now / 1000) : null),
		env.DB.prepare("UPDATE accounts SET dirty = 1 WHERE id = ?").bind(r.account),
		...(done ? [env.DB.prepare("DELETE FROM restored WHERE restore = ?").bind(r.id)] : []),
	]);
	const event = done ? "restore done" : "restore progress";
	console.log(
		JSON.stringify({ event, restore: r.id, account: r.account, at: r.at, restored: back.length, ...(skipped ? { skipped } : {}) }),
	);
}
