// The public pages of omacloud.computer: home, privacy and terms.

import { escapeXml } from "./upstream.ts";

const PRIVACY = "privacy@omacloud.computer";
const LEGAL = "legal@omacloud.computer";
const UPDATED = "7 October 2026";

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
form.ask { display: flex; flex-wrap: wrap; gap: .6rem; align-items: center; margin: 1rem 0; }
form.ask input { flex: 1 1 14rem; font: inherit; padding: .5rem .7rem; color: var(--fg);
  background: var(--bg); border: 1px solid var(--line); border-radius: 8px; }
form.ask button { font: inherit; padding: .5rem 1rem; border: 0; border-radius: 8px;
  background: var(--accent); color: var(--bg); cursor: pointer; }
form.ask .cf-turnstile { flex-basis: 100%; }
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

/** The invitation form, when Turnstile is set up for it. */
const ask = (siteKey: string) =>
	siteKey
		? `
<h2>Ask for an invitation</h2>
<p>Or leave your email here. You'll get a link to confirm it, and an invitation when there's
room.</p>
<form class="ask" method="post" action="/waitlist">
<input type="email" name="email" required maxlength="254" autocomplete="email" placeholder="you@example.com" aria-label="Email address">
<button>Request an invite</button>
<div class="cf-turnstile" data-sitekey="${escapeXml(siteKey)}"></div>
</form>
<script src="https://challenges.cloudflare.com/turnstile/v0/api.js" async defer></script>`
		: "";

export const home = (env: Env) =>
	layout(
		"Omacloud",
		`<h1>Omacloud</h1>
<p class="lead">Your files and settings on every Omarchy computer, end to end encrypted.</p>
<p>Desktop, Documents and Pictures sync in place, as iCloud does. Omarchy settings, package
lists and ssh and gpg keys follow you to every computer, and every version is kept.</p>
<p>Everything is encrypted on your computer before it leaves it. Omacloud stores only what it
can't read: not your files, not their names, not your settings.</p>
<h2>Get it</h2>
<pre>curl -fsSL https://github.com/ferdousbhai/omacloud/releases/latest/download/install.sh | sudo bash</pre>
<p>Then open Omacloud from the app launcher and sign in with Google. Omacloud storage is by
invitation for now: signing in without one puts you on the waitlist, and you'll get an email
when there's room. Until then, or instead, you can keep everything in a storage bucket of your
own.</p>${ask(env.TURNSTILE_SITE_KEY)}`,
	);

export const privacy = () =>
	layout(
		"Privacy · Omacloud",
		`<h1>Privacy</h1>
<p class="lead">Updated ${UPDATED}.</p>
<p>Omacloud storage keeps your files for you without being able to read them. This page says
what it does know.</p>
<h2>What Omacloud can't see</h2>
<p>Your files, their names, your folders, your settings and your keys are encrypted on your
computer, with keys that never leave your computers and your recovery code, before anything is
uploaded. Omacloud stores the encrypted result and can't decrypt it.</p>
<h2>What Omacloud keeps</h2>
<ul>
<li>From Google sign-in: your Google account's id and email address. Nothing else is asked
for. If you sign in without an invitation, they're kept on the waitlist until you're invited,
or until you ask for them to be removed.</li>
<li>If you ask for an invitation on this site: the email address you type. It's kept for two
days while the link emailed to it works; follow the link and it stays on the waitlist like a
sign-in, or don't and it's deleted when the link expires. Cloudflare Turnstile checks the form is sent by
a person, and sees your browser and IP address to do it.</li>
<li>Your account: when it was made, your storage quota and how much of it you use.</li>
<li>Your encrypted data, and the sizes and times of what your computers upload.</li>
<li>Service logs (which account made a request, what kind, and how it went), kept for a short
time to run and debug the service.</li>
</ul>
<h2>Where it lives</h2>
<p>Encrypted files are stored with Hetzner Online in Falkenstein, Germany. Account records are
kept in Cloudflare D1 in the EU, and requests pass through Cloudflare, which handles your IP
address to deliver them.</p>
<h2>What Omacloud doesn't do</h2>
<p>No ads, no tracking, no selling or sharing of your information. Your email is used only to
run your account and to contact you about it: to confirm a waitlist request, to invite you,
and about your account. Emails are sent through Cloudflare Email Sending.</p>
<h2>Leaving</h2>
<p>You can take your data out at any time (<code>omacloud export</code> gives you a standard
restic repository). Ask at <a href="mailto:${PRIVACY}">${PRIVACY}</a> and your account and
everything stored for it are deleted, or your address taken off the waitlist.</p>
<h2>Contact</h2>
<p><a href="mailto:${PRIVACY}">${PRIVACY}</a></p>`,
	);

export const terms = () =>
	layout(
		"Terms · Omacloud",
		`<h1>Terms of service</h1>
<p class="lead">Updated ${UPDATED}.</p>
<h2>The service</h2>
<p>Omacloud storage keeps the encrypted files and settings of the Omacloud app for you. It is
an early service, offered by invitation, and may change as it grows.</p>
<h2>Your recovery code</h2>
<p>Only your computers and your recovery code can decrypt your data. If you lose the recovery
code and all your computers, nobody, including Omacloud, can recover your files. Keep the code
safe and offline.</p>
<h2>Using it fairly</h2>
<p>Use Omacloud storage for your own files, within your quota and the law. Accounts used to
abuse the service or to harm others may be closed.</p>
<h2>No guarantee</h2>
<p>Omacloud storage is provided as it is, without warranty. Keep other copies of anything you
can't afford to lose. To the extent the law allows, Omacloud isn't liable for lost data or for
the service being unavailable.</p>
<h2>Ending</h2>
<p>You can stop using Omacloud storage and ask for your account to be deleted at any time.
If the service ends, you'll be told in advance and given time to export your data.</p>
<h2>Contact</h2>
<p><a href="mailto:${LEGAL}">${LEGAL}</a></p>`,
	);
