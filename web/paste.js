// What signs a person in, taken apart: where the bucket is, its key and the
// repository's password. Either the one-line code `omacloud drive-code`
// prints, or what `omacloud export` prints for restic. A terminal wraps long
// lines, and copying from one can break a line anywhere, in the middle of a
// name, a key or the address. So nothing here counts on where the lines end.

// `text` with any whitespace allowed between its characters, as a pattern.
const loose = (text) =>
	text
		.split("")
		.map((c) => c.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"))
		.join("\\s*");

const bytesOf = (base64url) => Uint8Array.from(atob(base64url.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0));

// What a drive code says, as the page uses it: { endpoint, bucket, root,
// region, key, secret, password }, or null if it names no bucket.
export function account(said) {
	try {
		const text = (name) => (typeof said[name] === "string" ? said[name] : "");
		if (!text("bucket") || !new URL(text("endpoint")).host) return null;
		return {
			endpoint: new URL(text("endpoint")).origin,
			bucket: text("bucket"),
			root: text("root"),
			region: text("region") || "auto",
			key: text("key"),
			secret: text("secret"),
			password: text("password"),
			...(uploadKey(said.upload) ? { upload: uploadKey(said.upload) } : {}),
		};
	} catch {
		return null;
	}
}

// What lets the page upload, when a drive link brought it: the inbox's folder
// in the bucket, the session's signing key, and a computer's word for it.
function uploadKey(said) {
	try {
		if (typeof said?.inbox !== "string" || !said.inbox || typeof said.endorsement !== "object") return null;
		const seed = bytesOf(said.seed);
		if (seed.length !== 32) return null;
		return { inbox: said.inbox.replace(/^\/+|\/+$/g, ""), seed, endorsement: JSON.stringify(said.endorsement) };
	} catch {
		return null;
	}
}

// `omacloud-drive-` and base64url of that JSON.
function driveCode(words) {
	const code = words.replace(/\s+/g, "").match(/omacloud-drive-([A-Za-z0-9_-]+)/)?.[1];
	if (!code) return null;
	try {
		return account(JSON.parse(new TextDecoder().decode(bytesOf(code))));
	} catch {
		return null;
	}
}

// What `omacloud drive-link` put after the page's `#`: the seed, and the
// address in the person's bucket where the drive code waits, sealed under
// it, both in base64url. { seed, address }, or null if that isn't what's there.
export function linked(hash) {
	const found = hash.match(/^#link=([A-Za-z0-9_-]{43})\.([A-Za-z0-9_-]+)$/);
	if (!found) return null;
	try {
		const address = new URL(new TextDecoder().decode(bytesOf(found[2])));
		// a bucket is reached over https; plain http only on this computer, for trying it
		const here = ["127.0.0.1", "localhost"].includes(address.hostname);
		if (address.protocol !== "https:" && !(address.protocol === "http:" && here)) return null;
		return { seed: bytesOf(found[1]), address: address.href };
	} catch {
		return null;
	}
}

export function pasted(words) {
	const fromCode = driveCode(words);
	if (fromCode) return fromCode;
	// a value in single quotes after NAME=, without the breaks a copy put in it
	const quoted = (name) => words.match(new RegExp(`${loose(name)}\\s*=\\s*'([^']*)'`))?.[1].replace(/\s+/g, "");
	// the address runs from "-r s3:" to "restore"
	const repo = words.match(new RegExp(`-r\\s+${loose("s3:")}([\\s\\S]*?)\\s+${loose("restore")}`))?.[1].replace(/[\s\\]+/g, "");
	if (!repo) return null;
	let url;
	try {
		url = new URL(repo);
	} catch {
		return null;
	}
	const [bucket, ...root] = url.pathname.split("/").filter(Boolean);
	if (!bucket) return null;
	return {
		endpoint: url.origin,
		bucket,
		root: root.join("/"),
		region: quoted("AWS_DEFAULT_REGION") || "auto",
		key: quoted("AWS_ACCESS_KEY_ID") || "",
		secret: quoted("AWS_SECRET_ACCESS_KEY") || "",
		password: quoted("RESTIC_PASSWORD") || "",
	};
}
