// Omacloud storage: people sign in with Google at PUBLIC_URL, and their
// computers keep their (already encrypted) files at STORAGE_HOST, a gateway
// to one bucket with a folder per account.

import * as gateway from "./gateway.ts";
import * as pages from "./pages.ts";
import * as signin from "./signin.ts";
import { countPage } from "./upstream.ts";

/** List pages one usage count may read, a thousand objects each (Workers Paid allows 10,000 subrequests). */
const COUNT_PAGES = 1000;

export default {
	async fetch(req, env, ctx): Promise<Response> {
		const url = new URL(req.url);
		const host = (req.headers.get("host") ?? url.host).toLowerCase();
		try {
			if (host === env.STORAGE_HOST.toLowerCase()) return await gateway.handle(req, env, ctx);
			if (req.method === "GET" && url.pathname === "/") return pages.home();
			if (req.method === "GET" && url.pathname === "/privacy") return pages.privacy();
			if (req.method === "GET" && url.pathname === "/terms") return pages.terms();
			if (req.method === "GET" && url.pathname === "/health") return new Response("ok");
			if (req.method === "GET" && url.pathname === "/signin") return await signin.start(url, env);
			if (req.method === "GET" && url.pathname === "/auth/google/callback") return await signin.back(url, env);
			if (req.method === "POST" && url.pathname === "/api/credentials") return await signin.credentials(req, env);
			return new Response("Not found\n", { status: 404 });
		} catch (e) {
			console.error(JSON.stringify({ path: url.pathname, error: String(e), stack: (e as Error).stack }));
			return new Response("Something went wrong\n", { status: 500 });
		}
	},

	// usage, counted again for accounts written to, a few pages at a time
	async scheduled(_controller, env): Promise<void> {
		let pages = COUNT_PAGES;
		const dirty = await env.DB.prepare(
			"SELECT id, scan_token, scan_total FROM accounts WHERE dirty = 1 OR scan_token IS NOT NULL LIMIT 100",
		).all<{ id: string; scan_token: string | null; scan_total: number }>();
		for (const a of dirty.results) {
			let token = a.scan_token;
			let total = token ? a.scan_total : 0;
			if (!token) await env.DB.prepare("UPDATE accounts SET dirty = 0 WHERE id = ?").bind(a.id).run();
			while (pages-- > 0) {
				const page = await countPage(env, `${a.id}/`, token);
				total += page.bytes;
				token = page.next;
				if (!token) break;
			}
			await env.DB.prepare(
				token
					? "UPDATE accounts SET scan_token = ?2, scan_total = ?3 WHERE id = ?1"
					: "UPDATE accounts SET used = ?3, scan_token = NULL, scan_total = 0 WHERE id = ?1",
			)
				.bind(a.id, token, total)
				.run();
			if (pages <= 0) return;
		}
	},
} satisfies ExportedHandler<Env>;
