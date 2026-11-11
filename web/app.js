// Your files, read in this browser from your own bucket. The bucket's key and
// the repository's password stay in this page's memory: they go nowhere but
// into requests to your bucket, and are gone when the tab closes.

import * as reader from "./reader.js";
import { Bucket } from "./s3.js";

const $ = (id) => document.getElementById(id);
const say = (words) => ($("status").textContent = words);

let bucket;
// the folders opened so far, from the top: [{ name, tree }]
let trail = [];
const trees = new Map();

// a few at once: a bucket answers many small reads faster side by side
async function each(items, work, atOnce = 8) {
	const results = new Array(items.length);
	let next = 0;
	const run = async () => {
		while (next < items.length) {
			const i = next++;
			results[i] = await work(items[i]);
		}
	};
	await Promise.all(Array.from({ length: Math.min(atOnce, items.length) }, run));
	return results;
}

async function readBlob(id) {
	const at = reader.locate(id);
	return reader.blob(id, await bucket.read(`data/${at.pack.slice(0, 2)}/${at.pack}`, at.offset, at.length));
}

async function readTree(id) {
	if (!trees.has(id)) trees.set(id, JSON.parse(new TextDecoder().decode(await readBlob(id))).nodes);
	return trees.get(id);
}

async function connect(form) {
	const field = (name) => form.elements[name].value.trim();
	bucket = new Bucket({
		endpoint: field("endpoint"),
		bucket: field("bucket"),
		root: field("root"),
		region: field("region"),
		accessKeyId: field("key"),
		secretAccessKey: form.elements.secret.value,
	});
	say("Opening…");
	await reader.load("omacloud_web.wasm");
	const keys = await bucket.list("keys");
	if (!keys.length) throw new Error("No repository here: check the bucket and its folder.");
	let opened = false;
	for (const key of keys) {
		try {
			reader.open(form.elements.password.value, await bucket.read(`keys/${key}`));
			opened = true;
			break;
		} catch {}
	}
	if (!opened) throw new Error("The password doesn't open this repository.");
	reader.file(await bucket.read("config"));

	const indexes = await bucket.list("index");
	say(`Reading the index (${indexes.length} files)…`);
	await each(indexes, async (name) => reader.addIndex(await bucket.read(`index/${name}`)));

	// the newest snapshot is the account as it stands
	const names = await bucket.list("snapshots");
	say(`Finding the latest version (${names.length})…`);
	const snapshots = await each(names, async (name) => reader.file(await bucket.read(`snapshots/${name}`)));
	const latest = snapshots.reduce((a, b) => (a && a.time > b.time ? a : b), null);
	if (!latest) throw new Error("Nothing has been synced yet.");
	trail = [{ name: "Home", tree: latest.tree }];
	$("connect").hidden = true;
	$("files").hidden = false;
	await show();
}

const sizes = ["bytes", "KB", "MB", "GB", "TB"];
function human(bytes) {
	let i = 0;
	while (bytes >= 1000 && i < sizes.length - 1) (bytes /= 1000), i++;
	return `${i ? bytes.toFixed(1) : bytes} ${sizes[i]}`;
}

async function show() {
	const here = trail[trail.length - 1];
	say("Loading…");
	// Omacloud's own files (settings, package lists) stay out of sight
	const nodes = (await readTree(here.tree)).filter((n) => n.name !== ".omacloud");
	nodes.sort((a, b) => (b.type === "dir") - (a.type === "dir") || a.name.localeCompare(b.name));

	const path = $("path");
	path.replaceChildren(
		...trail.map((step, i) => {
			const b = document.createElement("button");
			b.textContent = step.name;
			b.disabled = i === trail.length - 1;
			b.addEventListener("click", () => ((trail = trail.slice(0, i + 1)), act(show)));
			return b;
		}),
	);
	const list = $("list");
	list.replaceChildren(
		...nodes.map((node) => {
			const row = document.createElement("li");
			const name = document.createElement("button");
			name.textContent = node.type === "dir" ? `${node.name}/` : node.name;
			const detail = document.createElement("span");
			detail.textContent = node.type === "file" ? human(node.size || 0) : "";
			if (node.type === "dir") {
				name.addEventListener("click", () => (trail.push({ name: node.name, tree: node.subtree }), act(show)));
			} else if (node.type === "file") {
				name.addEventListener("click", () => act(() => download(node)));
			} else {
				name.disabled = true;
			}
			row.append(name, detail);
			return row;
		}),
	);
	say(nodes.length ? "" : "This folder is empty.");
}

async function download(node) {
	const ids = node.content || [];
	const parts = [];
	for (const [i, id] of ids.entries()) {
		say(`Downloading ${node.name} (${Math.round((i / ids.length) * 100)}%)…`);
		parts.push(await readBlob(id));
	}
	const link = document.createElement("a");
	link.href = URL.createObjectURL(new Blob(parts));
	link.download = node.name;
	link.click();
	setTimeout(() => URL.revokeObjectURL(link.href), 60_000);
	say(`Downloaded ${node.name}.`);
}

async function act(work) {
	try {
		await work();
	} catch (e) {
		say(e.message);
	}
}

$("connect").addEventListener("submit", (event) => {
	event.preventDefault();
	act(() => connect(event.target));
});
