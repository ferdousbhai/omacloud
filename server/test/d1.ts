// D1, standing in: the migrations applied to an in-memory SQLite (node:sqlite).

import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { DatabaseSync, type SQLInputValue } from "node:sqlite";

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
		// all or nothing, as D1's
		batch: async (stmts: Statement[]) => {
			db.exec("BEGIN");
			try {
				const out = [];
				// a query answers with its rows, as in D1
				for (const s of stmts) out.push(await (/^\s*SELECT/i.test(s.sql) ? s.all() : s.run()));
				db.exec("COMMIT");
				return out;
			} catch (e) {
				db.exec("ROLLBACK");
				throw e;
			}
		},
	} as unknown as D1Database & { sqlite: DatabaseSync };
}
