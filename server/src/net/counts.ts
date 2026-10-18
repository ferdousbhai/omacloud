// Counters with a window: how many of something a prefix did lately.

/**
 * Adds `n` to the counter `kind`/`key` if that keeps it within `limit`, and
 * says whether it did. A counter's window starts at its first use and lasts
 * `window` seconds; then it starts again from nothing.
 */
export async function take(db: D1Database, kind: string, key: string, n: number, limit: number, window: number, now: number): Promise<boolean> {
	if (n > limit) return false;
	const [, taken] = await db.batch([
		// lapsed windows go, everyone's: what keeps the table small
		db.prepare("DELETE FROM net_counts WHERE until <= ?").bind(now),
		db
			.prepare(
				`INSERT INTO net_counts (kind, key, n, until) VALUES (?1, ?2, ?3, ?4)
				ON CONFLICT (kind, key) DO UPDATE SET n = n + excluded.n WHERE n + excluded.n <= ?5`,
			)
			.bind(kind, key, n, now + window, limit),
	]);
	return (taken.meta.changes ?? 0) > 0;
}
