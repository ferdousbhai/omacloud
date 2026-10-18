// What every part of the network API shares: errors, bodies, hex and hashes.

/** A refusal: answered as `{"error": message}` with its status. */
export class HttpError extends Error {
	status: number;
	constructor(status: number, message: string) {
		super(message);
		this.status = status;
	}
}

export const json = (value: unknown, status = 200): Response =>
	new Response(JSON.stringify(value), { status, headers: { "content-type": "application/json" } });

export const noContent = (): Response => new Response(null, { status: 204 });
export const created = (): Response => new Response(null, { status: 201 });

/** The body's bytes, refused past `max` without reading the rest. */
export async function bodyBytes(req: Request, max: number): Promise<Uint8Array> {
	const length = Number(req.headers.get("content-length") ?? "0");
	if (length > max) throw new HttpError(413, "body too large");
	if (!req.body) return new Uint8Array();
	const chunks: Uint8Array[] = [];
	let total = 0;
	const reader = req.body.getReader();
	for (;;) {
		const { done, value } = await reader.read();
		if (done) break;
		total += value.length;
		if (total > max) {
			await reader.cancel();
			throw new HttpError(413, "body too large");
		}
		chunks.push(value);
	}
	const out = new Uint8Array(total);
	let at = 0;
	for (const c of chunks) {
		out.set(c, at);
		at += c.length;
	}
	return out;
}

/** The body parsed as a JSON object. */
export function parseObject(bytes: Uint8Array): Record<string, unknown> {
	let value: unknown;
	try {
		value = JSON.parse(new TextDecoder().decode(bytes));
	} catch {
		throw new HttpError(400, "body is not JSON");
	}
	if (typeof value !== "object" || value === null || Array.isArray(value)) throw new HttpError(400, "body is not a JSON object");
	return value as Record<string, unknown>;
}

export const readObject = async (req: Request, max: number) => parseObject(await bodyBytes(req, max));

export function toHex(bytes: Uint8Array): string {
	let s = "";
	for (const b of bytes) s += b.toString(16).padStart(2, "0");
	return s;
}

/** Hex to bytes, or null when it isn't even-length hex. */
export function fromHex(hex: string): Uint8Array | null {
	if (hex.length % 2 !== 0 || !/^[0-9a-fA-F]*$/.test(hex)) return null;
	const out = new Uint8Array(hex.length / 2);
	for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(2 * i, 2 * i + 2), 16);
	return out;
}

export async function sha256Hex(data: Uint8Array | string): Promise<string> {
	const bytes = typeof data === "string" ? new TextEncoder().encode(data) : data;
	return toHex(new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)));
}

export const now = (): number => Math.floor(Date.now() / 1000);

export const ID = /^[0-9a-f]{32}$/;
export const NAME = /^[A-Za-z0-9_-]{1,200}$/;
