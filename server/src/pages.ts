// The public pages of omacloud.computer: privacy, terms and llms.txt. The
// site opens on Omacloud in a browser (/files), which index.ts serves along
// with /install.sh and the paid tier (paid.ts).

const PRIVACY = "privacy@omacloud.computer";
const LEGAL = "legal@omacloud.computer";
const UPDATED = "28 November 2026";

function layout(title: string, body: string): Response {
	return new Response(
		`<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${title}</title>
<meta name="description" content="Sync for Omarchy computers: your files and your setup on every machine, encrypted before they leave it, kept in storage you own.">
<style>
:root { --bg: #fbfaf8; --fg: #1d1c1a; --muted: #6b6760; --line: #e6e2dc; --accent: #2f6f5e; }
@media (prefers-color-scheme: dark) {
  :root { --bg: #151514; --fg: #ecebe8; --muted: #a29e96; --line: #2c2b29; --accent: #7cc4ad; }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--fg);
  font: 17px/1.6 ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif; }
main { max-width: 40rem; margin: 0 auto; padding: 4rem 1rem 3rem; }
h1 { font-size: 2.2rem; line-height: 1.2; margin: 0 0 .5rem; letter-spacing: -0.02em; }
h2 { font-size: 1.15rem; margin: 2.2rem 0 .4rem; }
p, li { color: var(--fg); }
.lead { font-size: 1.2rem; color: var(--muted); margin-top: 0; }
a { color: var(--accent); }
code, pre { font: 14px/1.5 ui-monospace, "JetBrains Mono", monospace; }
pre { background: color-mix(in srgb, var(--fg) 6%, transparent); border: 1px solid var(--line);
  border-radius: 8px; padding: .8rem 1rem; overflow-x: auto; }
footer { border-top: 1px solid var(--line); margin-top: 3rem; padding-top: 1rem;
  color: var(--muted); font-size: .9rem; }
footer a { color: var(--muted); margin-right: 1rem; }
</style>
<main>
${body}
<footer><a href="/">Omacloud</a><a href="/privacy">Privacy</a><a href="/terms">Terms</a><a href="https://github.com/ferdousbhai/omacloud">Source</a></footer>
</main>
</html>`,
		{ headers: { "content-type": "text/html; charset=utf-8", "cache-control": "public, max-age=300" } },
	);
}

export const subscribed = () =>
	layout(
		"Subscribed · Omacloud",
		`<h1>You're subscribed</h1>
<p class="lead">Back at your computer:</p>
<pre>omacloud agents connect Documents/Notes</pre>
<p>That gives your agent the folder you name. <code>omacloud subscription</code> shows where it stands and how to end it.</p>`,
	);

export const privacy = () =>
	layout(
		"Privacy · Omacloud",
		`<h1>Privacy</h1>
<p class="lead">Updated ${UPDATED}.</p>
<p>Short version: without a subscription this site knows nothing about you. No cookies, no accounts, no logs,
no analytics. Cloudflare delivers the pages and sees your IP address to do that. That is all.</p>
<p>Omacloud itself runs no storage. Your files, their names, your settings and your keys are encrypted on your
computer, with keys that stay on your computers and in your recovery code, before they go to the storage you
chose. That provider can't read them. It has its own terms and privacy policy, and you deal with it directly.</p>
<p><a href="/files/">Omacloud in a browser</a> is a page that runs in your browser. What you give it (a link from one of
your computers, or your bucket's key and your password) stays in that tab. It goes into requests from your browser to your storage and
is gone when you close the tab, unless you press Stay Signed In: then it's kept in that browser, on that device,
encrypted, until you press Forget This Device. Your files travel between your browser and your storage.</p>
<h2>With a subscription</h2>
<p>A subscription is a key your computer makes. This site keeps its hash, Stripe's ids for the customer and the
subscription, and whether it's paid for. It keeps no name, email or card: Stripe takes the payment and holds those,
under <a href="https://stripe.com/privacy">its own policy</a>.</p>
<p>Your connector for agents then runs in Omacloud's Cloudflare account. Its settings hold the keys to your storage
and files sealed under the connector's passphrase, which is shown on your computer and never sent here. They're
opened when an agent you gave the passphrase to asks, and held in the running connector's memory from then on; what
the agent reads passes through that connector to it. Nothing it reads is logged or kept.</p>
<p>A passkey that opens your files on any device: this site keeps the passkey's public key and what your browser
sealed with the passkey, and hands that back to a browser that proves the passkey. It can't open it.</p>
<p>End the subscription, or run <code>omacloud agents disconnect</code> or press Forget This Device, and the
connector and the sealed notes are deleted.</p>
<h2>Contact</h2>
<p><a href="mailto:${PRIVACY}">${PRIVACY}</a></p>`,
	);

