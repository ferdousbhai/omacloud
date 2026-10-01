// One account: its device chain, heads, epoch records, grants and join
// requests, in the Durable Object's SQLite storage. Appends are compare and
// swap on the next position, checked against the same rules clients apply
// (src/proto.ts). The service is trusted for availability only: clients
// verify everything again.

import { DurableObject } from "cloudflare:workers";
import {
  type DeviceEntry,
  type EpochRecord,
  type Grant,
  type Head,
  type JoinRequest,
  checkDevice,
  checkEpoch,
  checkHead,
  grantSigned,
  joinSigned,
  membersAt,
  requestKey,
} from "./proto";
import { Storage, epochOf, validPath, validPrefix } from "./storage";

/** Longest a head wait is held open. */
const WAIT_MS = 25_000;
/** Join requests kept at once, so strangers can't fill the table. */
const MAX_REQUESTS = 32;
/** Storage operations in one call. */
const MAX_STORAGE_OPS = 256;

type Who = "member" | "root" | "pending" | "revoked" | "anonymous";

class HttpError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message?: string,
  ) {
    super(message ?? code);
  }
}

const json = (value: unknown, status = 200) =>
  new Response(JSON.stringify(value), { status, headers: { "content-type": "application/json" } });

export class Account extends DurableObject<Env> {
  private sql: SqlStorage;
  private waiters: Array<(seq: number) => void> = [];

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.sql = ctx.storage.sql;
    this.migrate();
  }

  private migrate() {
    this.sql.exec(`
      CREATE TABLE IF NOT EXISTS anchor (root TEXT PRIMARY KEY);
      CREATE TABLE IF NOT EXISTS devices (seq INTEGER PRIMARY KEY, body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS heads (seq INTEGER PRIMARY KEY, body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS epochs (epoch INTEGER PRIMARY KEY, body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS grants (device TEXT, epoch INTEGER, body TEXT NOT NULL, PRIMARY KEY (device, epoch));
      CREATE TABLE IF NOT EXISTS requests (device TEXT PRIMARY KEY, body TEXT NOT NULL, created INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS objects (path TEXT PRIMARY KEY, size INTEGER NOT NULL);
    `);
  }

  private rows<T>(query: string, ...args: SqlStorageValue[]): T[] {
    return this.sql.exec<{ body: string }>(query, ...args).toArray().map((r) => JSON.parse(r.body) as T);
  }
  private root(): string | null {
    return this.sql.exec<{ root: string }>("SELECT root FROM anchor").toArray()[0]?.root ?? null;
  }
  private devices(): DeviceEntry[] {
    return this.rows("SELECT body FROM devices ORDER BY seq");
  }
  private heads(after = 0): Head[] {
    return this.rows("SELECT body FROM heads WHERE seq > ? ORDER BY seq", after);
  }
  private lastHead(): Head | undefined {
    return this.rows<Head>("SELECT body FROM heads ORDER BY seq DESC LIMIT 1")[0];
  }
  private epochs(): EpochRecord[] {
    return this.rows("SELECT body FROM epochs ORDER BY epoch");
  }

  private objectSize(path: string): number {
    return this.sql.exec<{ size: number }>("SELECT size FROM objects WHERE path = ?", path).toArray()[0]?.size ?? 0;
  }

  private quota(): number {
    return Number(this.env.STORAGE_QUOTA_BYTES ?? 0) || 0;
  }

  /** Bytes stored in one epoch's repository. */
  private usedIn(epoch: number): number {
    const row =
      epoch === 0
        ? this.sql.exec<{ n: number | null }>("SELECT sum(size) AS n FROM objects WHERE path NOT GLOB 'e[0-9]*/*'").one()
        : this.sql.exec<{ n: number | null }>("SELECT sum(size) AS n FROM objects WHERE path GLOB ?", `e${epoch}/*`).one();
    return row.n ?? 0;
  }

  /** The current epoch's usage, which the quota applies to. */
  private usage(): { used: number; quota: number } {
    return { used: this.usedIn(this.epochs().at(-1)?.epoch ?? 0), quota: this.quota() };
  }

  private who(key: string | null): Who {
    if (!key) return "anonymous";
    if (key === this.root()) return "root";
    const m = membersAt(this.devices(), Number.MAX_SAFE_INTEGER);
    if (m.valid.has(key)) return "member";
    if (m.revoked.has(key)) return "revoked";
    const pending = this.sql.exec("SELECT 1 FROM requests WHERE device = ?", key).toArray().length > 0;
    return pending ? "pending" : "anonymous";
  }

  private need(who: Who, ...allowed: Who[]) {
    if (who === "revoked") throw new HttpError(403, "revoked", "this device was removed from the account");
    if (!allowed.includes(who)) throw new HttpError(403, "forbidden");
  }

  async fetch(request: Request): Promise<Response> {
    try {
      return await this.route(request);
    } catch (e) {
      if (e instanceof HttpError) return json({ error: e.code, message: e.message }, e.status);
      if (e instanceof SyntaxError) return json({ error: "bad json" }, 400);
      throw e;
    }
  }

  private async route(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const root = request.headers.get("x-account-root") ?? "";
    const rest = url.pathname.replace(/^\/v1\/accounts\/[0-9a-f]{64}\/?/, "");
    const body = new Uint8Array(await request.arrayBuffer());
    const key = await requestKey(
      request.headers.get("authorization"),
      request.method,
      url.pathname + url.search,
      body,
      Math.floor(Date.now() / 1000),
    );
    const who = this.who(key);
    const read = <T>() => JSON.parse(new TextDecoder().decode(body)) as T;
    const after = Number(url.searchParams.get("after") ?? "0");
    const route = `${request.method} ${rest}`;

    switch (true) {
      case route === "GET anchor": {
        const r = this.root();
        return r ? json({ root: r }) : json({ error: "no account" }, 404);
      }
      case route === "PUT anchor": {
        // until billing exists, new accounts need an invite
        const invite = this.env.INVITE_CODE;
        if (invite && request.headers.get("x-onecloud-invite") !== invite) {
          throw new HttpError(403, "invite", "creating an account needs an invite code");
        }
        const a = read<{ root: string }>();
        if (a.root !== root) throw new HttpError(422, "anchor names another root");
        if (this.root()) return json({ ok: false }, 409);
        this.sql.exec("INSERT INTO anchor (root) VALUES (?)", root);
        return json({ ok: true });
      }

      case route === "GET devices":
        this.need(who, "member", "root", "pending");
        return json(this.devices().filter((e) => e.seq > after));
      case route === "POST devices": {
        const e = read<DeviceEntry>();
        const r = this.root();
        if (!r) throw new HttpError(404, "no account");
        const entries = this.devices();
        if (e.seq !== entries.length + 1) return json({ ok: false }, 409);
        const why = await checkDevice(r, entries, e);
        if (why) throw new HttpError(422, why);
        this.sql.exec("INSERT INTO devices (seq, body) VALUES (?, ?)", e.seq, JSON.stringify(e));
        return json({ ok: true });
      }

      case route === "GET heads":
        this.need(who, "member", "root");
        return json(this.heads(after));
      case route === "GET heads/latest":
        this.need(who, "member", "root");
        return json(this.lastHead() ?? null);
      case route === "GET heads/wait": {
        this.need(who, "member", "root");
        const latest = this.lastHead()?.seq ?? 0;
        if (latest > after) return json({ seq: latest });
        const seq = await new Promise<number>((resolve) => {
          const timer = setTimeout(() => resolve(latest), WAIT_MS);
          this.waiters.push((s) => {
            clearTimeout(timer);
            resolve(s);
          });
        });
        return json({ seq });
      }
      case route === "POST heads": {
        const h = read<Head>();
        const epochs = this.epochs();
        if (h.epoch !== (epochs.at(-1)?.epoch ?? 0)) throw new HttpError(422, "head is not in the current epoch");
        return this.appendHead(h);
      }
      case route === "POST rotation": {
        // a rotation's head and its key record land together, so no device
        // ever sees a new epoch without its keys
        const { head: h, record: r } = read<{ head: Head; record: EpochRecord }>();
        const epochs = this.epochs();
        const current = epochs.at(-1)?.epoch ?? 0;
        if (h.epoch !== current + 1 || r.epoch !== h.epoch || r.head !== h.seq) {
          throw new HttpError(422, "rotation head and record don't match");
        }
        const why = await checkEpoch(epochs, r, this.devices());
        if (why) throw new HttpError(422, why);
        return this.appendHead(h, () => {
          this.sql.exec("INSERT INTO epochs (epoch, body) VALUES (?, ?)", r.epoch, JSON.stringify(r));
        });
      }

      case route === "GET epochs":
        this.need(who, "member", "root", "pending");
        return json(this.epochs());
      case route === "POST epochs": {
        // only the first record, at account creation; later ones come with a
        // rotation head
        const r = read<EpochRecord>();
        const epochs = this.epochs();
        if (epochs.length > 0) return json({ ok: false }, 409);
        if (r.epoch !== 0) throw new HttpError(422, "later epochs start with a rotation");
        const why = await checkEpoch(epochs, r, this.devices());
        if (why) throw new HttpError(422, why);
        this.sql.exec("INSERT INTO epochs (epoch, body) VALUES (?, ?)", r.epoch, JSON.stringify(r));
        return json({ ok: true });
      }

      case route.startsWith("GET grants/"): {
        const device = rest.slice("grants/".length);
        if (key !== device) this.need(who, "member", "root");
        else this.need(who, "member", "root", "pending");
        return json(this.rows("SELECT body FROM grants WHERE device = ? ORDER BY epoch", device));
      }
      case route === "PUT grants": {
        const g = read<Grant>();
        const r = this.root();
        const m = membersAt(this.devices(), Number.MAX_SAFE_INTEGER);
        if (!(g.signer === r || m.valid.has(g.signer))) throw new HttpError(403, "grant not signed by a member");
        if (!(await grantSigned(g))) throw new HttpError(422, "bad signature");
        this.sql.exec(
          "INSERT OR REPLACE INTO grants (device, epoch, body) VALUES (?, ?, ?)",
          g.device,
          g.epoch,
          JSON.stringify(g),
        );
        return json({ ok: true });
      }

      case route === "GET requests":
        this.need(who, "member", "root");
        return json(this.rows("SELECT body FROM requests ORDER BY created"));
      case route === "POST requests": {
        const j = read<JoinRequest>();
        if (!(await joinSigned(j))) throw new HttpError(422, "bad signature");
        // the joining device names the root it was shown; one naming another
        // root would be refused at approval anyway
        if (j.root !== root) throw new HttpError(422, "request names another account");
        const count = this.sql.exec<{ n: number }>("SELECT count(*) AS n FROM requests").one().n;
        if (count >= MAX_REQUESTS) throw new HttpError(429, "too many pending requests");
        this.sql.exec(
          "INSERT OR REPLACE INTO requests (device, body, created) VALUES (?, ?, ?)",
          j.device,
          JSON.stringify(j),
          Date.now(),
        );
        return json({ ok: true });
      }
      // storage, for accounts on the managed tiers: presigned URLs for data,
      // listing and deleting through here; members only
      case route === "POST storage/sign": {
        this.need(who, "member");
        const { ops } = read<{ ops: Array<{ method: "GET" | "PUT"; path: string; size?: number }> }>();
        if (!Array.isArray(ops) || ops.length > MAX_STORAGE_OPS) throw new HttpError(422, "bad operations");
        // uploads go to the current epoch, or the next one while a rotation
        // copies into it; each epoch is held to the quota on its own, so a
        // rotation can always complete, and old epochs only shrink
        const current = this.epochs().at(-1)?.epoch ?? 0;
        const adding = new Map<number, number>();
        for (const op of ops) {
          if ((op.method !== "GET" && op.method !== "PUT") || !validPath(op.path)) {
            throw new HttpError(422, `bad path ${op.path}`);
          }
          if (op.method === "PUT") {
            if (!Number.isSafeInteger(op.size) || (op.size as number) < 0) throw new HttpError(422, "a PUT needs its size");
            const e = epochOf(op.path);
            if (e !== current && e !== current + 1) throw new HttpError(422, `epoch ${e} is not open for writing`);
            adding.set(e, (adding.get(e) ?? 0) + (op.size as number) - this.objectSize(op.path));
          }
        }
        const quota = this.quota();
        for (const [e, bytes] of adding) {
          const used = this.usedIn(e);
          if (quota > 0 && used + bytes > quota) {
            throw new HttpError(413, "quota", `storage full: ${used} of ${quota} bytes used`);
          }
        }
        const storage = new Storage(this.env, root);
        const urls = await Promise.all(ops.map((op) => storage.sign(op.method, op.path, op.size)));
        // counted when signed: the URL only accepts exactly this size
        for (const op of ops) {
          if (op.method === "PUT") {
            this.sql.exec("INSERT OR REPLACE INTO objects (path, size) VALUES (?, ?)", op.path, op.size as number);
          }
        }
        return json({ urls });
      }
      case route === "GET storage/usage":
        this.need(who, "member", "root");
        return json(this.usage());
      case route === "POST storage/reconcile": {
        // recount from the bucket, dropping uploads that never happened
        this.need(who, "member");
        const objects = await new Storage(this.env, root).listAll();
        this.ctx.storage.transactionSync(() => {
          this.sql.exec("DELETE FROM objects");
          for (const o of objects) this.sql.exec("INSERT INTO objects (path, size) VALUES (?, ?)", o.path, o.size);
        });
        return json(this.usage());
      }
      case route === "GET storage/list": {
        this.need(who, "member");
        const prefix = url.searchParams.get("prefix") ?? "";
        if (!validPrefix(prefix)) throw new HttpError(422, "bad prefix");
        return json(await new Storage(this.env, root).list(prefix));
      }
      case route === "POST storage/remove": {
        this.need(who, "member");
        const { paths } = read<{ paths: string[] }>();
        if (!Array.isArray(paths) || paths.length > MAX_STORAGE_OPS || !paths.every(validPath)) {
          throw new HttpError(422, "bad paths");
        }
        await new Storage(this.env, root).remove(paths);
        for (const p of paths) this.sql.exec("DELETE FROM objects WHERE path = ?", p);
        return json({ ok: true });
      }

      case route === "DELETE account": {
        // everything goes: the bucket objects first, then this object's
        // records. The data was already unreadable to anyone without the
        // keys; now it is gone too.
        this.need(who, "member", "root");
        if (this.env.STORAGE_ENDPOINT) {
          const storage = new Storage(this.env, root);
          const objects = await storage.listAll();
          for (let i = 0; i < objects.length; i += MAX_STORAGE_OPS) {
            await storage.remove(objects.slice(i, i + MAX_STORAGE_OPS).map((o) => o.path));
          }
        }
        for (const wake of this.waiters.splice(0)) wake(0);
        await this.ctx.storage.deleteAll();
        this.migrate(); // this object lives on, empty
        return json({ ok: true });
      }

      case route.startsWith("DELETE requests/"):
        this.need(who, "member", "root");
        this.sql.exec("DELETE FROM requests WHERE device = ?", rest.slice("requests/".length));
        return json({ ok: true });
    }
    return json({ error: "not found" }, 404);
  }

  /** Append `h` as the next head, with `also` in the same transaction. */
  private async appendHead(h: Head, also?: () => void): Promise<Response> {
    const prev = this.lastHead();
    if (h.seq !== (prev?.seq ?? 0) + 1) return json({ ok: false }, 409);
    const entries = this.devices();
    // clients accept heads from any device valid at the list the head names;
    // the service also refuses devices removed since, closing the gap until
    // an honest device pushes after a revocation
    if (membersAt(entries, entries.length).revoked.has(h.device)) {
      throw new HttpError(403, "revoked", "this device was removed from the account");
    }
    const why = await checkHead(prev, h, entries);
    if (why) throw new HttpError(422, why);
    this.ctx.storage.transactionSync(() => {
      this.sql.exec("INSERT INTO heads (seq, body) VALUES (?, ?)", h.seq, JSON.stringify(h));
      also?.();
    });
    for (const wake of this.waiters.splice(0)) wake(h.seq);
    return json({ ok: true });
  }
}
