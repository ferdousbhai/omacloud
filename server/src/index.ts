// omacloud.computer: the public pages. The storage network's API will live
// in src/net/.

import * as pages from "./pages.ts";

export default {
	async fetch(req, env): Promise<Response> {
		const url = new URL(req.url);
		if (req.method === "GET" || req.method === "HEAD") {
			if (url.pathname === "/") return pages.home();
			if (url.pathname === "/privacy") return pages.privacy();
			if (url.pathname === "/terms") return pages.terms();
			if (url.pathname === "/llms.txt") return pages.llms(env);
			if (url.pathname === "/health") return new Response("ok");
		}
		return new Response("Not found\n", { status: 404 });
	},
} satisfies ExportedHandler<Env>;
