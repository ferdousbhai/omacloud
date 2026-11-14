// A repository kept in the person's Dropbox, read as s3.js reads a bucket:
// listing a folder, reading a file or a range of one, and writing one object
// for an upload. Omacloud's Dropbox app has no secret (it's allowed with
// PKCE), so the token a computer holds works from a page as it does there.

// Dropbox takes a call's arguments in a header, which holds ASCII only.
const header = (value) => JSON.stringify(value).replace(/[\u007f-￿]/g, (c) => `\\u${c.charCodeAt(0).toString(16).padStart(4, "0")}`);

export class Dropbox {
	// root is the repository's folder in the app's folder, like /Omacloud
	constructor({ appKey, refreshToken, root }) {
		this.appKey = appKey;
		this.refreshToken = refreshToken;
		this.root = `/${(root || "").replace(/^\/+|\/+$/g, "")}`.replace(/\/$/, "");
		this.access = "";
		this.until = 0;
	}

	async fetch(url, init) {
		let response;
		try {
			response = await fetch(url, init);
		} catch {
			throw new Error("Dropbox didn't answer this page.");
		}
		return response;
	}

	// a short-lived token from the long-lived one, renewed a minute early
	async token() {
		if (Date.now() < this.until) return this.access;
		const response = await this.fetch("https://api.dropboxapi.com/oauth2/token", {
			method: "POST",
			body: new URLSearchParams({ grant_type: "refresh_token", refresh_token: this.refreshToken, client_id: this.appKey }),
		});
		if (!response.ok) throw new Error("Dropbox refused this sign-in. Make a new link or code on your computer.");
		const said = await response.json();
		this.access = said.access_token;
		this.until = Date.now() + (said.expires_in - 60) * 1000;
		return this.access;
	}

	async call(name, args) {
		const response = await this.fetch(`https://api.dropboxapi.com/2/${name}`, {
			method: "POST",
			headers: { Authorization: `Bearer ${await this.token()}`, "Content-Type": "application/json" },
			body: JSON.stringify(args),
		});
		// a folder that isn't there lists as empty
		if (response.status === 409) return null;
		if (!response.ok) throw new Error(`Dropbox answered ${response.status}.`);
		return response.json();
	}

	async list(folder) {
		return (await this.listed(folder)).map((file) => file.name);
	}

	// the names directly under a folder of the repository, each with when it
	// was last written
	async listed(folder) {
		const names = [];
		let page = await this.call("files/list_folder", { path: `${this.root}/${folder}`, limit: 2000 });
		while (page) {
			for (const entry of page.entries) {
				if (entry[".tag"] === "file") names.push({ name: entry.name, modified: entry.server_modified });
			}
			page = page.has_more ? await this.call("files/list_folder/continue", { cursor: page.cursor }) : null;
		}
		return names;
	}

	// a file of the repository, or `length` bytes of it from `offset`
	async read(name, offset, length) {
		const headers = {
			Authorization: `Bearer ${await this.token()}`,
			"Dropbox-API-Arg": header({ path: `${this.root}/${name}` }),
		};
		if (length) headers.Range = `bytes=${offset}-${offset + length - 1}`;
		const response = await this.fetch("https://content.dropboxapi.com/2/files/download", { method: "POST", headers });
		if (!response.ok) throw new Error(`Dropbox answered ${response.status}.`);
		return new Uint8Array(await response.arrayBuffer());
	}

	// write an object by its whole path in the app's folder (not under the
	// repository's): what the page uploads goes beside the repository
	async put(key, bytes) {
		const response = await this.fetch("https://content.dropboxapi.com/2/files/upload", {
			method: "POST",
			headers: {
				Authorization: `Bearer ${await this.token()}`,
				"Dropbox-API-Arg": header({ path: `/${key.replace(/^\/+/, "")}`, mode: "overwrite", mute: true }),
				"Content-Type": "application/octet-stream",
			},
			body: bytes,
		});
		if (!response.ok) throw new Error(`Dropbox answered ${response.status}.`);
	}
}
