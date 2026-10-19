// The node directory: storage nodes say they're up, computers list them.

import { requestAsn, requestPrefix, signedBy } from "./auth.ts";
import { take } from "./counts.ts";
import { HttpError, bodyBytes, json, parseObject } from "./http.ts";

const HOUR = 3600;
const DAY = 24 * HOUR;
/** Nodes not seen for this long aren't listed. */
const LISTED = 72 * HOUR;
/** Uptime is counted over this many hours. */
const WINDOW_HOURS = 720;
const PROBATION = 14 * DAY;
/** Nodes gone this long are forgotten, with their hours. */
const FORGET = 30 * DAY;
/** New node ids a prefix may bring a day. */
const NEW_PER_PREFIX = 20;

/** POST /api/net/nodes: a node's heartbeat. */
export async function heartbeat(req: Request, env: Env, path: string, now: number): Promise<Response> {
	const body = await bodyBytes(req, 16 * 1024);
	const node = await signedBy(req, path, body, now);
	const { addr, capacity } = parseObject(body);
	if (addr === undefined || addr === null) throw new HttpError(400, "addr is missing");
	if (typeof capacity !== "number" || !Number.isSafeInteger(capacity) || capacity < 0) {
		throw new HttpError(400, "capacity must be a whole number of bytes");
	}
	const prefix = requestPrefix(req);
	const asn = requestAsn(req);
	const db = env.DB;
	const known = await db.prepare("SELECT 1 FROM net_nodes WHERE id = ?").bind(node).first();
	if (!known && !(await take(db, "node", prefix, 1, NEW_PER_PREFIX, DAY, now))) {
		throw new HttpError(429, "too many new nodes from this network today");
	}
	const hour = Math.floor(now / HOUR);
	await db.batch([
		db
			.prepare(
				`INSERT INTO net_nodes (id, addr, capacity, asn, prefix, first_seen, last_seen) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
				ON CONFLICT (id) DO UPDATE SET addr = excluded.addr, capacity = excluded.capacity, asn = excluded.asn,
				prefix = excluded.prefix, last_seen = excluded.last_seen`,
			)
			.bind(node, JSON.stringify(addr), capacity, asn, prefix, now),
		db.prepare("INSERT OR IGNORE INTO net_node_hours (node, hour) VALUES (?, ?)").bind(node, hour),
		db.prepare("DELETE FROM net_node_hours WHERE node = ? AND hour <= ?").bind(node, hour - WINDOW_HOURS),
		// nodes long gone, and their hours
		db.prepare("DELETE FROM net_node_hours WHERE node IN (SELECT id FROM net_nodes WHERE last_seen < ?)").bind(now - FORGET),
		db.prepare("DELETE FROM net_nodes WHERE last_seen < ?").bind(now - FORGET),
	]);
	return json({ asn, prefix });
}

type Row = { id: string; asn: number; prefix: string; addr: string; first_seen: number; hours: number };

/** GET /api/net/nodes: the nodes up lately with room to spare. */
export async function list(env: Env, now: number): Promise<Response> {
	const hour = Math.floor(now / HOUR);
	const { results } = await env.DB.prepare(
		`SELECT n.id, n.asn, n.prefix, n.addr, n.first_seen,
		(SELECT COUNT(*) FROM net_node_hours h WHERE h.node = n.id AND h.hour > ?1) AS hours
		FROM net_nodes n WHERE n.last_seen >= ?2 AND n.capacity > 0 ORDER BY n.id`,
	)
		.bind(hour - WINDOW_HOURS, now - LISTED)
		.all<Row>();
	const nodes = results.map((r) => {
		// the hours it could have been seen in: since its first, this one included
		const possible = Math.min(WINDOW_HOURS, hour - Math.floor(r.first_seen / HOUR) + 1);
		return {
			id: r.id,
			asn: r.asn,
			prefix: r.prefix,
			uptime: Math.min(1, r.hours / possible),
			probation: now - r.first_seen < PROBATION,
			since: r.first_seen,
			addr: JSON.parse(r.addr),
		};
	});
	return json({ nodes });
}
