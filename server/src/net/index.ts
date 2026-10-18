// A bucket's index: sealed entries the Worker keeps but can't read, under
// the hash of a scope only the account's computers know.

import { HttpError, ID, json, noContent, readObject, sha256Hex } from "./http.ts";

const MAX_SEALED = 64 * 1024;
const BASE64 = /^[A-Za-z0-9+/]*={0,2}$/;

/** sha256 of the request's `x-scope`, or a 400. */
async function scope(req: Request): Promise<string> {
	const s = (req.headers.get("x-scope") ?? "").toLowerCase();
	if (!ID.test(s)) throw new HttpError(400, "x-scope must be 32 hex characters");
	return sha256Hex(s);
}

/** Entries per page: a big bucket's index is read in pages, as one answer
 * would outgrow what D1 returns at once. */
export const PAGE = 1000;

/** A page of entries after `?after=<id>`; `next` names where the next
 * page starts, absent on the last. */
export async function list(req: Request, env: Env): Promise<Response> {
	const after = (new URL(req.url).searchParams.get("after") ?? "").toLowerCase();
	if (after !== "" && !ID.test(after)) throw new HttpError(400, "after must be an entry id");
	const { results } = await env.DB.prepare("SELECT id, sealed FROM net_index WHERE scope = ?1 AND id > ?2 ORDER BY id LIMIT ?3")
		.bind(await scope(req), after, PAGE)
		.all<{ id: string; sealed: string }>();
	const entries = results.map((r) => ({ id: r.id, sealed: r.sealed }));
	return json(entries.length === PAGE ? { entries, next: entries[PAGE - 1].id } : { entries });
}

export async function get(req: Request, env: Env, id: string): Promise<Response> {
	const row = await env.DB.prepare("SELECT sealed FROM net_index WHERE scope = ? AND id = ?")
		.bind(await scope(req), id)
		.first<{ sealed: string }>();
	if (!row) throw new HttpError(404, "no such entry");
	return json({ sealed: row.sealed });
}

export async function put(req: Request, env: Env, id: string): Promise<Response> {
	const s = await scope(req);
	// base64 of 64 KiB, and room for the JSON around it
	const { sealed } = await readObject(req, Math.ceil(MAX_SEALED / 3) * 4 + 1024);
	if (typeof sealed !== "string" || sealed.length % 4 !== 0 || !BASE64.test(sealed)) {
		throw new HttpError(400, "sealed must be base64");
	}
	const bytes = (sealed.length / 4) * 3 - (sealed.endsWith("==") ? 2 : sealed.endsWith("=") ? 1 : 0);
	if (bytes > MAX_SEALED) throw new HttpError(413, "sealed is over 64 KiB");
	await env.DB.prepare(
		"INSERT INTO net_index (scope, id, sealed) VALUES (?1, ?2, ?3) ON CONFLICT (scope, id) DO UPDATE SET sealed = excluded.sealed",
	)
		.bind(s, id, sealed)
		.run();
	return noContent();
}

export async function remove(req: Request, env: Env, id: string): Promise<Response> {
	await env.DB.prepare("DELETE FROM net_index WHERE scope = ? AND id = ?")
		.bind(await scope(req), id)
		.run();
	return noContent();
}
