// An account's files, read straight from the person's own storage and
// decrypted here with the reader the browser page uses (crates/omacloud-web).
// "Here" is a Worker whose settings hold the storage's key and the
// repository's key sealed under the connector's passphrase, opened while an
// agent that signed in with it is asking: see docs/cloud-agents.md for what
// that trades. Only the folders the person granted are ever shown.

import * as reader from "../web/reader.js";
import { Bucket } from "../web/s3.js";
import { Dropbox } from "../web/dropbox.js";

// how long the newest version found is taken as still the newest
const FRESH_MS = 20_000;
// the newest snapshots read to find the one the account stands on, as the
// page does: a listing can't order ones written in the same second
const NEWEST = 3;
const TREES_KEPT = 2000;
const text = new TextDecoder();
const hex = (bytes) => [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
const random = (n) => crypto.getRandomValues(new Uint8Array(n));

const decode = (words) => Uint8Array.from(atob(words.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0));

// The settings' sealed part (`storage`, `key`, `upload`), opened with what a
// signed-in agent's token carries; throws if that isn't what sealed it.
export async function unlock(config, k, wasm) {
	await reader.loadModule(wasm);
	return reader.unseal(decode(String(k ?? "")), decode(config.sealed));
}

// A refusal to say to the agent as it is.
export class Refused extends Error {}

const outOfDate =
	"This connector's keys are out of date: the account moved to new keys. On one of the account's computers, run `omacloud agents connect` again.";

function parts(path) {
	const names = String(path ?? "")
		.split("/")
		.filter((name) => name && name !== ".");
	if (names.some((name) => name === ".." || name.startsWith("."))) throw new Refused("A path here has no `..` and nothing hidden.");
	return names;
}

// Whether `names` is in a granted folder, or on the way to one (so the
// folders above a granted one can be listed down to it, and nothing else).
function reach(folders, names) {
	let within = false;
	let toward = false;
	for (const folder of folders) {
		const granted = folder.split("/").filter(Boolean);
		const shared = Math.min(granted.length, names.length);
		if (granted.slice(0, shared).join("/") !== names.slice(0, shared).join("/")) continue;
		if (names.length >= granted.length) within = true;
		else toward = true;
	}
	return { within, toward };
}

export class Files {
	// `config` is the connector's settings (see `omacloud agents connect`)
	// with their sealed part opened (`unlock`); `wasm` the reader's module.
	constructor(config, wasm) {
		this.config = config;
		this.wasm = wasm;
		this.folders = config.folders ?? [];
		this.trees = new Map();
		this.indexes = new Set();
		this.at = 0;
	}

	async open() {
		if (this.bucket) return;
		const s = this.config.storage;
		const bucket = s.dropbox
			? new Dropbox({ appKey: s.dropbox.app_key, refreshToken: s.dropbox.refresh_token, root: s.dropbox.root })
			: new Bucket({ endpoint: s.endpoint, bucket: s.bucket, root: s.root, region: s.region, accessKeyId: s.key, secretAccessKey: s.secret });
		await reader.loadModule(this.wasm);
		reader.openKey(JSON.stringify(this.config.key));
		try {
			reader.file(await bucket.read("config"));
		} catch {
			throw new Refused(outOfDate);
		}
		this.bucket = bucket;
	}

	async blob(id) {
		const at = reader.locate(id);
		return reader.blob(id, await this.bucket.read(`data/${at.pack.slice(0, 2)}/${at.pack}`, at.offset, at.length));
	}

	async tree(id) {
		if (!this.trees.has(id)) {
			if (this.trees.size >= TREES_KEPT) this.trees.clear();
			this.trees.set(id, JSON.parse(text.decode(await this.blob(id))).nodes);
		}
		return this.trees.get(id);
	}

	// The snapshots, newest first by when the storage says each was written.
	async written() {
		return (await this.bucket.listed("snapshots")).sort((a, b) => b.modified.localeCompare(a.modified));
	}

	async snapshot(name) {
		return { name, ...reader.file(await this.bucket.read(`snapshots/${name}`)) };
	}

	// The version the account stands on, looked for again once a while has passed.
	async latest() {
		await this.open();
		if (this.root && Date.now() - this.at < FRESH_MS) return this.root;
		let written;
		try {
			written = await this.written();
		} catch (e) {
			throw new Refused(`The storage didn't answer: ${e.message}`);
		}
		if (!written.length) throw new Refused(outOfDate);
		if (written[0].name !== this.newest) {
			const fresh = (await this.bucket.list("index")).filter((name) => !this.indexes.has(name));
			await Promise.all(fresh.map(async (name) => (reader.addIndex(await this.bucket.read(`index/${name}`)), this.indexes.add(name))));
			const cut = written[Math.min(NEWEST, written.length) - 1].modified;
			const names = written.filter((file, i) => i < NEWEST || file.modified === cut).map((file) => file.name);
			const snapshots = await Promise.all(names.map((name) => this.snapshot(name)));
			this.root = snapshots.reduce((a, b) => (a && a.time > b.time ? a : b), null);
			this.newest = written[0].name;
		}
		this.at = Date.now();
		return this.root;
	}

	// The tree a request is about: the newest, or the version named (the
	// start of a snapshot's id, from `versions`).
	async rootOf(version) {
		const latest = await this.latest();
		if (!version) return latest.tree;
		const matching = (await this.written()).filter((file) => file.name.startsWith(String(version)));
		if (matching.length !== 1) throw new Refused(`No one version starts with ${version}. \`list_versions\` gives them.`);
		return (await this.snapshot(matching[0].name)).tree;
	}

	// The node at `path`, and its names; refused outside what was granted.
	async find(path, version) {
		const names = parts(path);
		const { within, toward } = reach(this.folders, names);
		if (!within && !toward) throw new Refused(`${names.join("/") || "That"} is outside the folders this connector was given: ${this.folders.join(", ") || "none"}.`);
		let nodes = await this.tree(await this.rootOf(version));
		let node = { type: "dir", name: "" };
		for (const name of names) {
			if (node.type !== "dir") throw new Refused(`${names.join("/")} isn't there.`);
			node = nodes.find((n) => n.name === name);
			if (!node) throw new Refused(`${names.join("/")} isn't there.`);
			if (node.type === "dir") nodes = await this.tree(node.subtree);
		}
		return { names, node, nodes: node.type === "dir" ? nodes : null, within };
	}

	async list(path, version) {
		const { names, node, nodes } = await this.find(path, version);
		if (node.type !== "dir") throw new Refused(`${names.join("/")} is a file: read it with \`read_file\`.`);
		return nodes
			.filter((n) => !n.name.startsWith("."))
			.filter((n) => {
				const at = reach(this.folders, [...names, n.name]);
				return at.within || (at.toward && n.type === "dir");
			})
			.map((n) => ({ name: n.name, type: n.type === "dir" ? "folder" : "file", size: n.size ?? 0, modified: n.mtime ?? "" }));
	}

	// A file's bytes from `offset`, at most `length` of them, and its whole size.
	async read(path, offset = 0, length = Infinity, version) {
		const { names, node, within } = await this.find(path, version);
		if (node.type === "dir") throw new Refused(`${names.join("/")} is a folder: list it with \`list_files\`.`);
		if (!within) throw new Refused(`${names.join("/")} is outside the folders this connector was given.`);
		const chunks = [];
		let seen = 0;
		let taken = 0;
		for (const id of node.content ?? []) {
			if (taken >= length) break;
			// the index doesn't say how long a blob is once opened, so the ones
			// before the offset are read too
			const bytes = await this.blob(id);
			const from = Math.max(0, offset - seen);
			const part = bytes.subarray(from, Math.min(bytes.length, from + (length - taken)));
			seen += bytes.length;
			if (part.length) (chunks.push(part), (taken += part.length));
		}
		const out = new Uint8Array(taken);
		let at = 0;
		for (const chunk of chunks) (out.set(chunk, at), (at += chunk.length));
		return { bytes: out, size: node.size ?? taken, modified: node.mtime ?? "" };
	}

	// Files under `path` whose name holds `query`, or (text files, small ones,
	// while time lasts) a line that does. `done` is false if it stopped early.
	async search(query, path, { until, most = 60 } = {}) {
		const wanted = String(query).toLowerCase();
		if (!wanted) throw new Refused("Say what to look for.");
		const start = await this.find(path);
		if (start.node.type !== "dir") throw new Refused(`${start.names.join("/")} is a file.`);
		const found = [];
		let done = true;
		const visit = async (names, nodes) => {
			for (const n of nodes) {
				if (found.length >= most || Date.now() > until) return void (done = false);
				if (n.name.startsWith(".")) continue;
				const here = [...names, n.name];
				const at = reach(this.folders, here);
				if (n.type === "dir") {
					if (at.within || at.toward) await visit(here, await this.tree(n.subtree));
					continue;
				}
				if (!at.within) continue;
				const where = here.join("/");
				if (n.name.toLowerCase().includes(wanted)) found.push({ path: where });
				else if ((n.size ?? 0) <= 256 * 1024 && /\.(md|txt|csv|json|toml|ya?ml|org|rst|tex|html?|xml|ini|conf|log|sh|py|js|ts|rs|go|c|h|java|rb|lua|sql)$/i.test(n.name)) {
					const lines = text.decode((await this.read(where)).bytes).split("\n");
					const line = lines.findIndex((l) => l.toLowerCase().includes(wanted));
					if (line >= 0) found.push({ path: where, line: line + 1, text: lines[line].trim().slice(0, 200) });
				}
			}
		};
		await visit(start.names, start.nodes);
		return { found, done };
	}

	// The newest versions kept, each with when it was made and by which computer.
	async versions(most = 20) {
		await this.latest();
		const written = (await this.written()).slice(0, most);
		const snapshots = await Promise.all(written.map((file) => this.snapshot(file.name)));
		return snapshots
			.sort((a, b) => (a.time < b.time ? 1 : -1))
			.map((s) => ({ version: s.name.slice(0, 8), made: s.time, by: s.hostname ?? "" }));
	}

	// Leave a file for the account's computers to take in, as the browser
	// page does: signed with this connector's key, which a computer vouched
	// for with the folders it may write to, and sealed with the repository's.
	async write(path, content) {
		const upload = this.config.upload;
		if (!this.config.write || !upload) throw new Refused("This connector was set up to read only.");
		const names = parts(path);
		if (!names.length || !reach(this.folders, names).within) throw new Refused(`${names.join("/") || "That"} is outside the folders this connector was given.`);
		await this.open();
		const seed = Uint8Array.from(atob(upload.seed.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0));
		const sealed = reader.upload(seed, JSON.stringify(upload.endorsement), names.join("/"), Date.now(), random(16), content);
		const inbox = upload.inbox.replace(/^\/+|\/+$/g, "");
		await this.bucket.put(`${inbox}/${hex(random(12))}`, sealed);
		// one read tells a computer something is waiting
		await this.bucket.put(`${inbox}/changed`, random(16));
		return names.join("/");
	}
}
