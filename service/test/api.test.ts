// The coordinator API end to end, with objects signed here the way
// onecloud-core signs them.

import { SELF } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import {
  type Action,
  type DeviceEntry,
  type EpochRecord,
  type Head,
  type JoinRequest,
  authBytes,
  deviceBytes,
  deviceHash,
  epochBytes,
  headBytes,
  headHash,
  hex,
  joinBytes,
} from "../src/proto";

type Key = { pub: string; priv: CryptoKey };

async function newKey(): Promise<Key> {
  const pair = (await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"])) as CryptoKeyPair;
  const raw = await crypto.subtle.exportKey("raw", pair.publicKey);
  return { pub: hex(raw as ArrayBuffer), priv: pair.privateKey };
}

async function sign(k: Key, bytes: Uint8Array): Promise<string> {
  return hex(await crypto.subtle.sign({ name: "Ed25519" }, k.priv, bytes));
}

async function entry(signer: Key, prev: DeviceEntry | undefined, action: Action): Promise<DeviceEntry> {
  const e = { ...action, seq: (prev?.seq ?? 0) + 1, prev: prev ? await deviceHash(prev) : "", signer: signer.pub, sig: "" };
  e.sig = await sign(signer, deviceBytes(e as DeviceEntry));
  return e as DeviceEntry;
}

async function head(signer: Key, prev: Head | undefined, devices: number, epoch = 0): Promise<Head> {
  const h: Head = {
    seq: (prev?.seq ?? 0) + 1,
    snapshot: `snap-${Math.random()}`,
    prev: prev ? await headHash(prev) : "",
    devices,
    epoch,
    device: signer.pub,
    sig: "",
  };
  h.sig = await sign(signer, headBytes(h));
  return h;
}

async function record(signer: Key, prev: EpochRecord | undefined, epoch: number, headSeq: number, devices: number) {
  const { epochHash } = await import("../src/proto");
  const r: EpochRecord = {
    epoch,
    prev: prev ? await epochHash(prev) : "",
    head: headSeq,
    devices,
    keys: { [signer.pub]: "sealed" },
    snapshots: {},
    signer: signer.pub,
    sig: "",
  };
  r.sig = await sign(signer, epochBytes(r));
  return r;
}

/** A client for one account, signing requests with `as`. */
function client(root: string) {
  const base = `https://coord.test/v1/accounts/${root}`;
  return async (method: string, path: string, body?: unknown, as?: Key, tsShift = 0, invite = "test-invite") => {
    const bytes = body === undefined ? new Uint8Array() : new TextEncoder().encode(JSON.stringify(body));
    const headers: Record<string, string> = { "content-type": "application/json", "x-onecloud-invite": invite };
    const url = new URL(base + path);
    if (as) {
      const ts = Math.floor(Date.now() / 1000) + tsShift;
      const sig = await sign(as, await authBytes(method, url.pathname + url.search, ts, bytes));
      headers.authorization = `OneCloud ${as.pub} ${ts} ${sig}`;
    }
    const res = await SELF.fetch(url, { method, headers, body: body === undefined ? undefined : bytes });
    return { status: res.status, body: (await res.json()) as any };
  };
}

/** A fresh account with root, device a (genesis) and b, and epoch 0. */
async function account() {
  const [root, a, b] = [await newKey(), await newKey(), await newKey()];
  const call = client(root.pub);
  expect((await call("PUT", "/anchor", { root: root.pub })).status).toBe(200);
  const e1 = await entry(root, undefined, { action: "add", device: a.pub, name: "a" });
  const e2 = await entry(a, e1, { action: "add", device: b.pub, name: "b" });
  expect((await call("POST", "/devices", e1)).status).toBe(200);
  expect((await call("POST", "/devices", e2)).status).toBe(200);
  const r0 = await record(a, undefined, 0, 0, 2);
  expect((await call("POST", "/epochs", r0)).status).toBe(200);
  return { root, a, b, call, e2, r0 };
}

describe("coordinator", () => {
  it("creates an account once, rooted in its own key", async () => {
    const [root, other] = [await newKey(), await newKey()];
    const call = client(root.pub);
    expect((await call("GET", "/anchor")).status).toBe(404);
    // until billing exists, creating an account takes an invite
    expect((await call("PUT", "/anchor", { root: root.pub }, undefined, 0, "wrong")).body.error).toBe("invite");
    expect((await call("PUT", "/anchor", { root: other.pub })).status).toBe(422);
    expect((await call("PUT", "/anchor", { root: root.pub })).status).toBe(200);
    expect((await call("PUT", "/anchor", { root: root.pub })).status).toBe(409);
    expect((await call("GET", "/anchor")).body).toEqual({ root: root.pub });
    // the first device must be added by the root
    const d = await newKey();
    expect((await call("POST", "/devices", await entry(d, undefined, { action: "add", device: d.pub, name: "d" }))).body.error).toBe(
      "bad genesis",
    );
  });

  it("shows history only to signed requests from members", async () => {
    const { a, call } = await account();
    const stranger = await newKey();
    expect((await call("GET", "/devices")).status).toBe(403);
    expect((await call("GET", "/devices", undefined, stranger)).status).toBe(403);
    expect((await call("GET", "/devices", undefined, a, -600)).status).toBe(403); // stale signature
    const ok = await call("GET", "/devices", undefined, a);
    expect(ok.status).toBe(200);
    expect(ok.body).toHaveLength(2);
  });

  it("appends heads by compare and swap, checked like clients check them", async () => {
    const { a, b, call } = await account();
    const h1 = await head(a, undefined, 2);
    expect((await call("POST", "/heads", h1)).status).toBe(200);
    // two devices race for seq 2: one wins, the other is told to retry
    const [x, y] = [await head(a, h1, 2), await head(b, h1, 2)];
    expect((await call("POST", "/heads", x)).status).toBe(200);
    expect((await call("POST", "/heads", y)).status).toBe(409);
    // tampered, and signed by a stranger
    const bad = { ...(await head(a, x, 2)), snapshot: "evil" };
    expect((await call("POST", "/heads", bad)).body.error).toBe("bad signature");
    expect((await call("POST", "/heads", await head(await newKey(), x, 2))).body.error).toBe("untrusted device");
    const heads = await call("GET", "/heads?after=1", undefined, b);
    expect(heads.body.map((h: Head) => h.seq)).toEqual([2]);
  });

  it("refuses heads from a removed device at once, whatever list they name", async () => {
    const { a, b, call, e2 } = await account();
    const h1 = await head(a, undefined, 2);
    await call("POST", "/heads", h1);
    const e3 = await entry(a, e2, { action: "revoke", device: b.pub });
    expect((await call("POST", "/devices", e3)).status).toBe(200);
    // clients alone would accept this until a later head names entry 3
    const r = await call("POST", "/heads", await head(b, h1, 2));
    expect(r.status).toBe(403);
    expect(r.body.error).toBe("revoked");
    expect((await call("GET", "/heads", undefined, b)).body.error).toBe("revoked");
  });

  it("takes a rotation's head and key record together, and only through rotation", async () => {
    const { a, call, r0 } = await account();
    const h1 = await head(a, undefined, 2);
    await call("POST", "/heads", h1);
    const h2 = await head(a, h1, 2, 1);
    // a new epoch can't start with a plain head, nor with a bare record
    expect((await call("POST", "/heads", h2)).status).toBe(422);
    expect((await call("POST", "/epochs", await record(a, r0, 1, 2, 2))).status).toBe(409);
    const r1 = await record(a, r0, 1, h2.seq, 2);
    expect((await call("POST", "/rotation", { head: h2, record: r1 })).status).toBe(200);
    const epochs = await call("GET", "/epochs", undefined, a);
    expect(epochs.body.map((r: EpochRecord) => r.epoch)).toEqual([0, 1]);
    // after it, heads in the old epoch are refused
    expect((await call("POST", "/heads", await head(a, h2, 2, 0))).body.error).toBe("head is not in the current epoch");
    expect((await call("POST", "/heads", await head(a, h2, 2, 1))).status).toBe(200);
  });

  it("wakes waiting devices when a head lands", async () => {
    const { a, b, call } = await account();
    const waiting = call("GET", "/heads/wait?after=0", undefined, b);
    await new Promise((r) => setTimeout(r, 50));
    await call("POST", "/heads", await head(a, undefined, 2));
    const woke = await waiting;
    expect(woke.status).toBe(200);
    expect(woke.body.seq).toBe(1);
  });

  it("presigns storage for members only, inside the account's prefix", async () => {
    const { root, a, b, call, e2 } = await account();
    const id = "ab" + "c".repeat(62);
    const ops = { ops: [{ method: "PUT", path: `data/ab/${id}`, size: 10 }, { method: "GET", path: "e1/config" }] };
    const r = await call("POST", "/storage/sign", ops, a);
    expect(r.status).toBe(200);
    const [put, get] = r.body.urls.map((u: string) => new URL(u));
    expect(put.pathname).toBe(`/onecloud-storage/accounts/${root.pub}/data/ab/${id}`);
    expect(get.pathname).toBe(`/onecloud-storage/accounts/${root.pub}/e1/config`);
    expect(put.searchParams.get("X-Amz-Signature")).toMatch(/^[0-9a-f]{64}$/);
    expect(put.searchParams.get("X-Amz-Expires")).toBe("900");
    // the upload's size is part of the signature
    expect(put.searchParams.get("X-Amz-SignedHeaders")).toContain("content-length");
    // paths outside the layout, strangers and removed devices get nothing
    expect((await call("POST", "/storage/sign", { ops: [{ method: "GET", path: "../x/config" }] }, a)).status).toBe(422);
    expect((await call("POST", "/storage/sign", ops, await newKey())).status).toBe(403);
    await call("POST", "/devices", await entry(a, e2, { action: "revoke", device: b.pub }));
    const refused = await call("POST", "/storage/sign", ops, b);
    expect(refused.status).toBe(403);
    expect(refused.body.error).toBe("revoked");
  });

  it("counts storage against the quota when it signs uploads", async () => {
    const { a, call } = await account();
    const pack = (c: string) => `data/${c}${c}/${c.repeat(64)}`;
    const put = (c: string, size: number) => call("POST", "/storage/sign", { ops: [{ method: "PUT", path: pack(c), size }] }, a);
    expect((await put("a", 600)).status).toBe(200);
    expect((await call("GET", "/storage/usage", undefined, a)).body).toEqual({ used: 600, quota: 1000 });
    // a PUT must say how big it is
    expect((await call("POST", "/storage/sign", { ops: [{ method: "PUT", path: pack("b") }] }, a)).status).toBe(422);
    const full = await put("b", 500);
    expect(full.status).toBe(413);
    expect(full.body.message).toBe("storage full: 600 of 1000 bytes used");
    // signing the same object again replaces its size instead of adding
    expect((await put("a", 900)).status).toBe(200);
    expect((await call("GET", "/storage/usage", undefined, a)).body.used).toBe(900);
    // a rotation copies into the next epoch, which has its own room
    const next = (size: number) =>
      call("POST", "/storage/sign", { ops: [{ method: "PUT", path: `e1/${pack("c")}`, size }] }, a);
    expect((await next(900)).status).toBe(200);
    expect((await next(1001)).status).toBe(413);
    // but epochs beyond the next aren't open
    const later = await call("POST", "/storage/sign", { ops: [{ method: "PUT", path: `e2/${pack("d")}`, size: 1 }] }, a);
    expect(later.body.error).toBe("epoch 2 is not open for writing");
  });

  it("keeps signed join requests, and lets requesters read only what joining needs", async () => {
    const { root, a, call } = await account();
    const joiner = await newKey();
    const req: JoinRequest = { device: joiner.pub, name: "c", root: root.pub, sig: "" };
    req.sig = await sign(joiner, joinBytes(req));
    expect((await call("POST", "/requests", { ...req, name: "trusted laptop" })).body.error).toBe("bad signature");
    const foreign = { ...req, root: a.pub };
    foreign.sig = await sign(joiner, joinBytes(foreign));
    expect((await call("POST", "/requests", foreign)).status).toBe(422);
    expect((await call("POST", "/requests", req)).status).toBe(200);
    // the requester may read the device list, epoch records and its grants
    expect((await call("GET", "/devices", undefined, joiner)).status).toBe(200);
    expect((await call("GET", `/grants/${joiner.pub}`, undefined, joiner)).status).toBe(200);
    // but not history, nor the requests of others
    expect((await call("GET", "/heads", undefined, joiner)).status).toBe(403);
    expect((await call("GET", "/requests", undefined, joiner)).status).toBe(403);
    expect((await call("GET", "/requests", undefined, a)).body).toHaveLength(1);
  });
});
