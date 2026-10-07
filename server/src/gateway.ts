// The S3 gateway. A computer signs requests for its own bucket, named by
// its account's id, with the key it got at sign-in; the gateway checks the
// signature, keeps the request inside the account's folder of the one
// bucket behind, and signs it again with that bucket's key.
//
// Only what Omacloud's storage (OpenDAL's S3 service) asks for gets through:
// objects (get, head, put, delete, multipart), listing (v2), batch delete,
// and the bucket itself as a formality. Anything else is refused, so a key
// can't reach outside its folder, change permissions or copy from elsewhere.
// What a request deletes or overwrites goes to the trash first (trash.ts).

import { type Account, keyAccount, masterKey, release, reserve, secretOf, touched } from "./db.ts";
import { canonicalRequest, decode, encodePath, parseAuth, same, scope, sha256Hex, sign } from "./sigv4.ts";
import * as trash from "./trash.ts";
import { escapeXml, unescapeXml, upstream } from "./upstream.ts";
import { uploadBytes } from "./usage.ts";

/** Request headers that go through to the bucket. Everything else (ACLs, copy sources, server-side encryption) stays behind. */
const PASS_REQUEST = [
	"content-type",
	"content-md5",
	"range",
	"if-match",
	"if-none-match",
	"if-modified-since",
	"if-unmodified-since",
];
/** Response headers that come back. */
const PASS_RESPONSE = ["content-type", "content-length", "content-range", "accept-ranges", "etag", "last-modified"];
/** Largest body the gateway reads to rewrite. */
const SMALL = 8 << 20;
const UNSIGNED = "UNSIGNED-PAYLOAD";
const LIST_PARAMS = ["list-type", "prefix", "delimiter", "max-keys", "continuation-token", "start-after", "fetch-owner"];

function error(status: number, code: string, message: string): Response {
	return new Response(
		`<?xml version="1.0" encoding="UTF-8"?><Error><Code>${code}</Code><Message>${escapeXml(message)}</Message></Error>`,
		{ status, headers: { "content-type": "application/xml" } },
	);
}
const denied = (message: string) => error(403, "AccessDenied", message);
const notImplemented = () => error(501, "NotImplemented", "Omacloud storage doesn't do this");
const tooLarge = () => error(400, "EntityTooLarge", "An object can be at most 5 GiB");

/** Whether the bucket turned a request down for a header it doesn't do (here: If-Match). */
async function unsupported(r: Response): Promise<boolean> {
	if (r.status === 501) return true;
	if (r.status !== 400) return false;
	return /<Code>(NotImplemented|InvalidArgument)<\/Code>/.test(await r.clone().text());
}

/** A key the gateway will put under an account's folder: no `.` or `..` segments (a URL would resolve them, out of the folder), no control characters. */
export function validKey(key: string): boolean {
	return (
		key.length > 0 &&
		key.length <= 900 &&
		!/[\u0000-\u001f\u007f-\u009f]/.test(key) &&
		!key.split("/").some((s) => s === "." || s === "..")
	);
}

type Op = "HeadBucket" | "CreateBucket" | "Location" | "List" | "DeleteMany" | "Object" | "Multipart";

/** The request's decoded query pairs, or null if one doesn't decode. */
export function queryPairs(raw: string): [string, string][] | null {
	const out: [string, string][] = [];
	for (const p of raw.split("&")) {
		if (!p) continue;
		const i = p.indexOf("=");
		const k = decode(i < 0 ? p : p.slice(0, i));
		const v = decode(i < 0 ? "" : p.slice(i + 1));
		if (k === null || v === null) return null;
		out.push([k, v]);
	}
	return out;
}