export const terms = () =>
	layout(
		"Terms · Omacloud",
		`<h1>Terms</h1>
<p class="lead">Updated ${UPDATED}.</p>
<h2>The software</h2>
<p>Omacloud is free software under the MIT license. It comes as it is, with no warranty. Keep another copy
of anything you can't afford to lose. As far as the law allows, Omacloud isn't liable for lost data.</p>
<h2>Your recovery code</h2>
<p>Only your computers and your recovery code can decrypt your data, and what you let in yourself: a browser you
open your files in, and a connector for agents if you set one up. If you lose the code and all your
computers, nobody can get your files back, and that includes me. Keep the code safe and offline.</p>
<h2>The subscription</h2>
<p>Omacloud is a product of SummonGhost, Inc., which sells the subscription. It is $5 a month, charged by Stripe until you end it, which you can do at any time:
<code>omacloud subscription manage</code>. It then runs to the end of the month you paid for. For it Omacloud runs
your connector for agents and keeps what lets a passkey open your files in a browser. When it ends, both are
deleted; your files, in your own storage, are untouched. The service comes as it is, and may be unavailable at
times. It isn't a backup of anything.</p>
<h2>Contact</h2>
<p><a href="mailto:${LEGAL}">${LEGAL}</a></p>`,
	);

/** /llms.txt (llmstxt.org): what Omacloud is, for language models and agents. */
export const llms = (env: Env) => {
	const site = env.PUBLIC_URL.replace(/\/$/, "");
	const repo = "https://github.com/ferdousbhai/omacloud";
	return new Response(
		`# Omacloud

> Your files and settings on every Omarchy computer, end to end encrypted. The folders
> you choose sync in place; Omarchy settings, AI agent settings, Chromium, Wi-Fi, package lists
> and ssh and gpg keys follow; earlier versions and deleted files come back. Storage is your own
> Dropbox or S3 bucket (Cloudflare R2, Hetzner or others), one per person, with no Omacloud sign-in. Underneath it is
> plain restic, so restic or rustic restore everything without Omacloud.

Everything is encrypted on the computer before it is stored: whoever holds the data can't read
it. Omacloud runs on Omarchy (Arch Linux, x86_64 and aarch64), as a GTK app and the
\`omacloud\` command line tool that drives it. Install with
\`curl -fsSL ${site}/install.sh | sudo bash\` (or, where Omarchy's own repository carries it, \`omarchy pkg add omacloud\`).

## Docs

- [README](${repo}/blob/master/README.md): install, setup, what syncs, versions and configuration
- [How Omacloud works](${repo}/blob/master/docs/design.md): the encryption, device approval and threat model
- [Changelog](${repo}/blob/master/CHANGELOG.md): what each release changed

## Command line

- [omacloud CLI](${repo}/blob/master/README.md): \`omacloud --help\` lists every command. Piped, every command prints a JSON envelope ({ok, data, summary}; \`--quiet\` for the data alone) on stdout and exits 0; a failure exits 1 with {ok: false, error: {code, kind, message, hint}} on one line of stderr, kind being usage, setup, not_found, unavailable or error; \`omacloud status\` reports state. Commands that print a secret (\`join-code\`, \`invite\`, \`web-code\`, \`web-link\`, \`export\`, \`recovery split\` and \`combine\`, a new account's recovery code) print only to a terminal. Secrets come from environment variables (OMACLOUD_RECOVERY_CODE, OMACLOUD_JOIN_CODE, OMACLOUD_SECRET_ACCESS_KEY) or a prompt, never arguments

- [Agents off a person's computers](${repo}/blob/master/docs/cloud-agents.md): \`omacloud agents connect <folder>\` puts up an MCP connector for the folders named (claude.ai, an agent on a phone), in the person's own Cloudflare account, or run by Omacloud with a subscription (\`omacloud subscribe\`, $5 a month); what that trades

## Site

- [Omacloud in a browser](${site}/files/): where the site opens. A person's files in a browser, read from their own bucket or Dropbox and decrypted there; signed in with a link from one of their computers (\`omacloud web-link\`), it uploads too, and a phone can stay signed in; with a subscription a passkey signs in on any device
- [Privacy](${site}/privacy): visiting collects nothing; what a subscription keeps
- [Terms](${site}/terms): terms

## Optional

- [Source](${repo}): MIT licensed Rust (sync engine, CLI, GTK app) and the Cloudflare Worker behind this site
- [Contributing](${repo}/blob/master/CONTRIBUTING.md): building, testing and releases
`,
		{ headers: { "content-type": "text/plain; charset=utf-8", "cache-control": "public, max-age=300" } },
	);
};
