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
