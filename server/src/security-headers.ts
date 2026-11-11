// Headers every HTML page gets: no sniffing, framing or referrer, HTTPS only.
// The CSP sets only directives that need no exception for the pages' inline
// script and style; a page that sets its own CSP keeps it.
export function secureDocument(response: Response): Response {
	if (!response.headers.get("content-type")?.includes("text/html")) return response;
	const headers = new Headers(response.headers);
	headers.set("X-Content-Type-Options", "nosniff");
	headers.set("X-Frame-Options", "DENY");
	headers.set("Referrer-Policy", "no-referrer");
	headers.set("Strict-Transport-Security", "max-age=31536000");
	if (!headers.has("Content-Security-Policy")) {
		headers.set("Content-Security-Policy", "base-uri 'self'; object-src 'none'; frame-ancestors 'none'");
	}
	return new Response(response.body, { status: response.status, statusText: response.statusText, headers });
}

// Omacloud Drive's policy: this site's own scripts, styles and WebAssembly;
// pictures, sound and documents only as the page itself decrypted them; and
// requests only to storage over HTTPS. No inline code, no other site's code,
// no framing.
export const DRIVE_CSP = [
	"default-src 'none'",
	"script-src 'self' 'wasm-unsafe-eval'",
	"style-src 'self'",
	"img-src blob:",
	"media-src blob:",
	"frame-src blob:",
	"connect-src 'self' https:",
	"base-uri 'none'",
	"form-action 'none'",
	"frame-ancestors 'none'",
].join("; ");

// What the page loads (scripts, styles, WebAssembly): served as what it is.
export function secureAsset(response: Response): Response {
	const headers = new Headers(response.headers);
	headers.set("X-Content-Type-Options", "nosniff");
	return new Response(response.body, { status: response.status, statusText: response.statusText, headers });
}
