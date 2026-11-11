// What `omacloud export` prints, taken apart: where the bucket is, its key and
// the repository's password. A terminal wraps long lines, and copying from one
// can break a line anywhere, in the middle of a name, a key or the address. So
// nothing here counts on where the lines end.

// `text` with any whitespace allowed between its characters, as a pattern.
const loose = (text) =>
	text
		.split("")
		.map((c) => c.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"))
		.join("\\s*");

export function pasted(words) {
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
