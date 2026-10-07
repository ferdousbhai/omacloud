// The emails Omacloud sends, through Cloudflare Email Sending: an
// invitation, the link that confirms a waitlist request typed on the
// website, and the admin's daily count of the waitlist. Logs never name the
// address.

import { escapeXml } from "./upstream.ts";

const FROM = { email: "invites@omacloud.computer", name: "Omacloud" };
const INSTALL = "curl -fsSL https://github.com/ferdousbhai/omacloud/releases/latest/download/install.sh | sudo bash";

export interface Message {
	subject: string;
	text: string;
	html: string;
}

/** Send `message` to `to`; false (and a log line saying what kind) if it didn't go. */
export async function send(env: Env, to: string, kind: string, message: Message): Promise<boolean> {
	// absent in unit tests
	if (!env.EMAIL) {
		console.error(JSON.stringify({ event: "email not sent", kind, error: "no EMAIL binding" }));
		return false;
	}
	try {
		await env.EMAIL.send({ from: FROM, to, ...message });
		return true;
	} catch (e) {
		const code = (e as { code?: string }).code;
		console.error(JSON.stringify({ event: "email not sent", kind, code, error: String(e) }));
		return false;
	}
}

/** Paragraphs as plain text and as simple HTML (`pre` keeps a command whole). */
function message(subject: string, paragraphs: (string | { pre: string } | { link: string })[]): Message {
	const text = paragraphs
		.map((p) => (typeof p === "string" ? p : "pre" in p ? `    ${p.pre}` : p.link))
		.join("\n\n");
	const html =
		`<div style="font:16px/1.5 system-ui,sans-serif;max-width:36em">` +
		paragraphs
			.map((p) =>
				typeof p === "string"
					? `<p>${escapeXml(p)}</p>`
					: "pre" in p
						? `<pre style="white-space:pre-wrap;background:#f3f1ee;padding:.6em .8em;border-radius:6px">${escapeXml(p.pre)}</pre>`
						: `<p><a href="${escapeXml(p.link)}">${escapeXml(p.link)}</a></p>`,
			)
			.join("") +
		"</div>";
	return { subject, text: `${text}\n`, html };
}

export const invitation = (email: string): Message =>
	message("You're invited to Omacloud storage", [
		"You're invited to Omacloud storage: your files and settings on every Omarchy computer, end to end encrypted.",
		"Install Omacloud on an Omarchy computer:",
		{ pre: INSTALL },
		`Then open Omacloud from the app launcher and sign in with Google as ${email}. If Omacloud is already installed, open it and sign in.`,
		{ link: "https://omacloud.computer" },
	]);

export const confirmation = (link: string): Message =>
	message("Confirm your place on the Omacloud waitlist", [
		"Someone, hopefully you, asked for an invitation to Omacloud storage for this address. To join the waitlist, open this link in the next two days:",
		{ link },
		"If it wasn't you, there's nothing to do: the address is forgotten when the link expires.",
	]);

export const digest = (count: number, since: number): Message =>
	message(`Omacloud waitlist: ${count} new`, [
		`${count} joined the waitlist ${since ? `since ${new Date(since * 1000).toISOString().slice(0, 16).replace("T", " ")} UTC` : "so far"}.`,
		"server/invite.sh list shows who; server/invite.sh <email> invites.",
	]);
