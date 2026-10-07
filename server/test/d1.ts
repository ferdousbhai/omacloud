// D1, standing in: the migrations applied to an in-memory SQLite (node:sqlite);
// and what else of the Workers runtime the Worker's code needs in node.

import { createHash, timingSafeEqual } from "node:crypto";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { DatabaseSync, type SQLInputValue } from "node:sqlite";

// Workers' own, which node lacks
const subtle = crypto.subtle as unknown as { timingSafeEqual?: (a: Uint8Array, b: Uint8Array) => boolean };
subtle.timingSafeEqual ??= (a, b) => timingSafeEqual(a, b);
// and MD5, which Workers' digest does (for batch deletes)
const digest = crypto.subtle.digest.bind(crypto.subtle);
(crypto.subtle as { digest: unknown }).digest = (algorithm: string, data: Uint8Array) =>
	algorithm === "MD5" ? Promise.resolve(new Uint8Array(createHash("md5").update(data).digest()).buffer) : digest(algorithm, data);
const g = globalThis as unknown as { FixedLengthStream?: unknown };
g.FixedLengthStream ??= class extends TransformStream {
	constructor(_length: number) {
		super();
	}
};

class Statement {
	db: DatabaseSync;
	sql: string;
	params: SQLInputValue[];
	constructor(db: DatabaseSync, sql: string, params: SQLInputValue[] = []) {
		this.db = db;
		this.sql = sql;
		this.params = params;
	}
	bind(...params: unknown[]) {
		return new Statement(this.db, this.sql, params.map((p) => (p === undefined ? null : p)) as SQLInputValue[]);
	}
	async run() {
		const r = this.db.prepare(this.sql).run(...this.params);
		return { success: true, results: [], meta: { changes: Number(r.changes) } };
	}
	async all() {
		return { success: true, results: this.db.prepare(this.sql).all(...this.params).map((r) => ({ ...r })), meta: {} };
	}
	async first() {
		const r = this.db.prepare(this.sql).get(...this.params);
		return r ? { ...r } : null;
	}
}

export function d1(): D1Database & { sqlite: DatabaseSync } {
	const db = new DatabaseSync(":memory:");
	const dir = fileURLToPath(new URL("../migrations/", import.meta.url).href);
	for (const f of readdirSync(dir).sort()) db.exec(readFileSync(join(dir, f), "utf8"));
	return {
		sqlite: db,
		prepare: (sql: string) => new Statement(db, sql),
		batch: async (stmts: Statement[]) => {
			db.exec("BEGIN");
			try {
				const out = [];
				for (const s of stmts) out.push(await s.run());
				db.exec("COMMIT");
				return out;
			} catch (e) {
				db.exec("ROLLBACK");
				throw e;
			}
		},
	} as unknown as D1Database & { sqlite: DatabaseSync };
}
