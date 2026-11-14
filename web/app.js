// Omacloud Drive in a browser, laid out as iCloud Drive is on the web. Your
// files are read here from your own bucket. The bucket's key and the
// repository's password stay in this page's memory: they go nowhere but into
// requests to your bucket, and are gone when the tab closes or you lock it.

import { account, linked, pasted } from "./paste.js";
import * as reader from "./reader.js";
import { Dropbox } from "./dropbox.js";
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

const icon = (path) => `<svg viewBox="0 0 24 24" aria-hidden="true" class="line"><path d="${path}"/></svg>`;
const CLOCK = icon("M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18zm0 4v5l3.5 2");
const PHOTO = icon("M4 5h16v14H4zM4 16l4.5-4.5 3.5 3.5 3-3 5 5M9 9.5a1 1 0 1 0 0-.01");
const TRASH = icon("M5 7h14M10 7V4h4v3M7 7l1 13h8l1-13M10 11v6M14 11v6");

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

// how many of the snapshots written last are looked at for the newest
const NEWEST = 5;

// more than this isn't read just to show it
const LOOK_LIMIT = 200e6;
const THUMB_LIMIT = 8e6;
const TEXT_LIMIT = 1e6;

let bucket;
// what a link or a pasted code said, when it names a Dropbox: the form has
// no fields for one
let dropbox = null;
// what lets this session upload, if a drive link brought it (see paste.js)
let uploader = null;
// files sent from here that no version has yet: [{ name, where, size, mtime }]
let pending = [];
// the snapshot and index files read so far, by name
let latestName = "";
const indexesRead = new Set();
// every snapshot the bucket lists, newest written first: [{ name, modified }]
let written = [];
// the folders opened so far, from the top: [{ name, tree }]
let trail = [];
// what's open instead of a folder, if anything: "recents", "photos" or "deleted"
let place = null;
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
// a place lists files from everywhere: newest first until asked otherwise
let placeSort = "date";

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

