// Rate limits (the Workers Rate Limiting bindings in cloudflare.config.ts).

/** The address a request came from, as Cloudflare saw it. */
export const clientIp = (req: Request): string => req.headers.get("cf-connecting-ip") ?? "unknown";

/**
 * Whether `key` is still under the limiter's rate. Without the binding (unit
 * tests) everything is; if the limiter fails, the request goes through
 * rather than every sign-in failing with it.
 */
export async function allowed(limiter: RateLimit | undefined, key: string): Promise<boolean> {
	if (!limiter) return true;
	try {
		return (await limiter.limit({ key })).success;
	} catch (e) {
		console.error(JSON.stringify({ event: "rate limiter failed", error: String(e) }));
		return true;
	}
}
