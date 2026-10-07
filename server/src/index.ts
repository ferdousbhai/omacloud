// Omacloud storage: people sign in with Google at PUBLIC_URL, and their
// computers keep their (already encrypted) files at STORAGE_HOST, a gateway
// to one bucket with a folder per account.

import * as gateway from "./gateway.ts";
import * as keys from "./keys.ts";
import * as pages from "./pages.ts";
import * as signin from "./signin.ts";
import * as usage from "./usage.ts";
import * as waitlist from "./waitlist.ts";

export default {
	async fetch(req, env, ctx): Promise<Response> {
		const url = new URL(req.url);
		const host = (req.headers.get("host") ?? url.host).toLowerCase();
		try {
			if (host === env.STORAGE_HOST.toLowerCase()) return await gateway.handle(req, env, ctx);
			if (req.method === "GET" && url.pathname === "/") return pages.home(env);
			if (req.method === "GET" && url.pathname === "/privacy") return pages.privacy();
			if (req.method === "GET" && url.pathname === "/terms") return pages.terms();
			if (req.method === "GET" && url.pathname === "/health") return new Response("ok");
			if (req.method === "GET" && url.pathname === "/signin") return await signin.start(req, url, env);
			if (req.method === "GET" && url.pathname === "/auth/google/callback") return await signin.back(url, env);
			if (req.method === "POST" && url.pathname === "/waitlist") return await waitlist.request(req, env, ctx);
			if (req.method === "GET" && url.pathname === "/waitlist/confirm") return waitlist.confirmPage(url);
			if (req.method === "POST" && url.pathname === "/waitlist/confirm") return await waitlist.confirmed(req, env);
			if (req.method === "POST" && url.pathname === "/api/credentials") return await signin.credentials(req, env);
			if (req.method === "POST" && url.pathname === "/api/keys") return await keys.create(req, env);
			if (req.method === "POST" && url.pathname === "/api/keys/retire") return await keys.retire(req, env);
			if (req.method === "POST" && url.pathname === "/api/contact") return await signin.contact(req, env);
			return new Response("Not found\n", { status: 404 });
		} catch (e) {
			console.error(JSON.stringify({ path: url.pathname, error: String(e), stack: (e as Error).stack }));
			return new Response("Something went wrong\n", { status: 500 });
		}
	},

	// each job on its own, so one failing (email, say) doesn't stop the
	// rest; the quick ones first, as counting usage can take the whole run
	async scheduled(_controller, env): Promise<void> {
		const jobs: [string, () => Promise<void>][] = [
			["invitations", () => waitlist.notifyInvites(env)],
			["waitlist digest", () => waitlist.digest(env)],
			["waitlist purge", () => waitlist.purge(env)],
			["usage", () => usage.count(env)],
		];
		for (const [job, run] of jobs) {
			try {
				await run();
			} catch (e) {
				console.error(JSON.stringify({ job, error: String(e), stack: (e as Error).stack }));
			}
		}
	},
} satisfies ExportedHandler<Env>;
