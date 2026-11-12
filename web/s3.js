// The little of S3 the page needs, signed here with the browser's own crypto:
// listing a folder of the bucket and reading a file or a range of one. Each
// request is a presigned URL (the signature in the query), so it carries no
// header a browser asks the bucket's permission for first.

const text = new TextEncoder();
const hex = (bytes) => [...new Uint8Array(bytes)].map((b) => b.toString(16).padStart(2, "0")).join("");
// as S3 wants it: everything but unreserved characters
const encode = (s) => encodeURIComponent(s).replace(/[!'()*]/g, (c) => "%" + c.charCodeAt(0).toString(16).toUpperCase());

async function hmac(key, message) {
	const k = await crypto.subtle.importKey("raw", key, { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
	return crypto.subtle.sign("HMAC", k, text.encode(message));
}

export class Bucket {
	// endpoint like https://<account>.r2.cloudflarestorage.com; root is the
	// repository's folder in the bucket, if it has one
	constructor({ endpoint, bucket, root, region, accessKeyId, secretAccessKey }) {
		this.endpoint = new URL(endpoint);
		this.bucket = bucket;
		this.root = (root || "").replace(/^\/+|\/+$/g, "");
		if (this.root) this.root += "/";
		this.region = region || "auto";
		this.accessKeyId = accessKeyId;
		this.secret = secretAccessKey;
	}

	async url(key, query = {}) {
		const now = new Date().toISOString().replace(/[-:]|\.\d+/g, "");
		const day = now.slice(0, 8);
		const scope = `${day}/${this.region}/s3/aws4_request`;
		const path = "/" + [this.bucket, ...key.split("/").filter((p, i, all) => p || i < all.length - 1)].map(encode).join("/");
		const params = {
			...query,
			"X-Amz-Algorithm": "AWS4-HMAC-SHA256",
			"X-Amz-Credential": `${this.accessKeyId}/${scope}`,
			"X-Amz-Date": now,
			"X-Amz-Expires": "300",
			"X-Amz-SignedHeaders": "host",
		};
		const canonicalQuery = Object.keys(params)
			.sort()
			.map((k) => `${encode(k)}=${encode(params[k])}`)
			.join("&");
		const request = ["GET", path, canonicalQuery, `host:${this.endpoint.host}`, "", "host", "UNSIGNED-PAYLOAD"].join("\n");
		const digest = hex(await crypto.subtle.digest("SHA-256", text.encode(request)));
		let key_ = text.encode("AWS4" + this.secret);
		for (const part of [day, this.region, "s3", "aws4_request"]) key_ = await hmac(key_, part);
		const signature = hex(await hmac(key_, ["AWS4-HMAC-SHA256", now, scope, digest].join("\n")));
		return `${this.endpoint.origin}${path}?${canonicalQuery}&X-Amz-Signature=${signature}`;
	}

	async fetch(url, init) {
		let response;
		try {
			response = await fetch(url, init);
		} catch {
			// a browser hides why: no answer at all, or one it may not show the page
			throw new Error(
				"The bucket didn't answer this page. Check the endpoint, and that the bucket allows this site to read it (its CORS rule).",
			);
		}
		if (response.status === 401 || response.status === 403) throw new Error("The bucket refused this key.");
		if (!response.ok) throw new Error(`The bucket answered ${response.status}.`);
		return response;
	}

	// the names directly under a folder of the repository, like "index"
	async list(folder) {
		return (await this.listed(folder)).map((file) => file.name);
	}

	// the same, each with when it was last written
	async listed(folder) {
		const prefix = `${this.root}${folder}/`;
		const names = [];
		let token;
		do {
			const query = { "list-type": "2", prefix };
			if (token) query["continuation-token"] = token;
			const response = await this.fetch(await this.url("", query));
			const xml = new DOMParser().parseFromString(await response.text(), "application/xml");
			for (const entry of xml.querySelectorAll("Contents")) {
				// the folder's own marker comes back too
				const name = (entry.querySelector("Key")?.textContent ?? "").slice(prefix.length);
				const modified = entry.querySelector("LastModified")?.textContent ?? "";
				if (name && !name.endsWith("/")) names.push({ name, modified });
			}
			token = xml.querySelector("NextContinuationToken")?.textContent;
		} while (token);
		return names;
	}

	// a file of the repository, or `length` bytes of it from `offset`
	async read(name, offset, length) {
		const init = length ? { headers: { Range: `bytes=${offset}-${offset + length - 1}` } } : undefined;
		const response = await this.fetch(await this.url(this.root + name), init);
		return new Uint8Array(await response.arrayBuffer());
	}
}
