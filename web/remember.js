// Staying signed in on this device. What opens the account (what a web code
// holds: the storage's key and the password to the files) is kept in this
// browser, encrypted. Where the browser can, the key to it comes from a
// passkey, so opening takes the device's fingerprint or face each time and
// nothing here opens it alone. Where it can't, the key is one this browser
// holds and won't hand out, and the device's own lock is what protects it.
//
// Nothing is kept unless asked for, and `forget` removes all of it.

const DB = "omacloud";
const STORE = "kept";
const RECORD = "account";
const SALT = new TextEncoder().encode("omacloud stay signed in 1");

const random = (n) => crypto.getRandomValues(new Uint8Array(n));
const hex = (bytes) => [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");

function database() {
	return new Promise((resolve, reject) => {
		const request = indexedDB.open(DB, 1);
		request.onupgradeneeded = () => request.result.createObjectStore(STORE);
		request.onsuccess = () => resolve(request.result);
		request.onerror = () => reject(request.error);
	});
}

async function store(mode, work) {
	const db = await database();
	try {
		return await new Promise((resolve, reject) => {
			const tx = db.transaction(STORE, mode);
			const request = work(tx.objectStore(STORE));
			tx.oncomplete = () => resolve(request?.result);
			tx.onerror = tx.onabort = () => reject(tx.error);
		});
	} finally {
		db.close();
	}
}

// Whether this browser can keep anything at all.
export const able = () => typeof indexedDB !== "undefined" && !!globalThis.crypto?.subtle;

// Whether a passkey here can give the key (the WebAuthn `prf` extension).
// Asked before one is made, so none is left behind that can't.
async function passkeyAble() {
	try {
		const can = await globalThis.PublicKeyCredential?.getClientCapabilities?.();
		return !!can?.["extension:prf"] && (can.userVerifyingPlatformAuthenticator ?? true);
	} catch {
		return false;
	}
}

// The AES key a passkey's secret makes.
async function fromPasskey(secret) {
	const base = await crypto.subtle.importKey("raw", secret, "HKDF", false, ["deriveKey"]);
	return crypto.subtle.deriveKey(
		{ name: "HKDF", hash: "SHA-256", salt: SALT, info: new Uint8Array() },
		base,
		{ name: "AES-GCM", length: 256 },
		false,
		["encrypt", "decrypt"],
	);
}

// The passkey's secret for `salt`: asks for the fingerprint or face.
async function ask(credential, salt) {
	const got = await navigator.credentials.get({
		publicKey: {
			challenge: random(32),
			allowCredentials: [{ type: "public-key", id: credential }],
			userVerification: "required",
			extensions: { prf: { eval: { first: salt } } },
		},
	});
	const secret = got?.getClientExtensionResults().prf?.results?.first;
	if (!secret) throw new Error("This device's passkey didn't give its key.");
	return new Uint8Array(secret);
}

// What's kept here, without opening it: `null`, or its id (the name of its
// record in the storage), how it's locked ("passkey" or "device") and what
// this device was called.
export async function kept() {
	if (!able()) return null;
	try {
		const record = await store("readonly", (s) => s.get(RECORD));
		return record ? { id: record.id, lock: record.lock, name: record.name } : null;
	} catch {
		return null;
	}
}

// Keep `account` (as `pasted` or a link gives it) here, called `name`.
// Says how it ended up locked. A passkey someone declines keeps nothing.
export async function keep(account, name) {
	const id = hex(random(12));
	const words = new TextEncoder().encode(JSON.stringify(account));
	const iv = random(12);
	const record = { id, name, iv };
	let key;
	if (await passkeyAble()) {
		const salt = random(32);
		const made = await navigator.credentials.create({
			publicKey: {
				challenge: random(32),
				rp: { name: "Omacloud" },
				user: { id: random(16), name: "Omacloud", displayName: "Omacloud" },
				pubKeyCredParams: [
					{ type: "public-key", alg: -7 },
					{ type: "public-key", alg: -257 },
				],
				authenticatorSelection: { authenticatorAttachment: "platform", residentKey: "preferred", userVerification: "required" },
				extensions: { prf: { eval: { first: salt } } },
			},
		});
		const credential = new Uint8Array(made.rawId);
		// some give the secret as the passkey is made; the rest when asked
		const first = made.getClientExtensionResults().prf?.results?.first;
		key = await fromPasskey(first ? new Uint8Array(first) : await ask(credential, salt));
		Object.assign(record, { lock: "passkey", credential, salt });
	} else {
		key = await crypto.subtle.generateKey({ name: "AES-GCM", length: 256 }, false, ["encrypt", "decrypt"]);
		Object.assign(record, { lock: "device", key });
	}
	record.sealed = new Uint8Array(await crypto.subtle.encrypt({ name: "AES-GCM", iv }, key, words));
	await store("readwrite", (s) => s.put(record, RECORD));
	return { id, lock: record.lock, name };
}

// The account kept here, opened: with a passkey, that asks for the
// fingerprint or face, so call it from a press.
export async function open() {
	const record = await store("readonly", (s) => s.get(RECORD));
	if (!record) throw new Error("Nothing is kept on this device.");
	const key = record.lock === "passkey" ? await fromPasskey(await ask(record.credential, record.salt)) : record.key;
	const words = await crypto.subtle.decrypt({ name: "AES-GCM", iv: record.iv }, key, record.sealed);
	return JSON.parse(new TextDecoder().decode(words));
}

// Forget what's kept here. (A passkey made for it stays in the device's
// passwords, opening nothing; the device's settings remove it.)
export async function forget() {
	if (able()) await store("readwrite", (s) => s.delete(RECORD));
}

// What to call this device, from what the browser says of itself.
export function deviceName(agent = navigator.userAgent) {
	const device = /iPhone/.test(agent)
		? "iPhone"
		: /iPad/.test(agent)
			? "iPad"
			: /Android/.test(agent)
				? /Mobile/.test(agent)
					? "Android phone"
					: "Android tablet"
				: /Mac OS X/.test(agent)
					? "Mac"
					: /Windows/.test(agent)
						? "Windows PC"
						: /Linux/.test(agent)
							? "Linux computer"
							: "A browser";
	const browser = /Firefox\//.test(agent)
		? "Firefox"
		: /Edg\//.test(agent)
			? "Edge"
			: /CriOS\/|Chrome\//.test(agent)
				? "Chrome"
				: /Safari\//.test(agent)
					? "Safari"
					: "";
	return browser ? `${device}, ${browser}` : device;
}