export function op(method: string, key: string | null, query: [string, string][]): Op | null {
	// some clients name the operation; it changes nothing
	const names = query.map(([k]) => k).filter((k) => k !== "x-id");
	if (key === null) {
		if (method === "HEAD" && names.length === 0) return "HeadBucket";
		if (method === "PUT" && names.length === 0) return "CreateBucket";
		if (method === "GET" && names.length === 1 && names[0] === "location") return "Location";
		if (
			method === "GET" &&
			query.some(([k, v]) => k === "list-type" && v === "2") &&
			names.every((n) => LIST_PARAMS.includes(n))
		)
			return "List";
		if (method === "POST" && names.length === 1 && names[0] === "delete") return "DeleteMany";
		return null;
	}
	const sorted = [...names].sort().join("&");
	if (["GET", "HEAD", "PUT", "DELETE"].includes(method) && sorted === "") return "Object";
	if (method === "POST" && sorted === "uploads") return "Multipart";
	if (method === "PUT" && sorted === "partNumber&uploadId") return "Multipart";
	if (["POST", "DELETE", "GET"].includes(method) && sorted === "uploadId") return "Multipart";
	return null;
}

/** Who signed a request: the account, and the key it used. */
export interface Signer {
	account: Account;
	accessKeyId: string;
}

/** The account whose key signed the request (to storage, or to the key API), or the refusal. */
export async function authenticate(req: Request, env: Env, path: string, query: string): Promise<Signer | Response> {
	if (query.includes("X-Amz-Signature")) return denied("Signed URLs aren't accepted");
	const auth = parseAuth(req.headers.get("authorization") ?? "");
	if (!auth) return denied("Sign requests with AWS signature version 4");
	if (auth.region !== env.REGION || auth.service !== "s3")
		return error(400, "AuthorizationHeaderMalformed", `Sign for region ${env.REGION}`);
	for (const needed of ["host", "x-amz-date", "x-amz-content-sha256"]) {
		if (!auth.signedHeaders.includes(needed)) return denied(`Sign the ${needed} header`);
	}
	const stamp = req.headers.get("x-amz-date") ?? "";
	const m = stamp.match(/^(\d{4})(\d{2})(\d{2})T(\d{2})(\d{2})(\d{2})Z$/);
	const at = m ? Date.UTC(+m[1], +m[2] - 1, +m[3], +m[4], +m[5], +m[6]) : NaN;
	if (!(Math.abs(Date.now() - at) <= 15 * 60 * 1000) || !stamp.startsWith(auth.day))
		return error(403, "RequestTimeTooSkewed", "This computer's clock is off by more than 15 minutes");
	const account = await keyAccount(env.DB, auth.accessKeyId);
	if (!account) return error(403, "InvalidAccessKeyId", "This key isn't in use");
	const signed: [string, string][] = auth.signedHeaders.map((h) => [h, req.headers.get(h) ?? ""]);
	const payload = req.headers.get("x-amz-content-sha256") ?? "";
	const canonical = canonicalRequest(req.method, path, query, signed, payload);
	const secret = await secretOf(masterKey(env), auth.accessKeyId);
	if (!same(await sign(secret, stamp, scope(auth), canonical), auth.signature))
		return error(403, "SignatureDoesNotMatch", "The signature doesn't match this key");
	if (account.disabled) return denied("This account is closed");
	return { account, accessKeyId: auth.accessKeyId };
}

