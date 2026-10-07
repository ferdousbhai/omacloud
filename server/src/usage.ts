// Usage, counted again for accounts written to, a few pages at a time: the
// objects in the account's folder, and the parts of multipart uploads still
// open there (which a listing doesn't show, but which take space). An upload
// left open more than a day is aborted, its parts with it; while any are
// open the account stays due a count, so none is forgotten.

import { countPage, unescapeXml, upstream, EMPTY_SHA256 } from "./upstream.ts";
import { encodePath } from "./sigv4.ts";

/** List pages one usage count may read, a thousand objects each (Workers Paid allows 10,000 subrequests). */
const COUNT_PAGES = 1000;
/** Open uploads looked at per account and count. */
const UPLOADS = 100;
/** An upload open longer is abandoned. */
const UPLOAD_MS = 86400 * 1000;

const tag = (xml: string, name: string) => {
	const m = xml.match(new RegExp(`<${name}>([^<]*)</${name}>`));
	return m ? unescapeXml(m[1]) : null;
};

/**
 * The bytes in the open multipart uploads under `prefix`, aborting the
 * abandoned ones; `open` if any are left.
 */
export async function openUploads(env: Env, prefix: string, now = Date.now()): Promise<{ bytes: number; open: boolean }> {
	const q: [string, string][] = [
		["uploads", ""],
		["prefix", prefix],
		["max-uploads", String(UPLOADS)],
	];
	const r = await upstream(env, "GET", "", q, [], EMPTY_SHA256, null);
	if (!r.ok) throw new Error(`listing uploads in ${prefix}: ${r.status}`);
	const text = await r.text();
	let bytes = 0;
	let open = /<IsTruncated>true<\/IsTruncated>/.test(text);
	for (const [, upload] of text.matchAll(/<Upload>([\s\S]*?)<\/Upload>/g)) {
		const key = tag(upload, "Key");
		const id = tag(upload, "UploadId");
		const started = Date.parse(tag(upload, "Initiated") ?? "");
		if (!key || !id || !key.startsWith(prefix)) continue;
		const path = encodePath(key);
		if (now - started > UPLOAD_MS) {
			const d = await upstream(env, "DELETE", path, [["uploadId", id]], [], EMPTY_SHA256, null);
			if (!d.ok && d.status !== 404) throw new Error(`aborting an upload in ${prefix}: ${d.status}`);
			continue;
		}
		open = true;
		let marker: string | null = null;
		do {
			const pq: [string, string][] = [["uploadId", id]];
			if (marker) pq.push(["part-number-marker", marker]);
			const p = await upstream(env, "GET", path, pq, [], EMPTY_SHA256, null);
			if (!p.ok) break; // finished or aborted meanwhile
			const parts = await p.text();
			for (const m of parts.matchAll(/<Part>[\s\S]*?<Size>(\d+)<\/Size>[\s\S]*?<\/Part>/g)) bytes += Number(m[1]);
			marker = /<IsTruncated>true<\/IsTruncated>/.test(parts) ? tag(parts, "NextPartNumberMarker") : null;
		} while (marker);
	}
	return { bytes, open };
}

export async function count(env: Env): Promise<void> {
	let pages = COUNT_PAGES;
	const dirty = await env.DB.prepare(
		"SELECT id, scan_token, scan_total FROM accounts WHERE dirty = 1 OR scan_token IS NOT NULL LIMIT 100",
	).all<{ id: string; scan_token: string | null; scan_total: number }>();
	for (const a of dirty.results) {
		let token = a.scan_token;
		let total = token ? a.scan_total : 0;
		if (!token) await env.DB.prepare("UPDATE accounts SET dirty = 0 WHERE id = ?").bind(a.id).run();
		try {
			while (pages-- > 0) {
				const page = await countPage(env, `${a.id}/`, token);
				total += page.bytes;
				token = page.next;
				if (!token) break;
			}
			if (token) {
				await env.DB.prepare("UPDATE accounts SET scan_token = ?2, scan_total = ?3 WHERE id = ?1")
					.bind(a.id, token, total)
					.run();
				return;
			}
			const uploads = await openUploads(env, `${a.id}/`);
			await env.DB.prepare(
				"UPDATE accounts SET used = ?2, scan_token = NULL, scan_total = 0, dirty = max(dirty, ?3) WHERE id = ?1",
			)
				.bind(a.id, total + uploads.bytes, uploads.open ? 1 : 0)
				.run();
		} catch (e) {
			// count it again next time: until then `used` may still hold
			// space reserved for writes that never landed
			await env.DB.prepare("UPDATE accounts SET dirty = 1 WHERE id = ?").bind(a.id).run();
			console.error(JSON.stringify({ event: "usage count failed", account: a.id, error: String(e) }));
			return;
		}
		if (pages <= 0) return;
	}
}
