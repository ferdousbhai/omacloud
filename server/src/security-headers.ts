/** Shared document defenses. Keep game asset sandbox policies and API responses intact. */
export function secureDocument(response: Response): Response {
  if (!response.headers.get('content-type')?.includes('text/html')) return response
  const headers = new Headers(response.headers)
  headers.set('X-Content-Type-Options', 'nosniff')
  headers.set('X-Frame-Options', 'DENY')
  headers.set('Referrer-Policy', 'no-referrer')
  headers.set('Strict-Transport-Security', 'max-age=31536000')
  // Script and connection policies vary by app; these directives need no unsafe-inline exception.
  if (!headers.has('Content-Security-Policy')) {
    headers.set('Content-Security-Policy', "base-uri 'self'; object-src 'none'; frame-ancestors 'none'")
  }
  return new Response(response.body, { status: response.status, statusText: response.statusText, headers })
}
