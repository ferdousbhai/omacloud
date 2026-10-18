// Who may do what: node signatures, capabilities, and where a request
// came from (only its /24 or /48, never the address).

import { HttpError, fromHex, sha256Hex } from "./http.ts";

/** How far a node's clock may be from ours, in seconds. */
const SKEW = 300;

/**
 * The node that signed this request (its Ed25519 key, hex), or a 401. The
 * signature covers the method, path, time and body, so it can't be replayed
 * elsewhere, nor later than the skew allows.
 */
export async function signedBy(req: Request, path: string, body: Uint8Array, now: number): Promise<string> {
	const node = (req.headers.get("x-node") ?? "").toLowerCase();
	const time = req.headers.get("x-time") ?? "";
	const key = fromHex(node);
	const sig = fromHex(req.headers.get("x-sig") ?? "");
	if (!key || key.length !== 32 || !sig || sig.length !== 64 || !/^\d{1,12}$/.test(time)) {
		throw new HttpError(401, "missing or malformed node signature");
	}
	if (Math.abs(now - Number(time)) > SKEW) throw new HttpError(401, "x-time is too far from now");
	const message = `omacloud-node-v1\n${req.method}\n${path}\n${time}\n${await sha256Hex(body)}`;
	let ok = false;
	try {
		const pub = await crypto.subtle.importKey("raw", key, { name: "Ed25519" }, false, ["verify"]);
		ok = await crypto.subtle.verify({ name: "Ed25519" }, pub, sig, new TextEncoder().encode(message));
	} catch {
		ok = false;
	}
	if (!ok) throw new HttpError(401, "bad node signature");
	return node;
}

/** The stored form of a capability (`x-cap`): its sha256, hex. */
export async function capHash(req: Request): Promise<string> {
	const cap = req.headers.get("x-cap") ?? "";
	if (cap.length === 0 || cap.length > 512) throw new HttpError(401, "missing x-cap");
	return sha256Hex(cap);
}

/** The /24 or /48 of the address Cloudflare saw, or a 400. */
export function requestPrefix(req: Request): string {
	const prefix = prefixOf(req.headers.get("cf-connecting-ip") ?? "");
	if (!prefix) throw new HttpError(400, "no client address");
	return prefix;
}

/** The autonomous system Cloudflare saw the request from, 0 if unknown. */
export function requestAsn(req: Request): number {
	const asn = (req as { cf?: { asn?: unknown } }).cf?.asn;
	return typeof asn === "number" && Number.isSafeInteger(asn) ? asn : 0;
}

/** `a.b.c.0/24` for IPv4, `x:y:z::/48` for IPv6, or null. */
export function prefixOf(ip: string): string | null {
	const v4 = ipv4(ip);
	if (v4) return `${v4[0]}.${v4[1]}.${v4[2]}.0/24`;
	const g = ipv6(ip);
	if (!g) return null;
	// an IPv4 address written as IPv6 is that IPv4 address
	if (g.slice(0, 5).every((x) => x === 0) && g[5] === 0xffff) {
		return `${g[6] >> 8}.${g[6] & 0xff}.${g[7] >> 8}.0/24`;
	}
	return `${g[0].toString(16)}:${g[1].toString(16)}:${g[2].toString(16)}::/48`;
}

function ipv4(ip: string): number[] | null {
	const parts = ip.split(".");
	if (parts.length !== 4 || !parts.every((p) => /^\d{1,3}$/.test(p) && Number(p) < 256)) return null;
	return parts.map(Number);
}

/** The eight 16-bit groups of an IPv6 address. */
function ipv6(ip: string): number[] | null {
	let s = ip.replace(/%.*$/, "");
	// a dotted IPv4 tail is the last two groups
	const tail = s.match(/^(.*:)(\d+\.\d+\.\d+\.\d+)$/);
	if (tail) {
		const v4 = ipv4(tail[2]);
		if (!v4) return null;
		s = `${tail[1]}${((v4[0] << 8) | v4[1]).toString(16)}:${((v4[2] << 8) | v4[3]).toString(16)}`;
	}
	const halves = s.split("::");
	if (halves.length > 2) return null;
	const groups = (part: string) => (part === "" ? [] : part.split(":"));
	const head = groups(halves[0]);
	const rest = halves.length === 2 ? groups(halves[1]) : [];
	const missing = 8 - head.length - rest.length;
	if (halves.length === 2 ? missing < 1 : missing !== 0) return null;
	const all = [...head, ...Array<string>(missing).fill("0"), ...rest];
	if (!all.every((x) => /^[0-9a-fA-F]{1,4}$/.test(x))) return null;
	return all.map((x) => parseInt(x, 16));
}
