// The public pages of omacloud.computer: home, privacy, terms and llms.txt.
// index.ts also serves /install.sh and Omacloud Drive (/drive).

const PRIVACY = "privacy@omacloud.computer";
const LEGAL = "legal@omacloud.computer";
const UPDATED = "11 November 2026";

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

export const home = () =>
	layout(
		"Omacloud",
		`<h1>Omacloud</h1>
<p class="lead">Sync for Omarchy computers. Your files and your setup on every machine, encrypted before
they leave it, kept in storage you own.</p>
<p>I have a few Omarchy computers and I wanted them to feel like one. iCloud does that for a Mac.
Nothing did it for Omarchy, so I wrote this.</p>
<h2>What it does</h2>
<p>You pick the folders you want everywhere (Documents, Pictures, anything in your home) and they sync in
place. Then it carries the things that make a computer feel like yours:</p>
<ul>
<li>Omarchy settings: keybindings, look and feel, terminals, your theme</li>
<li>AI agent settings, instructions and skills, for Claude Code, Codex, pi and the rest</li>
<li>Chromium bookmarks, settings and extensions</li>
<li>Wi-Fi networks, web apps, and the list of packages you installed</li>
<li>SSH and GPG keys, sealed so that only your recovery code opens them</li>
</ul>
<p>What describes one machine stays on that machine: monitors, the touchpad, sign-ins, anything another
program installed. And if you delete a file or wreck one, the old versions are there for 30 days.</p>
<h2>How it works</h2>
<p>There is no Omacloud server. That is the whole idea. Your computer encrypts everything, then writes it to
storage that is yours: your Dropbox, or an S3 bucket at Cloudflare R2, Hetzner or anywhere else. The provider
holds bytes it can't read. I hold nothing.</p>
<p>Underneath it is plain <a href="https://restic.net">restic</a>. If Omacloud vanished tomorrow, restic by
itself restores everything, and <code>omacloud export</code> prints the command to do it.</p>
<p>The cost of nobody else holding your keys is that nobody else can reset them. Setup gives you a recovery
code. Write it down and keep it somewhere that isn't a computer. Lose the code and all your computers, and
the data is gone. If that worries you, you can split the code among people you trust.</p>
<h2>In a browser</h2>
<p><a href="/drive/">Omacloud Drive</a> opens your files from any browser. Same rule as everything else: the
page fetches from your bucket and decrypts in the tab, so this site never sees a file or a key. It is early.
Today it needs a bucket (no Dropbox yet), and you sign in by pasting what <code>omacloud export</code> prints,
which is clunky. Approving a browser from one of your computers comes next.</p>
<h2>Family</h2>
<p>Each person gets an account of their own, in storage of their own. You can pay for someone's bucket and
still not be able to read their files, which is how it should be.</p>
<h2>Get it</h2>
<pre>curl -fsSL https://omacloud.computer/install.sh | sudo bash</pre>
<p>Then open Omacloud from the app launcher. It is free, MIT licensed, and the
<a href="https://github.com/ferdousbhai/omacloud">code is on GitHub</a>. It is also young. I run it on my own
computers, but keep another copy of anything you can't afford to lose.</p>`,
	);

export const privacy = () =>
	layout(
		"Privacy · Omacloud",
		`<h1>Privacy</h1>
<p class="lead">Updated ${UPDATED}.</p>
<p>Short version: this site knows nothing about you. No cookies, no accounts, no logs, no analytics.
Cloudflare delivers the pages and sees your IP address to do that. That is all.</p>
<p>Omacloud itself has no accounts and runs no storage. Your files, their names, your settings and your
keys are encrypted on your computer, with keys that stay on your computers and in your recovery code, before
they go to the storage you chose. That provider can't read them. It has its own terms and privacy policy,
and you deal with it directly. Nothing of yours comes here.</p>
<p><a href="/drive/">Omacloud Drive</a> is a page that runs in your browser. What you type into it (your
bucket's key, your password) stays in that tab. It goes into requests from your browser to your storage and
is gone when you close the tab. Your files travel between your browser and your storage. This site serves
the page and sees none of it.</p>
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
<p>Only your computers and your recovery code can decrypt your data. If you lose the code and all your
computers, nobody can get your files back, and that includes me. Keep the code safe and offline.</p>
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

- [omacloud CLI](${repo}/blob/master/README.md): \`omacloud --help\` lists every command. Piped, every command prints a JSON envelope ({ok, data, summary} or {ok: false, error: {code, message, hint}}; \`--quiet\` for the data alone) and exits 0, 1, 2 usage, 3 not set up, 4 not found or 5 unreachable; \`omacloud status\` reports state. Commands that print a secret (\`join-code\`, \`invite\`, \`export\`, \`recovery split\` and \`combine\`, a new account's recovery code) print only to a terminal. Secrets come from environment variables (OMACLOUD_RECOVERY_CODE, OMACLOUD_JOIN_CODE, OMACLOUD_SECRET_ACCESS_KEY) or a prompt, never arguments

## Site

- [Home](${site}/): what Omacloud is and how to install it
- [Omacloud Drive](${site}/drive/): a person's files in a browser, read from their own bucket and decrypted there
- [Privacy](${site}/privacy): visiting collects nothing; Omacloud has no accounts
- [Terms](${site}/terms): terms

## Optional

- [Source](${repo}): MIT licensed Rust (sync engine, CLI, GTK app) and the Cloudflare Worker behind this site
- [Contributing](${repo}/blob/master/CONTRIBUTING.md): building, testing and releases
`,
		{ headers: { "content-type": "text/plain; charset=utf-8", "cache-control": "public, max-age=300" } },
	);
};
