// AWS signature version 4, both ways: checking what a computer signed with
// the key it was given, and signing again for the bucket behind.

const utf8 = new TextEncoder();

/** A value encoded as SigV4 wants: everything but A-Z a-z 0-9 - _ . ~ */
export function encode(s: string): string {
	return encodeURIComponent(s).replace(
		/[!*'()]/g,
		(c) => "%" + c.charCodeAt(0).toString(16).toUpperCase(),
	);
}

/** An object key encoded for a path, slashes kept. */
export function encodePath(s: string): string {
	return encode(s).replace(/%2F/g, "/");
}

/** A percent-encoded string decoded, or null if it isn't valid UTF-8. */
export function decode(s: string): string | null {
	try {
		return decodeURIComponent(s);
	} catch {
		return null;
	}
}

export interface Auth {
	accessKeyId: string;
	day: string;
	region: string;
	service: string;
	signedHeaders: string[];
	signature: string;
}

/** The parts of an `Authorization: AWS4-HMAC-SHA256 ...` header. */
export function parseAuth(header: string): Auth | null {
	if (!header.startsWith("AWS4-HMAC-SHA256")) return null;
	const fields = new Map<string, string>();
	for (const part of header.slice("AWS4-HMAC-SHA256".length).split(",")) {
		const i = part.indexOf("=");
		if (i < 0) return null;
		const k = part.slice(0, i).trim();
		if (!["Credential", "SignedHeaders", "Signature"].includes(k)) return null;
		fields.set(k, part.slice(i + 1).trim());
	}
	const c = fields.get("Credential")?.split("/");
	const signed = fields.get("SignedHeaders");
	const signature = fields.get("Signature");
	if (!c || c.length !== 5 || c[4] !== "aws4_request" || !signed || !signature) return null;
	if (!/^[0-9a-fA-F]{64}$/.test(signature)) return null;
	return {
		accessKeyId: c[0],
		day: c[1],
		region: c[2],
		service: c[3],
		signedHeaders: signed.split(";"),
		signature: signature.toLowerCase(),
	};
}

export function scope(a: Auth): string {
	return `${a.day}/${a.region}/${a.service}/aws4_request`;
}

/** A query string as SigV4 wants it: every pair decoded, encoded again strictly, sorted. */
export function canonicalQuery(raw: string): string {
	const pairs = raw
		.split("&")
		.filter((p) => p !== "")
		.map((p) => {
			const i = p.indexOf("=");
			const [k, v] = i < 0 ? [p, ""] : [p.slice(0, i), p.slice(i + 1)];
			return [encode(decode(k) ?? k), encode(decode(v) ?? v)];
		});
	pairs.sort(([a, x], [b, y]) => (a < b ? -1 : a > b ? 1 : x < y ? -1 : x > y ? 1 : 0));
	return pairs.map(([k, v]) => `${k}=${v}`).join("&");
}

/** The canonical request; `headers` are the signed ones, lowercase, in signing order. */
export function canonicalRequest(
	method: string,
	path: string,
	query: string,
	headers: [string, string][],
	payloadHash: string,
): string {
	const lines = headers.map(([k, v]) => `${k}:${v.trim().split(/\s+/).join(" ")}\n`).join("");
	const names = headers.map(([k]) => k).join(";");
	return `${method}\n${path}\n${canonicalQuery(query)}\n${lines}\n${names}\n${payloadHash}`;
}

async function hmac(key: Uint8Array, data: string): Promise<Uint8Array> {
	const k = await crypto.subtle.importKey("raw", key, { name: "HMAC", hash: "SHA-256" }, false, [
		"sign",
	]);
	return new Uint8Array(await crypto.subtle.sign("HMAC", k, utf8.encode(data)));
}

export function hex(bytes: Uint8Array): string {
	return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

export async function sha256Hex(data: string | Uint8Array): Promise<string> {
	const bytes = typeof data === "string" ? utf8.encode(data) : data;
	return hex(new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)));
}

/** The signature of a canonical request made at `stamp` (`20261005T120000Z`) in `scope`. */
export async function sign(
	secret: string,
	stamp: string,
	scopeText: string,
	canonical: string,
): Promise<string> {
	const toSign = `AWS4-HMAC-SHA256\n${stamp}\n${scopeText}\n${await sha256Hex(canonical)}`;
	let key = utf8.encode(`AWS4${secret}`);
	for (const part of scopeText.split("/")) key = await hmac(key, part);
	return hex(await hmac(key, toSign));
}

/** HMAC-SHA256 of `data` under `key`, as hex. */
export async function hmacHex(key: Uint8Array, data: string): Promise<string> {
	return hex(await hmac(key, data));
}

/** Equal, in time that doesn't depend on where they differ. */
export function same(a: string, b: string): boolean {
	const x = utf8.encode(a);
	const y = utf8.encode(b);
	if (x.byteLength !== y.byteLength) return false;
	return crypto.subtle.timingSafeEqual(x, y);
}

/** Now as SigV4 writes it: `20261005T120000Z`. */
export function amzDate(now = new Date()): string {
	return now.toISOString().replace(/[-:]/g, "").replace(/\.\d{3}/, "");
}
