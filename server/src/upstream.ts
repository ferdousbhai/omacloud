// Requests to the bucket behind, signed with its key.

import { amzDate, canonicalQuery, canonicalRequest, encode, sign } from "./sigv4.ts";

export const EMPTY_SHA256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/**
 * A request to the bucket behind. `path` is the encoded object key (empty
 * for the bucket), `query` decoded pairs, `headers` lowercase ones to pass
 * on (signed, apart from content-length).
 */
export async function upstream(
	env: Env,
	method: string,
	path: string,
	query: [string, string][],
	headers: [string, string][],
	payload: string,
	body: BodyInit | null,
): Promise<Response> {
	const endpoint = env.UPSTREAM_ENDPOINT.replace(/\/+$/, "");
	const urlPath = path ? `/${env.UPSTREAM_BUCKET}/${path}` : `/${env.UPSTREAM_BUCKET}`;
	const q = canonicalQuery(query.map(([k, v]) => `${encode(k)}=${encode(v)}`).join("&"));
	const url = `${endpoint}${urlPath}${q ? `?${q}` : ""}`;
	if (new URL(url).pathname !== urlPath) throw new Error("the path would change on the way");
	const stamp = amzDate();
	const signed: [string, string][] = headers.filter(([k]) => k !== "content-length");
	signed.push(["host", new URL(endpoint).host]);
	signed.push(["x-amz-content-sha256", payload]);
	signed.push(["x-amz-date", stamp]);
	signed.sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
	const canonical = canonicalRequest(method, urlPath, q, signed, payload);
	const scopeText = `${stamp.slice(0, 8)}/${env.UPSTREAM_REGION}/s3/aws4_request`;
	const signature = await sign(env.UPSTREAM_SECRET, stamp, scopeText, canonical);
	const out = new Headers();
	for (const [k, v] of signed) if (k !== "host") out.set(k, v);
	out.set(
		"authorization",
		`AWS4-HMAC-SHA256 Credential=${env.UPSTREAM_KEY_ID}/${scopeText}, ` +
			`SignedHeaders=${signed.map(([k]) => k).join(";")}, Signature=${signature}`,
	);
	return fetch(url, { method, headers: out, body, redirect: "manual" });
}

/** The bytes in a folder of the bucket behind, counted by listing it a page at a time. */
export async function countPage(
	env: Env,
	prefix: string,
	token: string | null,
): Promise<{ bytes: number; next: string | null }> {
	const query: [string, string][] = [
		["list-type", "2"],
		["prefix", prefix],
	];
	if (token) query.push(["continuation-token", token]);
	const r = await upstream(env, "GET", "", query, [], EMPTY_SHA256, null);
	if (!r.ok) throw new Error(`listing ${prefix}: ${r.status}`);
	const text = await r.text();
	let bytes = 0;
	for (const m of text.matchAll(/<Size>(\d+)<\/Size>/g)) bytes += Number(m[1]);
	const next = /<IsTruncated>true<\/IsTruncated>/.test(text)
		? (text.match(/<NextContinuationToken>([^<]*)<\/NextContinuationToken>/)?.[1] ?? null)
		: null;
	return { bytes, next: next === null ? null : unescapeXml(next) };
}

export function escapeXml(s: string): string {
	return s.replace(/[&<>"']/g, (c) => `&${{ "&": "amp", "<": "lt", ">": "gt", '"': "quot", "'": "apos" }[c]};`);
}

const ENTITY = /&(amp|lt|gt|quot|apos|#[0-9]{1,7}|#x[0-9a-fA-F]{1,6});/g;
const NAMED: Record<string, string> = { amp: "&", lt: "<", gt: ">", quot: '"', apos: "'" };

/** XML text with its entities decoded; null if an `&` starts anything else. */
export function unescapeXml(s: string): string | null {
	if (s.replace(ENTITY, "").includes("&")) return null;
	let bad = false;
	const out = s.replace(ENTITY, (_, e: string) => {
		if (e in NAMED) return NAMED[e];
		const n = e.startsWith("#x") ? parseInt(e.slice(2), 16) : parseInt(e.slice(1), 10);
		if (n > 0x10ffff) {
			bad = true;
			return "";
		}
		return String.fromCodePoint(n);
	});
	return bad ? null : out;
}