async function connect(form) {
	const status = (words) => ($("signin-status").textContent = words);
	const field = (name) => form.elements[name].value.trim();
	bucket = dropbox
		? new Dropbox(dropbox)
		: new Bucket({
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
	if (!keys.length) throw new Error(dropbox ? "No Omacloud files in this Dropbox." : "No Omacloud files here: check the bucket and its folder.");
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

	status("Reading the index…");
	await readIndexes();

	// The newest snapshot is the account as it stands. An account keeps a
	// month of them, so only the few written last are read: the newest by
	// what each says is among them. (Any written at the same moment as the
	// last of those are read too, since the listing can't order them.)
	status("Finding your files…");
	const latest = await newest();
	if (!latest) throw new Error("Nothing has been synced yet.");
	trail = [{ name: "Omacloud Drive", tree: latest.tree }];
	form.reset();
	$("pasted").value = "";
	status("");
	$("signin").hidden = true;
	await sidebar();
	home();
}

// Take in the index files not read yet.
async function readIndexes() {
	const fresh = (await bucket.list("index")).filter((name) => !indexesRead.has(name));
	await each(fresh, async (name) => (reader.addIndex(await bucket.read(`index/${name}`)), indexesRead.add(name)));
}

async function newest() {
	written = (await bucket.listed("snapshots")).sort((a, b) => b.modified.localeCompare(a.modified));
	const cut = written[Math.min(NEWEST, written.length) - 1]?.modified;
	const names = written.filter((file, i) => i < NEWEST || file.modified === cut).map((file) => file.name);
	const snapshots = await each(names, async (name) => ({ name, ...reader.file(await bucket.read(`snapshots/${name}`)) }));
	const latest = snapshots.reduce((a, b) => (a && a.time > b.time ? a : b), null);
	latestName = latest?.name ?? "";
	return latest;
}

// A newer version than the one on show, if a computer pushed one: the page
// moves to it, staying in the folder it was in while that still exists.
async function refresh() {
	const before = latestName;
	const latest = await newest();
	if (!latest || latest.name === before) return false;
	await readIndexes();
	everything = undefined;
	deletedFiles = undefined;
	const names = trail.slice(1).map((step) => step.name);
	trail = [{ name: "Omacloud Drive", tree: latest.tree }];
	for (const name of names) {
		const next = (await readTree(trail.at(-1).tree)).find((n) => n.name === name && n.type === "dir");
		if (!next) break;
		trail.push({ name, tree: next.subtree });
	}
	const have = new Set((await allFiles()).map((file) => `${file.where}/${file.name}`));
	pending = pending.filter((file) => !have.has(`${file.where}/${file.name}`));
	await sidebar();
	if ($("drive").hidden) return true;
	await (place ? openPlace(place) : open());
	return true;
}

// The folder on show, as a path in the account: "Work/Reports".
const folderPath = () =>
	trail
		.slice(1)
		.map((step) => step.name)
		.join("/");

const UPLOAD_LIMIT = 100e6;

// Send files to the account: each is signed with this session's key, sealed
// with the repository's, and left in the inbox beside it. One of the
// person's computers takes it in and syncs it, and then it shows here as
// any file does.
async function send(files, where = folderPath()) {
	if (!uploader || !where) throw new Error("Open one of your folders to upload into it.");
	const random = (n) => crypto.getRandomValues(new Uint8Array(n));
	const hex = (bytes) => [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
	let sent = 0;
	for (const file of files) {
		if (file.size > UPLOAD_LIMIT) throw new Error(`${file.name} is over ${human(UPLOAD_LIMIT)}: too big to send from a browser.`);
		say(`Uploading ${file.name}…`);
		const content = new Uint8Array(await file.arrayBuffer());
		const sealed = reader.upload(uploader.seed, uploader.endorsement, `${where}/${file.name}`, file.lastModified, random(16), content);
		await bucket.put(`${uploader.inbox}/${hex(random(12))}`, sealed);
		pending.push({ type: "file", name: file.name, where, size: file.size, mtime: new Date(file.lastModified).toISOString(), pending: true });
		sent++;
	}
	// one read tells a computer something is waiting
	await bucket.put(`${uploader.inbox}/changed`, random(16));
	show();
	say(`Sent ${sent} ${sent === 1 ? "file" : "files"}. ${sent === 1 ? "It shows" : "They show"} here once one of your computers has taken ${sent === 1 ? "it" : "them"} in.`);
	watchForVersion();
}

// After an upload, look for the version that has it: every so often, for a
// few minutes.
let watching = 0;
function watchForVersion() {
	const mine = ++watching;
	let tries = 0;
	const look = async () => {
		if (mine !== watching || !pending.length || ++tries > 40) return;
		try {
			await refresh();
		} catch {}
		setTimeout(look, 15_000);
	};
	setTimeout(look, 15_000);
}

// A deleted file goes back where it was the same way: its last version is
// read here and sent as an upload.
async function putBack(node) {
	if ((node.size || 0) > UPLOAD_LIMIT) throw new Error(`${node.name} is over ${human(UPLOAD_LIMIT)}: put it back from one of your computers.`);
	say(`Reading ${node.name}…`);
	const blob = await readFile(node);
	const file = new File([blob], node.name, { lastModified: node.mtime ? Date.parse(node.mtime) : Date.now() });
	await send([file], node.where);
	say(`Sent ${node.name} back to ${node.where}. It's there once one of your computers has taken it in.`);
}
$("putback").addEventListener("click", () => selected && act(() => putBack(selected)));

$("upload").addEventListener("click", () => $("files").click());
$("files").addEventListener("change", (event) => {
	const files = [...event.target.files];
	event.target.value = "";
	if (files.length) act(() => send(files));
});
const canDrop = () => uploader && !place && trail.length > 1;
$("items").addEventListener("dragover", (event) => {
	if (!canDrop()) return;
	event.preventDefault();
	$("items").classList.add("dropping");
});
$("items").addEventListener("dragleave", () => $("items").classList.remove("dropping"));
$("items").addEventListener("drop", (event) => {
	$("items").classList.remove("dropping");
	if (!canDrop()) return;
	event.preventDefault();
	const files = [...event.dataTransfer.files];
	if (files.length) act(() => send(files));
});

// Where signing in lands, as iCloud's site does: what there is to open.
function home() {
	$("drive").hidden = true;
	$("home").hidden = false;
}
function leaveHome() {
	$("home").hidden = true;
	$("drive").hidden = false;
}
$("home-drive").addEventListener("click", () => (leaveHome(), act(() => ((trail = trail.slice(0, 1)), open()))));
$("home-photos").addEventListener("click", () => (leaveHome(), act(() => openPlace("photos"))));
$("home-recents").addEventListener("click", () => (leaveHome(), act(() => openPlace("recents"))));
$("home-deleted").addEventListener("click", () => (leaveHome(), act(() => openPlace("deleted"))));
$("go-home").addEventListener("click", home);

// Every file of the account, each with the folder it's in (`where`): read
// once, when a place that shows files from everywhere is first opened.
let everything;
function allFiles() {
	everything ??= filesUnder(trail[0].tree, "", true);
	return everything;
}
async function filesUnder(tree, where, top = false) {
	const list = await readTree(tree);
	const here = top ? visible(list) : list;
	const files = here.filter((n) => n.type === "file").map((n) => ({ ...n, where }));
	const inside = await each(
		here.filter((n) => n.type === "dir"),
		(n) => filesUnder(n.subtree, where ? `${where}/${n.name}` : n.name),
		4,
	);
	return files.concat(...inside);
}

// What `was` holds that `is` doesn't, by name all the way down: the files
// deleted between two versions. A folder that's the same in both (the same
// tree) is never opened.
async function gone(was, is, where, top, out) {
	if (was === is) return;
	const before = await readTree(was);
	const after = new Map((is ? await readTree(is) : []).map((n) => [n.name, n]));
	for (const node of top ? visible(before) : before) {
		const now = after.get(node.name);
		const path = where ? `${where}/${node.name}` : node.name;
		if (node.type === "dir") {
			if (now?.type === "dir") await gone(node.subtree, now.subtree, path, false, out);
			else for (const file of await filesUnder(node.subtree, path)) out.push(file);
		} else if (node.type === "file" && !now) out.push({ ...node, where });
	}
}

// Whether the newest version still has a file at this path.
async function stillThere(where, name) {
	let tree = trail[0].tree;
	for (const part of where ? where.split("/") : []) {
		tree = (await readTree(tree)).find((n) => n.name === part && n.type === "dir")?.subtree;
		if (!tree) return false;
	}
	return (await readTree(tree)).some((n) => n.name === name);
}

// Files deleted in the time the account keeps versions for (a month): each
// from the last version that had it, with when the next one came without it.
let deletedFiles;
function allDeleted() {
	deletedFiles ??= (async () => {
		const snapshots = await each(written, async (file) => reader.file(await bucket.read(`snapshots/${file.name}`)));
		snapshots.sort((a, b) => a.time.localeCompare(b.time));
		const found = new Map();
		for (let i = 0; i + 1 < snapshots.length; i++) {
			const out = [];
			await gone(snapshots[i].tree, snapshots[i + 1].tree, "", true, out);
			for (const file of out) found.set(`${file.where}/${file.name}`, { ...file, deleted: snapshots[i + 1].time });
		}
		const kept = [];
		for (const file of found.values()) if (!(await stillThere(file.where, file.name))) kept.push(file);
		return kept;
	})();
	return deletedFiles;
}

const PLACES = {
	recents: { title: "Recents", empty: "Nothing here yet.", files: async () => (await allFiles()).toSorted(orders.date).slice(0, 200) },
	photos: {
		title: "Photos",
		empty: "No pictures in your synced folders.",
		files: async () => (await allFiles()).filter((n) => kindOf(n.name).kind === "image"),
	},
	deleted: { title: "Recently Deleted", empty: "Nothing was deleted in the last 30 days.", files: allDeleted },
};

async function openPlace(name) {
	place = name;
	placeSort = "date";
	say("Loading…");
	mark(`place:${name}`);
	$("path").replaceChildren(el("span", { textContent: PLACES[name].title }));
	$("items").replaceChildren();
	const files = await PLACES[name].files();
	if (place !== name) return;
	nodes = files;
	selected = null;
	$("filter").value = "";
	show();
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
	const special = (name, drawing) => {
		const button = place(PLACES[name].title, () => openPlace(name));
		button.querySelector("svg").outerHTML = drawing;
		// a folder may be called Photos too
		button.dataset.name = `place:${name}`;
		return button;
	};
	$("sidebar").replaceChildren(
		special("recents", CLOCK),
		place("Browse", () => ((trail = trail.slice(0, 1)), open())),
		special("photos", PHOTO),
		el("h2", { textContent: "Folders" }),
		...top.map((n) => place(n.name, () => ((trail = [trail[0], { name: n.name, tree: n.subtree }]), open()))),
		el("h2", { textContent: "" }),
		special("deleted", TRASH),
	);
}

// which of the sidebar's places is open
function mark(name) {
	for (const b of $("sidebar").querySelectorAll("button")) b.setAttribute("aria-current", b.dataset.name === name);
}

async function open() {
	place = null;
	say("Loading…");
	nodes = visible(await readTree(trail[trail.length - 1].tree));
	selected = null;
	$("filter").value = "";
	mark(trail.length === 1 ? "Browse" : trail[1].name);
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
	// what was sent from here into this folder shows at once, as waiting
	const sent = place ? [] : pending.filter((file) => file.where === folderPath() && !nodes.some((n) => n.name === file.name));
	const shown = [...nodes, ...sent].filter((n) => !find || n.name.toLowerCase().includes(find));
	// uploads go into a folder of the account, never beside them
	$("upload").hidden = !uploader || !!place || trail.length < 2;
	// a deleted file goes back to a folder, so one from the top level can't
	$("putback").hidden = !uploader || place !== "deleted";
	$("putback").disabled = !selected?.where;
	if (place) shown.sort(orders[placeSort]);
	else shown.sort((a, b) => (b.type === "dir") - (a.type === "dir") || orders[sort](a, b));
	waiting.length = 0;
	seen.disconnect();

	const items = $("items");
	// pictures are looked at, not listed
	items.className = place === "photos" ? "grid photos" : view;
	$("as-grid").setAttribute("aria-pressed", view === "grid");
	$("as-list").setAttribute("aria-pressed", view === "list");
	$("sort").value = place ? placeSort : sort;
	items.replaceChildren(
		...shown.map((node) => {
			const art = el("div", { className: "art" });
			const folder = node.type === "dir";
			art.innerHTML = folder ? FOLDER : sheet(ending(node.name).slice(0, 4).toUpperCase());
			if (!folder && kindOf(node.name).kind === "image" && (node.size || 0) <= THUMB_LIMIT && node.content?.length) {
				art.node = node;
				seen.observe(art);
			}
			const detail = folder ? "" : node.pending ? "Waiting for a computer" : human(node.size || 0);
			// where a file from elsewhere is, or was
			const from = place ? node.where || "Omacloud Drive" : "";
			const row = el(
				"div",
				{ className: "item", role: "option" },
				art,
				el("div", { className: "name", textContent: node.name, title: from ? `${from}/${node.name}` : node.name }),
			);
			const date = place === "deleted" ? `Deleted ${when({ mtime: node.deleted })}` : when(node);
			if (view === "list" && place !== "photos") {
				row.append(el("div", { className: "detail when", textContent: date }));
				row.append(el("div", { className: "detail", textContent: from || detail, title: from }));
			} else {
				const under = place === "deleted" ? `${date} · ${from}` : from || detail || date;
				row.append(el("div", { className: "detail", textContent: under }));
			}
			row.setAttribute("aria-selected", node === selected);
			if (node.pending) {
				row.classList.add("pending");
				return row;
			}
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
		const nothing = place ? PLACES[place].empty : "This folder is empty.";
		items.append(el("p", { className: "empty", textContent: find ? "Nothing here by that name." : nothing }));
	}
	$("download").disabled = !selected || selected.type !== "file";
	say(`${shown.length} ${shown.length === 1 ? "item" : "items"}`);
}

function choose(node, row) {
	selected = node;
	for (const other of $("items").children) other.setAttribute?.("aria-selected", other === row);
	$("download").disabled = node.type !== "file";
	$("putback").disabled = !node.where;
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

const fill = (form, found) => {
	for (const [name, value] of Object.entries(found)) if (form.elements[name]) form.elements[name].value = value || "";
};

// A drive link: one of the person's computers left the drive code in their
// bucket, sealed, for a few minutes, and the link says where and carries
// what opens it. Nothing is typed, and the address alone opens nothing later.
async function follow({ seed, address }) {
	const status = (words) => ($("signin-status").textContent = words);
	const again = "Make a new one on your computer: Open on a Phone in Omacloud, or omacloud drive-link.";
	status("Opening your link…");
	await reader.load("omacloud_web.wasm");
	let response;
	try {
		response = await fetch(address, { cache: "no-store" });
	} catch {
		throw new Error("The bucket didn't answer this page. Check that it allows this site to read it (its CORS rule).");
	}
	if (!response.ok) throw new Error(`This link has run out, or was closed. ${again}`);
	let found;
	try {
		found = account(reader.unseal(seed, new Uint8Array(await response.arrayBuffer())));
	} catch {}
	if (!found) throw new Error(`This link is damaged. ${again}`);
	fill($("connect"), found);
	uploader = found.upload;
	dropbox = found.dropbox ?? null;
	await connect($("connect"));
}

$("pasted").addEventListener("input", (event) => {
	const text = event.target.value.trim();
	const found = pasted(text);
	const form = $("connect");
	const status = (words) => ($("signin-status").textContent = words);
	if (!found) {
		status(text ? "That doesn't read as a drive code. Paste all of what omacloud drive-code prints, or enter it by hand." : "");
		return;
	}
	fill(form, found);
	dropbox = found.dropbox ?? null;
	// a Dropbox has nothing more to enter: open it
	if (dropbox) return found.password ? act(() => connect(form)) : status("Found your Dropbox, but not the password: paste the whole code.");
	const missing = !found.key || !found.secret || !found.password;
	status(
		missing
			? `Found ${found.bucket}, but not its key or the password: paste the whole code.`
			: `Found ${found.bucket} at ${new URL(found.endpoint).host}. Press Open.`,
	);
});
// a field left empty while they're folded away: show them
$("connect").addEventListener("invalid", () => ($("byhand").open = true), true);
$("connect").addEventListener("submit", (event) => {
	event.preventDefault();
	// typed by hand, it's a bucket
	if (!pasted($("pasted").value)?.dropbox) dropbox = null;
	act(() => connect(event.target));
});
// forgetting everything is a fresh page: nothing was kept anywhere else
$("lock").addEventListener("click", () => location.reload());
$("home-lock").addEventListener("click", () => location.reload());
$("filter").addEventListener("input", show);
$("filter").placeholder = "Search";
$("sort").addEventListener("change", (event) => {
	if (place) placeSort = event.target.value;
	else (sort = event.target.value), keep("sort", sort);
	show();
});
$("as-grid").addEventListener("click", () => ((view = "grid"), keep("view", view), show()));
$("as-list").addEventListener("click", () => ((view = "list"), keep("view", view), show()));
$("download").addEventListener("click", () => selected && act(() => download(selected)));
$("items").addEventListener("keydown", (event) => {
	if (event.key === "Enter" && selected) act(() => activate(selected));
	if (event.key === "Backspace" && trail.length > 1) (trail.pop(), act(open));
});

// The link is taken out of the address at once, so it isn't left on show or
// kept as this page's address in the tab's history.
const link = linked(location.hash);
if (location.hash) history.replaceState(null, "", location.pathname + location.search);
if (link) act(() => follow(link));
// another link followed in this same tab: start afresh with it
addEventListener("hashchange", () => linked(location.hash) && location.reload());
