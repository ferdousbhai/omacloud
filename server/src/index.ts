// omacloud.computer: the public pages, and the storage network's API under
// /api/net/ (src/net/).

import { handle as net } from "./net/router.ts";
import * as pages from "./pages.ts";

export default {
	async fetch(req, env): Promise<Response> {
		const url = new URL(req.url);
		if (url.pathname.startsWith("/api/net/")) return net(req, env);
		if (req.method === "GET" || req.method === "HEAD") {
			if (url.pathname === "/") return pages.home();
			if (url.pathname === "/privacy") return pages.privacy();
			if (url.pathname === "/terms") return pages.terms();
			if (url.pathname === "/s") return pages.shared();
			if (url.pathname === "/llms.txt") return pages.llms(env);
			if (url.pathname === "/health") return new Response("ok");
		}
		return new Response("Not found\n", { status: 404 });
	},
} satisfies ExportedHandler<Env>;
