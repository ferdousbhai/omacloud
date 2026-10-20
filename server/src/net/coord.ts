// Coordination: each account's logs and records, sealed by its computers.
// Anyone with the account id reads; writes need its capability, except a
// join request, which comes from a computer that has none yet.

import { capHash, requestPrefix } from "./auth.ts";
import { take } from "./counts.ts";
import { HttpError, NAME, created, json, noContent, readObject, sha256Hex } from "./http.ts";

const MAX_BODY = 256 * 1024;
export const LOGS = new Set(["heads", "devices", "epochs"]);
/** Join requests a prefix may leave an hour, over all accounts. */
const REQUESTS_PER_HOUR = 10;

type Account = { cap: string; version: number };

async function account(env: Env, id: string): Promise<Account> {
	const row = await known(env, id);
	if (!row) throw new HttpError(404, "no such account");
	return row;
}

/** The account, if the request carries its capability. */
async function authorized(req: Request, env: Env, id: string): Promise<Account> {
	const cap = await capHash(req);
	const row = await account(env, id);
	if (row.cap !== cap) throw new HttpError(403, "wrong capability");
	return row;
}

const bump = (env: Env, id: string) => env.DB.prepare("UPDATE net_accounts SET version = version + 1 WHERE id = ?").bind(id);

/** The `body` of a write. */
async function body(req: Request): Promise<string> {
	// room for JSON's escapes around the body
	const { body } = await readObject(req, 2 * MAX_BODY + 1024);
	if (typeof body !== "string") throw new HttpError(400, "body must be a string");
	if (new TextEncoder().encode(body).length > MAX_BODY) throw new HttpError(413, "body is over 256 KiB");
	return body;
}

/** PUT /coord/{account}: makes the account, once. */
export async function create(req: Request, env: Env, id: string): Promise<Response> {
	const cap = await capHash(req);
	const r = await env.DB.prepare("INSERT INTO net_accounts (id, cap, version) VALUES (?, ?, 1) ON CONFLICT (id) DO NOTHING")
		.bind(id, cap)
		.run();
	if (!r.meta.changes) throw new HttpError(409, "account exists");
	return created();
}

/** PUT /coord/{account}/cap: a new capability in place of the current one. */
export async function replaceCap(req: Request, env: Env, id: string): Promise<Response> {
	const current = await capHash(req);
	const { cap } = await readObject(req, 4096);
	if (typeof cap !== "string" || cap.length === 0 || cap.length > 512) throw new HttpError(400, "cap must be a string");
	await account(env, id);
	// only if the capability is still the one presented: two rotations
	// at once can't both win
	const r = await env.DB.prepare("UPDATE net_accounts SET cap = ?, version = version + 1 WHERE id = ? AND cap = ?")
		.bind(await sha256Hex(cap), id, current)
		.run();
	if (!r.meta.changes) throw new HttpError(403, "wrong capability");
	return noContent();
}

/** Reads of an account the Worker doesn't have answer as for an empty one,
 * so its computers see a rollback and fill it again. */
async function known(env: Env, id: string): Promise<Account | null> {
	return await env.DB.prepare("SELECT cap, version FROM net_accounts WHERE id = ?").bind(id).first<Account>();
}

export async function marker(env: Env, id: string): Promise<Response> {
	return json({ version: (await known(env, id))?.version ?? 0 });
}

/** Records of a log an answer holds at most; `next` asks for more. */
const LOG_PAGE = 1000;

/** GET /coord/{account}/{log}?after=n */
export async function readLog(env: Env, id: string, log: string, after: string | null): Promise<Response> {
	if (after !== null && !/^-?\d{1,15}$/.test(after)) throw new HttpError(400, "after must be a number");
	const { results } = await env.DB.prepare("SELECT seq, body FROM net_logs WHERE account = ? AND log = ? AND seq > ? ORDER BY seq LIMIT ?")
		.bind(id, log, after === null ? -1 : Number(after), LOG_PAGE)
		.all<{ seq: number; body: string }>();
	const records = results.map((r) => ({ seq: r.seq, body: r.body }));
	return json(records.length === LOG_PAGE ? { records, next: records[records.length - 1].seq } : { records });
}