/** Requests to the storage host. */
export async function handle(req: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
	const url = new URL(req.url);
	const path = url.pathname;
	const rawQuery = url.search.slice(1);
	const signer = await authenticate(req, env, path, rawQuery);
	if (signer instanceof Response) return signer;
	const { account } = signer;

	const query = queryPairs(rawQuery);
	if (!query) return error(400, "InvalidArgument", "Bad query");
	const rest = path.slice(1);
	const slash = rest.indexOf("/");
	const bucket = slash < 0 ? rest : rest.slice(0, slash);
	const rawKey = slash < 0 ? "" : rest.slice(slash + 1);
	if (bucket !== account.id) return denied("This key opens only its own bucket");
	let key: string | null = null;
	if (rawKey) {
		key = decode(rawKey);
		if (key === null || !validKey(key)) return error(400, "InvalidArgument", "Bad object name");
	}
	if (req.headers.has("x-amz-copy-source")) return notImplemented();
	const payload = req.headers.get("x-amz-content-sha256") ?? UNSIGNED;
	if (payload.startsWith("STREAMING-")) return notImplemented();
	const operation = op(req.method, key, query);
	if (!operation) return notImplemented();
	const folder = `${account.id}/`;

	if (operation === "HeadBucket" || operation === "CreateBucket") return new Response(null, { status: 200 });
	if (operation === "Location")
		return new Response(
			`<?xml version="1.0" encoding="UTF-8"?><LocationConstraint xmlns="http://s3.amazonaws.com/doc/2006-03-01/">${env.REGION}</LocationConstraint>`,
			{ headers: { "content-type": "application/xml" } },
		);

	// what goes to the bucket behind
	const upstreamPath = key === null ? "" : encodePath(folder + key);
	let upstreamQuery = query.filter(([k]) => k !== "x-id" && k !== "fetch-owner");
	if (operation === "List") {
		// inside the folder, whatever was asked for
		const prefix = query.find(([k]) => k === "prefix")?.[1] ?? "";
		upstreamQuery = upstreamQuery
			.filter(([k]) => k !== "prefix")
			.map(([k, v]) => (k === "start-after" ? [k, folder + v] : [k, v]));
		upstreamQuery.push(["prefix", folder + prefix]);
	}
	const pass: [string, string][] = [];
	for (const h of PASS_REQUEST) {
		const v = req.headers.get(h);
		if (v !== null) pass.push([h, v]);
	}
	const lengthHeader = req.headers.get("content-length");
	const length = lengthHeader !== null && /^\d+$/.test(lengthHeader) ? Number(lengthHeader) : null;
	const writes = req.method === "PUT" && (operation === "Object" || operation === "Multipart");
	if (writes) {
		if (length === null) return error(411, "MissingContentLength", "Give the length");
		// every object stays small enough to copy to the trash
		if (length > trash.MAX_OBJECT) return tooLarge();
		// taken before the write, so writes side by side can't overrun it
		if (!(await reserve(env.DB, account.id, length)))
			return error(403, "QuotaExceeded", "Your Omacloud storage is full");
	}

	let body: BodyInit | null = null;
	let upstreamPayload = payload;
	// what this request would delete or overwrite: kept in the trash first
	let doomed: string[] = [];
	if (operation === "DeleteMany") {
		if (length === null || length > SMALL) return error(400, "InvalidRequest", "Too large");
		const bytes = new Uint8Array(await req.arrayBuffer());
		if (payload !== UNSIGNED && !same(await sha256Hex(bytes), payload))
			return error(400, "XAmzContentSHA256Mismatch", "The body doesn't match its hash");
		const asked = parseDelete(new TextDecoder().decode(bytes));
		if (asked === null) return error(400, "MalformedXML", "Bad delete request");
		doomed = asked.keys;
		const encoded = new TextEncoder().encode(deleteXml(asked, folder));
		const md5 = new Uint8Array(await crypto.subtle.digest("MD5", encoded));
		const i = pass.findIndex(([k]) => k === "content-md5");
		if (i >= 0) pass.splice(i, 1);
		pass.push(["content-md5", btoa(String.fromCharCode(...md5))]);
		upstreamPayload = await sha256Hex(encoded);
		body = encoded;
	} else if (req.method === "PUT" || req.method === "POST") {
		// streamed through, its length kept so the bucket sees one
		const n = length ?? 0;
		body = req.body && n > 0 ? req.body.pipeThrough(new FixedLengthStream(n)) : "";
	}

	// a put that can only create (If-None-Match: *, which every bucket
	// behind honors on a plain put) destroys nothing; nor does a part, or
	// starting or abandoning an upload
	const createOnly = req.method === "PUT" && req.headers.get("if-none-match")?.trim() === "*";
	const completes = operation === "Multipart" && req.method === "POST" && query.some(([k]) => k === "uploadId");
	if (key !== null && ((operation === "Object" && ((req.method === "PUT" && !createOnly) || req.method === "DELETE")) || completes))
		doomed = [key];
	doomed = doomed.filter((k) => !trash.exempt(k));
	if (completes) {
		const size = await uploadBytes(env, upstreamPath, query.find(([k]) => k === "uploadId")![1]);
		if (size !== null && size > trash.MAX_OBJECT) return tooLarge();
	}
	let kept = new Map<string, string | null>();
	if (doomed.length) {
		try {
			const r = await trash.preserve(env, account, doomed);
			kept = r.kept;
			if (r.full) ctx.waitUntil(trash.alertFull(env, account.id));
		} catch (e) {
			if (writes) ctx.waitUntil(release(env.DB, account.id, length ?? 0));
			if (e instanceof trash.Full) {
				ctx.waitUntil(trash.alertFull(env, account.id));
				return error(
					400,
					"TooMuchDeleted",
					"Too much was deleted or replaced in this account in the last 14 days: deleting and replacing files is paused, and frees up as deleted files expire",
				);
			}
			if (e instanceof trash.TooLarge) return tooLarge();
			console.error(JSON.stringify({ event: "trash failed", account: account.id, method: req.method, error: String(e) }));
			return error(503, "ServiceUnavailable", "Try again");
		}
	}
	// A single delete or put lands only on the version just kept: the bucket
	// checks (If-Match, or If-None-Match: * for nothing there; Ceph and
	// SeaweedFS honor both), and a version written in between makes it try
	// again. A batch delete and a finished upload can't say so on every
	// bucket: a version another key of the account writes between the copy
	// and the delete (milliseconds) is lost to those.
	let guarded = false;
	if (key !== null && operation === "Object" && kept.has(key)) {
		const etag = kept.get(key);
		if (etag === null && req.method === "DELETE") return new Response(null, { status: 204 });
		const h = etag === null ? "if-none-match" : "if-match";
		const i = pass.findIndex(([k]) => k === h);
		const theirs = i >= 0 ? pass[i][1].trim().replace(/^"|"$/g, "") : null;
		if (etag !== null && theirs !== null && theirs !== etag) {
			if (writes) ctx.waitUntil(release(env.DB, account.id, length ?? 0));
			return error(412, "PreconditionFailed", "At least one of the preconditions you specified did not hold");
		}
		if (i >= 0) pass.splice(i, 1);
		pass.push([h, etag === null ? "*" : `"${etag}"`]);
		guarded = true;
		// a small overwrite (records, never packs) is held, so it can go
		// again without the check if the bucket doesn't take it
		if (req.method === "PUT" && body instanceof ReadableStream && (length ?? 0) <= SMALL)
			body = new Uint8Array(await new Response(body).arrayBuffer());
	}

	let response: Response;
	try {
		response = await upstream(env, req.method, upstreamPath, upstreamQuery, pass, upstreamPayload, body);
		// a bucket that doesn't do the check on this request: once more
		// without it, as before there was one (the window above, for this
		// request too)
		if (guarded && !(body instanceof ReadableStream) && (await unsupported(response))) {
			console.error(JSON.stringify({ event: "bucket refused a conditional request", method: req.method }));
			const plain = pass.filter(([k]) => k !== "if-match" && k !== "if-none-match");
			response = await upstream(env, req.method, upstreamPath, upstreamQuery, plain, upstreamPayload, body);
		}
	} catch (e) {
		console.error(JSON.stringify({ account: account.id, method: req.method, error: String(e) }));
		if (writes) ctx.waitUntil(release(env.DB, account.id, length ?? 0));
		return error(502, "ServiceUnavailable", "Try again");
	}
	if (writes && !response.ok) ctx.waitUntil(release(env.DB, account.id, length ?? 0));
	// written by someone else since it was kept: again
	if (guarded && response.status === 412) return error(503, "ServiceUnavailable", "Try again");
	// gone meanwhile (a delete answers that as if it went now)
	if (guarded && req.method === "DELETE" && response.status === 404) return new Response(null, { status: 204 });
	// due a count again, even if one ran while this was written
	if (response.ok && (writes || operation === "DeleteMany" || req.method === "DELETE"))
		ctx.waitUntil(touched(env.DB, account.id));

	// listings, multipart answers and errors name the folder and the bucket
	// behind: say them as the computer knows them
	const rewrite = req.method !== "HEAD" && (!response.ok || operation !== "Object");
	const headers = new Headers();
	for (const h of PASS_RESPONSE) {
		if (rewrite && h === "content-length") continue;
		const v = response.headers.get(h);
		if (v !== null) headers.set(h, v);
	}
	for (const [h, v] of response.headers) if (h.startsWith("x-amz-checksum-")) headers.set(h, v);
	if (rewrite) {
		const text = unfolder(await response.text(), folder, env.UPSTREAM_BUCKET, account.id);
		return new Response(text, { status: response.status, headers });
	}
	return new Response(response.body, { status: response.status, headers });
}

