// omacloud.computer: the public pages.

import { type Paid, d1Store, paid } from "./paid.ts";
import * as pages from "./pages.ts";
import { FILES_CSP, secureAsset, secureDocument } from "./security-headers.ts";

// Omacloud in a browser: a page of static files that reads a person's storage from
// their browser. Nothing of theirs comes here: the Worker only hands out the
// page, under a policy that lets it load this site's own code and nothing else.
async function files(req: Request, env: Env): Promise<Response> {
	const response = await env.ASSETS.fetch(req);
	if (!response.headers.get("content-type")?.includes("text/html")) return secureAsset(response);
	const headers = new Headers(response.headers);
	headers.set("Content-Security-Policy", FILES_CSP);
	// the page and what it loads change together: don't keep an old page
	headers.set("Cache-Control", "no-cache");
	return secureDocument(new Response(response.body, { status: response.status, statusText: response.statusText, headers }));
}

// What the paid tier runs on, once it's all there: a deploy without Stripe's
// keys or the database serves the free site and nothing else.
function paidTier(env: Env): Paid | null {
	if (!env.DB || !env.STRIPE_SECRET_KEY || !env.STRIPE_WEBHOOK_SECRET || !env.STRIPE_PRICE || !env.PASSKEY_SECRET || !env.CLOUDFLARE_API_TOKEN || !env.CLOUDFLARE_ACCOUNT) return null;
	return {
		store: d1Store(env.DB),
		site: env.PUBLIC_URL.replace(/\/$/, ""),
		stripeKey: env.STRIPE_SECRET_KEY,
		stripeWebhookSecret: env.STRIPE_WEBHOOK_SECRET,
		stripePrice: env.STRIPE_PRICE,
		secret: env.PASSKEY_SECRET,
		cloud: {
			token: env.CLOUDFLARE_API_TOKEN,
			account: env.CLOUDFLARE_ACCOUNT,
			module: async (name) => (await env.ASSETS.fetch(new Request(`https://assets.invalid/connector/${name}`))).arrayBuffer(),
		},
		fetch: (input, init) => fetch(input, init),
		now: () => Math.floor(Date.now() / 1000),
	};
}

export default {
	async fetch(req, env): Promise<Response> {
		const url = new URL(req.url);
		if (url.pathname.startsWith("/api/") || url.pathname.startsWith("/subscribe/")) {
			const tier = paidTier(env);
			try {
				const answered = tier && (await paid(tier, req));
				if (answered) return answered;
			} catch (e) {
				// Stripe or Cloudflare said no: in their words, for the command to show
				return new Response(JSON.stringify({ error: { code: "unavailable", message: (e as Error).message } }), {
					status: 502,
					headers: { "content-type": "application/json", "cache-control": "no-store" },
				});
			}
			return new Response("Not found\n", { status: 404 });
		}
		if (req.method === "GET" || req.method === "HEAD") {
			if (url.pathname === "/") return secureDocument(pages.home());
			if (url.pathname === "/privacy") return secureDocument(pages.privacy());
			if (url.pathname === "/terms") return secureDocument(pages.terms());
			if (url.pathname === "/subscribed") return secureDocument(pages.subscribed());
			if (url.pathname === "/llms.txt") return pages.llms(env);
			if (url.pathname === "/health") return new Response("ok");
			if (url.pathname === "/files" || url.pathname.startsWith("/files/")) return files(req, env);
			// the latest release's installer: what it installs it checks
			// against the signing key it pins, wherever it came from
			if (url.pathname === "/install.sh") {
				return Response.redirect("https://github.com/ferdousbhai/omacloud/releases/latest/download/install.sh", 302);
			}
		}
		return new Response("Not found\n", { status: 404 });
	},
} satisfies ExportedHandler<Env>;
