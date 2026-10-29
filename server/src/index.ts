// omacloud.computer: the public pages, and the storage network's API under
// /api/net/ (src/net/).

import { handle as net } from "./net/router.ts";
import * as pages from "./pages.ts";
import { secureDocument } from "./security-headers.ts";

export default {
	async fetch(req, env): Promise<Response> {
		const url = new URL(req.url);
		if (url.pathname.startsWith("/api/net/")) return net(req, env);
		if (req.method === "GET" || req.method === "HEAD") {
			if (url.pathname === "/") return secureDocument(pages.home());
			if (url.pathname === "/privacy") return secureDocument(pages.privacy());
			if (url.pathname === "/terms") return secureDocument(pages.terms());
			if (url.pathname === "/s") return secureDocument(pages.shared());
			if (url.pathname === "/llms.txt") return pages.llms(env);
			if (url.pathname === "/health") return new Response("ok");
			// the latest release's installer: what it installs it checks
			// against the signing key it pins, wherever it came from
			if (url.pathname === "/install.sh") {
				return Response.redirect("https://github.com/ferdousbhai/omacloud/releases/latest/download/install.sh", 302);
			}
		}
		return new Response("Not found\n", { status: 404 });
	},
} satisfies ExportedHandler<Env>;
