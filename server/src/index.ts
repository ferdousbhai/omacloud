// omacloud.computer: the public pages.

import * as pages from "./pages.ts";
import { DRIVE_CSP, secureAsset, secureDocument } from "./security-headers.ts";

// Omacloud Drive: a page of static files that reads a person's storage from
// their browser. Nothing of theirs comes here: the Worker only hands out the
// page, under a policy that lets it load this site's own code and nothing else.
async function drive(req: Request, env: Env): Promise<Response> {
	const response = await env.ASSETS.fetch(req);
	if (!response.headers.get("content-type")?.includes("text/html")) return secureAsset(response);
	const headers = new Headers(response.headers);
	headers.set("Content-Security-Policy", DRIVE_CSP);
	// the page and what it loads change together: don't keep an old page
	headers.set("Cache-Control", "no-cache");
	return secureDocument(new Response(response.body, { status: response.status, statusText: response.statusText, headers }));
}

export default {
	async fetch(req, env): Promise<Response> {
		const url = new URL(req.url);
		if (req.method === "GET" || req.method === "HEAD") {
			if (url.pathname === "/") return secureDocument(pages.home());
			if (url.pathname === "/privacy") return secureDocument(pages.privacy());
			if (url.pathname === "/terms") return secureDocument(pages.terms());
			if (url.pathname === "/llms.txt") return pages.llms(env);
			if (url.pathname === "/health") return new Response("ok");
			if (url.pathname === "/drive" || url.pathname.startsWith("/drive/")) return drive(req, env);
			// the latest release's installer: what it installs it checks
			// against the signing key it pins, wherever it came from
			if (url.pathname === "/install.sh") {
				return Response.redirect("https://github.com/ferdousbhai/omacloud/releases/latest/download/install.sh", 302);
			}
		}
		return new Response("Not found\n", { status: 404 });
	},
} satisfies ExportedHandler<Env>;
