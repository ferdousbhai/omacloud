// The network's API under /api/net/ (docs/network-api.md). It logs nothing
// and keeps no address of a request but a /24 or /48 where a limit needs
// one; storage nodes publish their own, for computers to reach them.

import * as coord from "./coord.ts";
import * as credits from "./credits.ts";
import { HttpError, ID, NAME, json, now } from "./http.ts";
import * as bucket from "./index.ts";
import * as nodes from "./nodes.ts";

export async function handle(req: Request, env: Env): Promise<Response> {
	try {
		return await route(req, env);
	} catch (e) {
		if (e instanceof HttpError) return json({ error: e.message }, e.status);
		return json({ error: "internal error" }, 500);
	}
}

const notFound = () => new HttpError(404, "not found");

async function route(req: Request, env: Env): Promise<Response> {
	const url = new URL(req.url);
	const path = url.pathname;
	const parts = path.slice("/api/net/".length).split("/");
	const m = req.method;
	const t = now();
	switch (parts[0]) {
		case "nodes":
			if (parts.length !== 1) throw notFound();
			if (m === "POST") return nodes.heartbeat(req, env, path, t);
			if (m === "GET") return nodes.list(env, t);
			break;
		case "index": {
			if (parts.length === 1 && m === "GET") return bucket.list(req, env);
			if (parts.length !== 2) throw notFound();
			const id = parts[1].toLowerCase();
			if (!ID.test(id)) throw new HttpError(400, "id must be 32 hex characters");
			if (m === "GET") return bucket.get(req, env, id);
			if (m === "PUT") return bucket.put(req, env, id);
			if (m === "DELETE") return bucket.remove(req, env, id);
			break;
		}
		case "coord":
			return routeCoord(req, env, parts.slice(1), url, t);
		case "credits":
			if (parts.length !== 2) throw notFound();
			if (parts[1] === "key" && m === "GET") return credits.publicKey(env);
			if (parts[1] === "starter" && m === "POST") return credits.starter(req, env, t);
			if (parts[1] === "exchange" && m === "POST") return credits.exchange(req, env);
			break;
	}
	throw notFound();
}

async function routeCoord(req: Request, env: Env, parts: string[], url: URL, t: number): Promise<Response> {
	const m = req.method;
	const account = (parts[0] ?? "").toLowerCase();
	if (!ID.test(account)) throw new HttpError(400, "account must be 32 hex characters");
	if (parts.length === 1) {
		if (m === "PUT") return coord.create(req, env, account);
		throw notFound();
	}
	const what = parts[1];
	if (parts.length === 2) {
		if (what === "cap" && m === "PUT") return coord.replaceCap(req, env, account);
		if (what === "marker" && m === "GET") return coord.marker(env, account);
		if (what === "anchor" && m === "GET") return coord.getAnchor(env, account);
		if (what === "anchor" && m === "PUT") return coord.putAnchor(req, env, account);
		if (coord.KINDS.has(what) && m === "GET") return coord.listRecords(env, account, what as coord.Kind, url.searchParams.get("name") ?? "");
		if (coord.LOGS.has(what) && m === "GET") return coord.readLog(env, account, what, url.searchParams.get("after"));
		throw notFound();
	}
	if (parts.length === 3) {
		const name = parts[2];
		if (coord.LOGS.has(what) && m === "PUT") return coord.appendLog(req, env, account, what, name);
		if (coord.KINDS.has(what)) {
			if (!NAME.test(name)) throw new HttpError(400, "bad name");
			if (m === "PUT") return coord.putRecord(req, env, account, what as coord.Kind, name, t);
			if (m === "DELETE") return coord.deleteRecord(req, env, account, what as coord.Kind, name);
		}
	}
	throw notFound();
}
