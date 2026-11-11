// Omacloud Drive in a browser, laid out as iCloud Drive is on the web. Your
// files are read here from your own bucket. The bucket's key and the
// repository's password stay in this page's memory: they go nowhere but into
// requests to your bucket, and are gone when the tab closes or you lock it.

import * as reader from "./reader.js";
import { Bucket } from "./s3.js";

const $ = (id) => document.getElementById(id);
const say = (words) => ($("status").textContent = words);

function el(tag, props = {}, ...children) {
	const node = Object.assign(document.createElement(tag), props);
	node.append(...children);
	return node;
}

// our own drawings, so nothing is fetched for them
const FOLDER =
	'<svg viewBox="0 0 64 64" aria-hidden="true"><path class="folder-back" d="M6 14a4 4 0 0 1 4-4h14l6 6h24a4 4 0 0 1 4 4v6H6z"/><path class="folder-front" d="M6 22h52v26a4 4 0 0 1-4 4H10a4 4 0 0 1-4-4z"/></svg>';
const sheet = (kind) =>
	`<svg viewBox="0 0 64 64" aria-hidden="true"><path class="sheet" d="M16 6h22l12 12v36a4 4 0 0 1-4 4H16a4 4 0 0 1-4-4V10a4 4 0 0 1 4-4z"/><path class="sheet" d="M38 6v12h12"/><text class="sheet-kind" x="31" y="45">${kind}</text></svg>`;