/** A batch delete, its keys moved into `folder`; null if it's anything but plain keys (see parseDelete). */
export function rewriteDelete(xml: string, folder: string): string | null {
	const asked = parseDelete(xml);
	return asked && deleteXml(asked, folder);
}

function deleteXml(asked: { keys: string[]; quiet: boolean }, folder: string): string {
	return (
		`<?xml version="1.0" encoding="UTF-8"?><Delete xmlns="http://s3.amazonaws.com/doc/2006-03-01/">` +
		(asked.quiet ? "<Quiet>true</Quiet>" : "") +
		asked.keys.map((k) => `<Object><Key>${escapeXml(folder + k)}</Key></Object>`).join("") +
		"</Delete>"
	);
}

/**
 * A batch delete's keys; null if it's anything but plain keys. Parsed
 * strictly: an element it doesn't know (a namespace prefix, a version)
 * fails it rather than slipping past.
 */
export function parseDelete(xml: string): { keys: string[]; quiet: boolean } | null {
	const body = xml.replace(/^\s*<\?xml[^?]*\?>/, "").trim();
	const tokens = body.match(/<[^>]*>|[^<]+/g) ?? [];
	const keys: string[] = [];
	let quiet = false;
	const stack: string[] = [];
	let text = "";
	for (const t of tokens) {
		if (!t.startsWith("<")) {
			text += t;
			continue;
		}
		const close = t.match(/^<\/([A-Za-z]+)\s*>$/);
		const open = t.match(/^<([A-Za-z]+)(\s+xmlns="[^"]*")?\s*>$/);
		if (open) {
			const name = open[1];
			const parent = stack.at(-1);
			const ok =
				(name === "Delete" && stack.length === 0) ||
				(parent === "Delete" && (name === "Object" || name === "Quiet")) ||
				(parent === "Object" && name === "Key");
			if (!ok || (open[2] && name !== "Delete")) return null;
			stack.push(name);
			text = "";
		} else if (close) {
			if (stack.pop() !== close[1]) return null;
			if (close[1] === "Key") {
				const k = unescapeXml(text);
				if (k === null || !validKey(k)) return null;
				keys.push(k);
			} else if (close[1] === "Quiet") {
				if (text.trim() !== "true" && text.trim() !== "false") return null;
				quiet = text.trim() === "true";
			} else if (text.trim() !== "") return null;
			text = "";
		} else return null;
	}
	if (stack.length !== 0 || keys.length === 0 || keys.length > 1000) return null;
	return { keys, quiet };
}

/** The bucket's XML with the account's folder taken off keys and prefixes, and the bucket behind named as the account's. */
export function unfolder(xml: string, folder: string, real: string, bucket: string): string {
	let s = xml;
	for (const tag of ["Key", "Prefix", "StartAfter"]) s = s.replaceAll(`<${tag}>${folder}`, `<${tag}>`);
	for (const tag of ["Bucket", "Name", "BucketName"])
		s = s.replaceAll(`<${tag}>${real}</${tag}>`, `<${tag}>${bucket}</${tag}>`);
	return s.replaceAll(`/${real}/${folder}`, `/${bucket}/`);
}
