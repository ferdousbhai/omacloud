// The bucket behind, standing in: objects in memory, and fetch answering
// what the gateway and the scheduled job ask of it (get, head, put, copy,
// delete, batch delete, an upload's parts and completing it, list v2,
// If-Match). `calls` says what was asked, in order; `failCopy` makes copies
// fail (`failCopyOf` those from under a prefix); `parts` holds open
// uploads' part sizes.

import { createHash } from "node:crypto";

export interface Obj {
	body: Uint8Array;
	etag: string;
}

const xml = (s: string) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
const unxml = (s: string) => s.replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&quot;/g, '"').replace(/&amp;/g, "&");

export function bucket(name = "everyone") {
	const objects = new Map<string, Obj>();
	const calls: string[] = [];
	// `sizes`: objects said to be larger than they are
	const state = { failCopy: false, failCopyOf: "", parts: new Map<string, number[]>(), sizes: new Map<string, number>() };
	const size = (k: string) => state.sizes.get(k) ?? objects.get(k)!.body.length;
	const put = (key: string, body: string | Uint8Array) => {
		const bytes = typeof body === "string" ? new TextEncoder().encode(body) : body;
		objects.set(key, { body: bytes, etag: createHash("md5").update(bytes).digest("hex") });
	};
	const text = (key: string) => {
		const o = objects.get(key);
		return o ? new TextDecoder().decode(o.body) : null;
	};
	const fetch = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
		const url = new URL(String(input));
		const method = init?.method ?? "GET";
		const headers = new Headers(init?.headers);
		const path = url.pathname.slice(1);
		if (!path.startsWith(name)) throw new Error(`not the bucket: ${url}`);
		const key = decodeURIComponent(path.slice(name.length + 1));
		const q = url.searchParams;
		const copySource = headers.get("x-amz-copy-source");
		calls.push(`${copySource ? "COPY" : method} ${key}${copySource ? ` from ${decodeURIComponent(copySource)}` : ""}`);
		const body = new Uint8Array(await new Response(init?.body ?? null).arrayBuffer());
		if (!key) {
			if (method === "POST" && q.has("delete")) {
				const keys = [...new TextDecoder().decode(body).matchAll(/<Key>([^<]*)<\/Key>/g)].map((m) => unxml(m[1]));
				for (const k of keys) objects.delete(k);
				return new Response("<DeleteResult></DeleteResult>");
			}
			if (method === "GET" && q.get("list-type") === "2") {
				const prefix = q.get("prefix") ?? "";
				const after = q.get("continuation-token") ?? q.get("start-after") ?? "";
				const max = Number(q.get("max-keys") ?? 1000);
				const all = [...objects.keys()].filter((k) => k.startsWith(prefix) && k > after).sort();
				if (q.get("delimiter") === "/") {
					const folders = [...new Set(all.map((k) => k.slice(0, k.indexOf("/", prefix.length) + 1)).filter(Boolean))];
					return new Response(
						`<ListBucketResult><Prefix>${xml(prefix)}</Prefix>` +
							folders.map((f) => `<CommonPrefixes><Prefix>${xml(f)}</Prefix></CommonPrefixes>`).join("") +
							"<IsTruncated>false</IsTruncated></ListBucketResult>",
					);
				}
				const page = all.slice(0, max);
				const truncated = all.length > page.length;
				return new Response(
					`<ListBucketResult><Name>${name}</Name><Prefix>${xml(prefix)}</Prefix>` +
						page
							.map((k) => `<Contents><Key>${xml(k)}</Key><ETag>&quot;${objects.get(k)!.etag}&quot;</ETag><Size>${size(k)}</Size></Contents>`)
							.join("") +
						`<IsTruncated>${truncated}</IsTruncated>` +
						(truncated ? `<NextContinuationToken>${xml(page.at(-1)!)}</NextContinuationToken>` : "") +
						"</ListBucketResult>",
				);
			}
			throw new Error(`unexpected ${method} ${url}`);
		}
		const o = objects.get(key);
		const ifMatch = headers.get("if-match");
		if (ifMatch !== null && (method === "PUT" || method === "DELETE") && !copySource) {
			if (!o) return new Response("<Error><Code>NoSuchKey</Code></Error>", { status: 404 });
			if (ifMatch.replace(/"/g, "") !== o.etag)
				return new Response("<Error><Code>PreconditionFailed</Code></Error>", { status: 412 });
		}
		if (method === "GET" && q.has("uploadId")) {
			const sizes = state.parts.get(q.get("uploadId")!);
			if (!sizes) return new Response("<Error><Code>NoSuchUpload</Code></Error>", { status: 404 });
			return new Response(
				`<ListPartsResult><IsTruncated>false</IsTruncated>${sizes.map((n, i) => `<Part><PartNumber>${i + 1}</PartNumber><Size>${n}</Size></Part>`).join("")}</ListPartsResult>`,
			);
		}
		if (method === "HEAD" || method === "GET") {
			if (!o) return new Response(method === "GET" ? "<Error><Code>NoSuchKey</Code></Error>" : null, { status: 404 });
			return new Response(method === "GET" ? o.body : null, {
				headers: { "content-length": String(size(key)), etag: `"${o.etag}"` },
			});
		}
		if (method === "PUT" && copySource) {
			const from = decodeURIComponent(copySource).slice(name.length + 2);
			const src = objects.get(from);
			if (state.failCopy || (state.failCopyOf && from.startsWith(state.failCopyOf))) return new Response("<Error><Code>InternalError</Code></Error>", { status: 500 });
			if (!src) return new Response("<Error><Code>NoSuchKey</Code></Error>", { status: 404 });
			objects.set(key, { ...src });
			return new Response(`<CopyObjectResult><ETag>"${src.etag}"</ETag></CopyObjectResult>`);
		}
		if (method === "PUT") {
			if (headers.get("if-none-match") === "*" && o)
				return new Response("<Error><Code>PreconditionFailed</Code></Error>", { status: 412 });
			put(key, body);
			return new Response(null, { status: 200 });
		}
		if (method === "POST" && q.has("uploadId")) {
			put(key, `assembled ${q.get("uploadId")}`);
			return new Response("<CompleteMultipartUploadResult></CompleteMultipartUploadResult>");
		}
		if (method === "DELETE") {
			objects.delete(key);
			return new Response(null, { status: 204 });
		}
		throw new Error(`unexpected ${method} ${url}`);
	};
	return { objects, calls, state, put, text, fetch: fetch as typeof globalThis.fetch };
}