// how a file can be looked at here, by its name's ending
const KINDS = {
	image: { png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif", webp: "image/webp", avif: "image/avif", svg: "image/svg+xml" },
	video: { mp4: "video/mp4", webm: "video/webm", mov: "video/quicktime" },
	audio: { mp3: "audio/mpeg", m4a: "audio/mp4", ogg: "audio/ogg", wav: "audio/wav", flac: "audio/flac" },
	pdf: { pdf: "application/pdf" },
	text: Object.fromEntries(
		"txt md json csv log toml yaml yml sh rs py js ts html css xml ini conf lua".split(" ").map((e) => [e, "text/plain"]),
	),
};
const ending = (name) => (name.includes(".") ? name.split(".").pop().toLowerCase() : "");
function kindOf(name) {
	const end = ending(name);
	for (const [kind, types] of Object.entries(KINDS)) if (types[end]) return { kind, type: types[end] };
	return { kind: "other", type: "application/octet-stream" };
}

// more than this isn't read just to show it
const LOOK_LIMIT = 200e6;
const THUMB_LIMIT = 8e6;
const TEXT_LIMIT = 1e6;

let bucket;
// the folders opened so far, from the top: [{ name, tree }]
let trail = [];
let nodes = [];
let selected = null;
const trees = new Map();
const thumbs = new Map();

const kept = (name, fallback) => {
	try {
		return localStorage.getItem(name) || fallback;
	} catch {
		return fallback;
	}
};
const keep = (name, value) => {
	try {
		localStorage.setItem(name, value);
	} catch {}
};
let view = kept("view", "grid");
let sort = kept("sort", "name");

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

// a file's content, in order; `progress` hears how far along it is
async function readFile(node, progress) {
	const ids = node.content || [];
	const parts = [];
	for (const [i, id] of ids.entries()) {
		progress?.(i / ids.length);
		parts.push(await readBlob(id));
	}
	return new Blob(parts, { type: kindOf(node.name).type });
}

// what `omacloud export` prints, taken apart
function pasted(words) {
	const quoted = (name) => words.match(new RegExp(`${name}='([^']*)'`))?.[1];
	const repo = words.match(/-r\s+s3:(\S+)/)?.[1];
	if (!repo) return null;
	const url = new URL(repo);
	const [bucketName, ...root] = url.pathname.split("/").filter(Boolean);
	return {
		endpoint: url.origin,
		bucket: bucketName,
		root: root.join("/"),
		region: quoted("AWS_DEFAULT_REGION") || "auto",
		key: quoted("AWS_ACCESS_KEY_ID"),
		secret: quoted("AWS_SECRET_ACCESS_KEY"),
		password: quoted("RESTIC_PASSWORD"),
	};
}

async function connect(form) {
	const status = (words) => ($("signin-status").textContent = words);
	const field = (name) => form.elements[name].value.trim();
	bucket = new Bucket({
		endpoint: field("endpoint"),
		bucket: field("bucket"),
		root: field("root"),
		region: field("region"),
		accessKeyId: field("key"),
		secretAccessKey: form.elements.secret.value,
	});
	status("Opening…");
	// let that show before the password is worked out, which takes a moment
	await new Promise((shown) => setTimeout(shown, 30));
	await reader.load("omacloud_web.wasm");
	const keys = await bucket.list("keys");
	if (!keys.length) throw new Error("No Omacloud files here: check the bucket and its folder.");
	let opened = false;
	for (const key of keys) {
		try {
			reader.open(form.elements.password.value, await bucket.read(`keys/${key}`));
			opened = true;
			break;
		} catch {}
	}
	if (!opened) throw new Error("The password doesn't open these files.");
	reader.file(await bucket.read("config"));

	const indexes = await bucket.list("index");
	status("Reading the index…");
	await each(indexes, async (name) => reader.addIndex(await bucket.read(`index/${name}`)));

	// the newest snapshot is the account as it stands
	const names = await bucket.list("snapshots");
	status("Finding your files…");
	const snapshots = await each(names, async (name) => reader.file(await bucket.read(`snapshots/${name}`)));
	const latest = snapshots.reduce((a, b) => (a && a.time > b.time ? a : b), null);
	if (!latest) throw new Error("Nothing has been synced yet.");
	trail = [{ name: "Omacloud Drive", tree: latest.tree }];
	form.reset();
	$("pasted").value = "";
	status("");
	$("signin").hidden = true;
	$("drive").hidden = false;
	await sidebar();
	await open();
}

// Omacloud's own files (settings, package lists) stay out of sight
const visible = (list) => list.filter((n) => n.name !== ".omacloud");

async function sidebar() {
	const top = visible(await readTree(trail[0].tree)).filter((n) => n.type === "dir");
	top.sort((a, b) => a.name.localeCompare(b.name));
	const place = (name, go) => {
		const button = el("button", { type: "button" });
		button.innerHTML = FOLDER;
		button.append(el("span", { textContent: name }));
		button.dataset.name = name;
		button.addEventListener("click", () => act(go));
		return button;
	};
	$("sidebar").replaceChildren(
		place("Browse", () => ((trail = trail.slice(0, 1)), open())),
		el("h2", { textContent: "Folders" }),
		...top.map((n) => place(n.name, () => ((trail = [trail[0], { name: n.name, tree: n.subtree }]), open()))),
	);
}

async function open() {
	say("Loading…");
	nodes = visible(await readTree(trail[trail.length - 1].tree));
	selected = null;
	$("filter").value = "";
	const here = trail.length === 1 ? "Browse" : trail[1].name;
	for (const b of $("sidebar").querySelectorAll("button")) b.setAttribute("aria-current", b.dataset.name === here);
	$("path").replaceChildren(
		...trail.map((step, i) =>
			i === trail.length - 1
				? el("span", { textContent: step.name })
				: el("button", {
						type: "button",
						textContent: step.name,
						onclick: () => ((trail = trail.slice(0, i + 1)), act(open)),
					}),
		),
	);
	show();
}

const sizes = ["bytes", "KB", "MB", "GB", "TB"];
function human(bytes) {
	let i = 0;
	while (bytes >= 1000 && i < sizes.length - 1) (bytes /= 1000), i++;
	return `${i ? bytes.toFixed(1) : bytes} ${sizes[i]}`;
}
const when = (node) =>
	node.mtime ? new Date(node.mtime).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" }) : "";

const orders = {
	name: (a, b) => a.name.localeCompare(b.name, undefined, { numeric: true }),
	date: (a, b) => (b.mtime || "").localeCompare(a.mtime || ""),
	size: (a, b) => (b.size || 0) - (a.size || 0),
};

// small pictures show themselves, a few at a time, once they scroll into view
const waiting = [];
let drawing = 0;
async function drawNext() {
	if (drawing >= 3 || !waiting.length) return;
	drawing++;
	const { node, art } = waiting.shift();
	try {
		const key = node.content.join();
		if (!thumbs.has(key)) thumbs.set(key, URL.createObjectURL(await readFile(node)));
		if (art.isConnected) art.replaceChildren(el("img", { src: thumbs.get(key), alt: "", loading: "lazy" }));
	} catch {}
	drawing--;
	drawNext();
}
const seen = new IntersectionObserver((entries) => {
	for (const entry of entries) {
		if (!entry.isIntersecting) continue;
		seen.unobserve(entry.target);
		waiting.push({ node: entry.target.node, art: entry.target });
		drawNext();
	}
});

function show() {
	const find = $("filter").value.trim().toLowerCase();
	const shown = nodes.filter((n) => !find || n.name.toLowerCase().includes(find));
	shown.sort((a, b) => (b.type === "dir") - (a.type === "dir") || orders[sort](a, b));
	waiting.length = 0;
	seen.disconnect();

	const items = $("items");
	items.className = view;
	$("as-grid").setAttribute("aria-pressed", view === "grid");
	$("as-list").setAttribute("aria-pressed", view === "list");
	$("sort").value = sort;
	items.replaceChildren(
		...shown.map((node) => {
			const art = el("div", { className: "art" });
			const folder = node.type === "dir";
			art.innerHTML = folder ? FOLDER : sheet(ending(node.name).slice(0, 4).toUpperCase());
			if (!folder && kindOf(node.name).kind === "image" && (node.size || 0) <= THUMB_LIMIT && node.content?.length) {
				art.node = node;
				seen.observe(art);
			}
			const detail = folder ? "" : human(node.size || 0);
			const row = el(
				"div",
				{ className: "item", role: "option" },
				art,
				el("div", { className: "name", textContent: node.name, title: node.name }),
			);
			if (view === "list") row.append(el("div", { className: "detail when", textContent: when(node) }));
			row.append(el("div", { className: "detail", textContent: view === "list" ? detail : detail || when(node) }));
			row.setAttribute("aria-selected", node === selected);
			// a tap opens; with a mouse, a click chooses and a second opens
			row.addEventListener("click", () => {
				if (matchMedia("(pointer: coarse)").matches || node === selected) return act(() => activate(node));
				choose(node, row);
			});
			row.addEventListener("dblclick", () => act(() => activate(node)));
			return row;
		}),
	);
	if (!shown.length) {
		items.append(el("p", { className: "empty", textContent: find ? "Nothing here by that name." : "This folder is empty." }));
	}
	$("download").disabled = !selected || selected.type !== "file";
	say(`${shown.length} ${shown.length === 1 ? "item" : "items"}`);
}

function choose(node, row) {
	selected = node;
	for (const other of $("items").children) other.setAttribute?.("aria-selected", other === row);
	$("download").disabled = node.type !== "file";
}

async function activate(node) {
	if (node.type === "dir") {
		trail.push({ name: node.name, tree: node.subtree });
		return open();
	}
	if (node.type === "file") return look(node);
}

function save(blob, name) {
	const link = el("a", { href: URL.createObjectURL(blob), download: name });
	link.click();
	setTimeout(() => URL.revokeObjectURL(link.href), 60_000);
}

async function download(node) {
	const blob = await readFile(node, (done) => say(`Downloading ${node.name} (${Math.round(done * 100)}%)…`));
	save(blob, node.name);
	say(`Downloaded ${node.name}.`);
}

// a look at one file, over the folder, as Quick Look gives
let looking = null;
async function look(node) {
	const { kind } = kindOf(node.name);
	const body = $("look-body");
	const none = (words) => {
		const art = el("div", { className: "art" });
		art.innerHTML = sheet(ending(node.name).slice(0, 4).toUpperCase());
		body.replaceChildren(el("div", { className: "none" }, art, el("p", { textContent: words })));
	};
	looking = { node, url: null };
	const mine = looking;
	$("look-name").textContent = node.name;
	none(`${human(node.size || 0)} · ${when(node)}`);
	if (!$("look").open) $("look").showModal();
	if (kind === "other" || (node.size || 0) > LOOK_LIMIT) return;
	if (kind === "text" && (node.size || 0) > TEXT_LIMIT) return;

	none("Loading…");
	const blob = await readFile(node);
	if (looking !== mine) return;
	if (kind === "text") return body.replaceChildren(el("pre", { textContent: await blob.text() }));
	mine.url = URL.createObjectURL(blob);
	const shows = {
		image: () => el("img", { src: mine.url, alt: node.name }),
		video: () => el("video", { src: mine.url, controls: true, autoplay: true }),
		audio: () => el("audio", { src: mine.url, controls: true, autoplay: true }),
		pdf: () => el("iframe", { src: mine.url, title: node.name }),
	};
	body.replaceChildren(shows[kind]());
}

$("look").addEventListener("close", () => {
	if (looking?.url) URL.revokeObjectURL(looking.url);
	looking = null;
	$("look-body").replaceChildren();
});
$("look-close").addEventListener("click", () => $("look").close());
$("look-download").addEventListener("click", () => looking && act(() => download(looking.node)));

async function act(work) {
	try {
		await work();
	} catch (e) {
		($("drive").hidden ? $("signin-status") : $("status")).textContent = e.message;
	}
}

$("pasted").addEventListener("input", (event) => {
	const found = pasted(event.target.value);
	if (!found) return;
	const form = $("connect");
	for (const [name, value] of Object.entries(found)) form.elements[name].value = value || "";
});
// a field left empty while they're folded away: show them
$("connect").addEventListener("invalid", () => ($("byhand").open = true), true);
$("connect").addEventListener("submit", (event) => {
	event.preventDefault();
	act(() => connect(event.target));
});
// forgetting everything is a fresh page: nothing was kept anywhere else
$("lock").addEventListener("click", () => location.reload());
$("filter").addEventListener("input", show);
$("sort").addEventListener("change", (event) => ((sort = event.target.value), keep("sort", sort), show()));
$("as-grid").addEventListener("click", () => ((view = "grid"), keep("view", view), show()));
$("as-list").addEventListener("click", () => ((view = "list"), keep("view", view), show()));
$("download").addEventListener("click", () => selected && act(() => download(selected)));
$("items").addEventListener("keydown", (event) => {
	if (event.key === "Enter" && selected) act(() => activate(selected));
	if (event.key === "Backspace" && trail.length > 1) (trail.pop(), act(open));
});
