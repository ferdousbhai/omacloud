// The public pages of omacloud.computer: home, privacy, terms, llms.txt and
// /s, where a share link lands. index.ts also serves /install.sh.

const PRIVACY = "privacy@omacloud.computer";
const LEGAL = "legal@omacloud.computer";
const UPDATED = "19 October 2026";

function layout(title: string, body: string): Response {
	return new Response(
		`<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${title}</title>
<meta name="description" content="Your files and settings on every Omarchy computer, end to end encrypted.">
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
<p class="lead">Your files and settings on all your Omarchy computers. Only you can read them.</p>
<p>Omacloud syncs:</p>
<ul>
<li>The folders that you choose, such as Documents, Pictures or Music</li>
<li>Your Omarchy settings and package lists</li>
<li>Your Chromium bookmarks, settings and extensions</li>
<li>Your AI agent settings and your Wi-Fi networks</li>
<li>Your SSH and GPG keys.</li>
</ul>
<p>You can get back an earlier version of a file, or a file that you deleted.</p>
<p>Omacloud encrypts your files on your computer. Other Omarchy computers keep each encrypted
file, on more than one computer. Your computer keeps files for other persons. No computer can read the files
that it keeps for other persons.</p>
<p>You do not need an account, an email address or a password. Keep your recovery code in a
safe location. If you lose all your computers, you need this code to get your data.</p>
<h2>Get it</h2>
<pre>curl -fsSL https://omacloud.computer/install.sh | sudo bash</pre>
<p>Then open Omacloud from the app launcher.</p>`,
	);

/** Where a share link lands. The key is after the #, which browsers never
 * send, so this page can't open the file and doesn't try. */
export const shared = () =>
	layout(
		"A shared file · Omacloud",
		`<h1>A file shared with you</h1>
<p class="lead">Someone shared a file through Omacloud. It is encrypted, and only this link opens it.</p>
<p>To download it on Omarchy, install Omacloud, run this command and paste the link:</p>
<pre>omacloud receive</pre>
<p>You don't need an account. The link works until it expires or the person who shared it takes
it back: 7 days, unless they chose longer.</p>
<p>This site never receives the part of the link after the #, so it can't read the file.</p>
<h2>Get Omacloud</h2>
<pre>curl -fsSL https://omacloud.computer/install.sh | sudo bash</pre>`,
	);

export const privacy = () =>
	layout(
		"Privacy · Omacloud",
		`<h1>Privacy</h1>
<p class="lead">Updated ${UPDATED}.</p>
<p>This site collects nothing. It sets no cookies, has no accounts and keeps no logs. Requests
pass through Cloudflare, which handles your IP address to deliver them.</p>
<p>Omacloud itself has no accounts or sign-in. Your files, their names, your settings and your
keys are encrypted on your computer, with keys that never leave your computers and your
recovery code, before anything is stored. Whoever holds the encrypted data can't read it.</p>
<p>For Omacloud's storage network, this site keeps only what its limits need: the /24 or /48
that an address belongs to, never the address itself, and only for a while. For each storage
node it keeps the node's network and the address that computers reach it at, and forgets the
node 30 days after it was last seen.</p>
<h2>Contact</h2>
<p><a href="mailto:${PRIVACY}">${PRIVACY}</a></p>`,
	);

export const terms = () =>
	layout(
		"Terms · Omacloud",
		`<h1>Terms</h1>
<p class="lead">Updated ${UPDATED}.</p>
<h2>The software</h2>
<p>Omacloud is free software under the MIT license, provided as it is, without warranty. Keep
other copies of anything you can't afford to lose. To the extent the law allows, Omacloud isn't
liable for lost data.</p>
<h2>Your recovery code</h2>
<p>Only your computers and your recovery code can decrypt your data. If you lose the recovery
code and all your computers, nobody, including Omacloud, can recover your files. Keep the code
safe and offline.</p>
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
> and ssh and gpg keys follow; earlier versions and deleted files come back. Its storage network is held by Omarchy
> computers themselves, untrusted, with no accounts or sign-in. Underneath it is plain
> restic, so restic or rustic restore everything without Omacloud.

Everything is encrypted on the computer before it is stored: whoever holds the data can't read
it. Omacloud runs on Omarchy (Arch Linux, x86_64 and aarch64), as a GTK app and the
\`omacloud\` command line tool that drives it. Install with
\`curl -fsSL ${site}/install.sh | sudo bash\` (or, where Omarchy's own repository carries it, \`omarchy pkg add omacloud\`).

## Docs

- [README](${repo}/blob/master/README.md): install, setup, what syncs, versions and configuration
- [How Omacloud works](${repo}/blob/master/docs/design.md): the encryption, device approval and threat model
- [Changelog](${repo}/blob/master/CHANGELOG.md): what each release changed

## Command line

- [omacloud CLI](${repo}/blob/master/README.md): \`omacloud --help\` lists every command. Piped, every command prints a JSON envelope ({ok, data, summary} or {ok: false, error: {code, message, hint}}; \`--quiet\` for the data alone) and exits 0, 1, 2 usage, 3 not set up, 4 not found or 5 unreachable; \`omacloud status\` reports state. Commands that print a secret (\`join-code\`, \`export\`, \`share\`, a new account's recovery code) print only to a terminal. Secrets come from environment variables (OMACLOUD_RECOVERY_CODE, OMACLOUD_JOIN_CODE) or a prompt, never arguments

## Site

- [Home](${site}/): what Omacloud is and how to install it
- [Privacy](${site}/privacy): the site collects nothing; Omacloud has no accounts
- [Terms](${site}/terms): terms

## Optional

- [Source](${repo}): MIT licensed Rust (sync engine, CLI, GTK app) and the Cloudflare Worker behind this site
- [Contributing](${repo}/blob/master/CONTRIBUTING.md): building, testing and releases
`,
		{ headers: { "content-type": "text/plain; charset=utf-8", "cache-control": "public, max-age=300" } },
	);
};