/** PUT /coord/{account}/{log}/{seq}: create-only. */
export async function appendLog(req: Request, env: Env, id: string, log: string, seq: string): Promise<Response> {
	if (!/^\d{1,15}$/.test(seq)) throw new HttpError(400, "seq must be a number");
	const text = await body(req);
	await authorized(req, env, id);
	const r = await env.DB.prepare("INSERT INTO net_logs (account, log, seq, body) VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING")
		.bind(id, log, Number(seq), text)
		.run();
	if (!r.meta.changes) throw new HttpError(409, "that position is taken");
	// the marker moves after the record is there, so whoever sees it change finds it
	await bump(env, id).run();
	return created();
}

export async function getAnchor(env: Env, id: string): Promise<Response> {
	await account(env, id);
	const row = await env.DB.prepare("SELECT body FROM net_records WHERE account = ? AND kind = 'anchor' AND name = ''")
		.bind(id)
		.first<{ body: string }>();
	if (!row) throw new HttpError(404, "no anchor");
	return json({ body: row.body });
}

export async function putAnchor(req: Request, env: Env, id: string): Promise<Response> {
	const text = await body(req);
	await authorized(req, env, id);
	const r = await env.DB.prepare("INSERT INTO net_records (account, kind, name, body) VALUES (?, 'anchor', '', ?) ON CONFLICT DO NOTHING")
		.bind(id, text)
		.run();
	if (!r.meta.changes) throw new HttpError(409, "the anchor is set");
	await bump(env, id).run();
	return created();
}

/** Named records: grants, join requests, the account's computers' node
 * ids (so holders are chosen elsewhere), and the version each computer
 * last synced (kept when older versions go). */
export type Kind = "grants" | "requests" | "nodes" | "bases";
export const KINDS: ReadonlySet<string> = new Set(["grants", "requests", "nodes", "bases"]);

/** GET named records of a kind, those whose name starts with a prefix. */
export async function listRecords(env: Env, id: string, kind: Kind, prefix = ""): Promise<Response> {
	if (prefix !== "" && !NAME.test(prefix)) throw new HttpError(400, "bad name prefix");
	const { results } = await env.DB.prepare(
		"SELECT name, body FROM net_records WHERE account = ?1 AND kind = ?2 AND substr(name, 1, length(?3)) = ?3 ORDER BY name",
	)
		.bind(id, kind, prefix)
		.all<{ name: string; body: string }>();
	return json({ records: results.map((r) => ({ name: r.name, body: r.body })) });
}

/** PUT a named record: with the capability, or for a join request
 * without it, rate limited. */
export async function putRecord(req: Request, env: Env, id: string, kind: Kind, name: string, now: number): Promise<Response> {
	const text = await body(req);
	if (kind !== "requests") {
		await authorized(req, env, id);
	} else {
		await account(env, id);
		if (!(await take(env.DB, "request", requestPrefix(req), 1, REQUESTS_PER_HOUR, 3600, now))) {
			throw new HttpError(429, "too many join requests from this network");
		}
	}
	await env.DB.batch([
		env.DB.prepare(
			"INSERT INTO net_records (account, kind, name, body) VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO UPDATE SET body = excluded.body",
		).bind(id, kind, name, text),
		bump(env, id),
	]);
	return noContent();
}

/** DELETE a named record, with the capability. */
export async function deleteRecord(req: Request, env: Env, id: string, kind: Kind, name: string): Promise<Response> {
	await authorized(req, env, id);
	await env.DB.batch([
		env.DB.prepare("DELETE FROM net_records WHERE account = ? AND kind = ? AND name = ?").bind(id, kind, name),
		bump(env, id),
	]);
	return noContent();
}
