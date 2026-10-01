// Signed formats and rules, mirroring onecloud-core (head.rs, devices.rs,
// epoch.rs, auth.rs). The service holds writes to the same rules clients
// apply, so a misbehaving device is refused at the door, not only by its
// peers. test/proto.test.ts checks these against fixtures signed by Rust.

export type Action = { action: "add"; device: string; name: string } | { action: "revoke"; device: string };

export type DeviceEntry = Action & { seq: number; prev: string; signer: string; sig: string };

export type Head = {
  seq: number;
  snapshot: string;
  prev: string;
  devices: number;
  epoch: number;
  device: string;
  sig: string;
};

export type EpochRecord = {
  epoch: number;
  prev: string;
  head: number;
  devices: number;
  keys: Record<string, string>;
  snapshots: Record<string, string>;
  signer: string;
  sig: string;
};

export type Grant = { epoch: number; device: string; sealed: string; signer: string; sig: string };

export type JoinRequest = { device: string; name: string; root: string; sig: string };

const enc = new TextEncoder();

export function hex(bytes: ArrayBuffer | Uint8Array): string {
  return [...new Uint8Array(bytes)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

function unhex(s: string): Uint8Array<ArrayBuffer> | null {
  if (!/^([0-9a-f]{2})*$/.test(s)) return null;
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(s.slice(2 * i, 2 * i + 2), 16);
  return out;
}

export async function sha256(data: Uint8Array | string): Promise<string> {
  const bytes = typeof data === "string" ? enc.encode(data) : data;
  return hex(await crypto.subtle.digest("SHA-256", bytes as Uint8Array<ArrayBuffer>));
}

export async function verify(keyHex: string, sigHex: string, msg: Uint8Array): Promise<boolean> {
  const key = unhex(keyHex);
  const sig = unhex(sigHex);
  if (!key || key.length !== 32 || !sig || sig.length !== 64) return false;
  try {
    const k = await crypto.subtle.importKey("raw", key, { name: "Ed25519" }, false, ["verify"]);
    return await crypto.subtle.verify({ name: "Ed25519" }, k, sig, msg as Uint8Array<ArrayBuffer>);
  } catch {
    return false;
  }
}

// serde_json's compact output: JSON.stringify escapes strings the same way
// (quotes, backslashes, control characters; other text as is).
const str = JSON.stringify;

function map(m: Record<string, string>): string {
  // BTreeMap order: by bytes, which for these ASCII hex keys is plain sort
  const keys = Object.keys(m).sort();
  return "{" + keys.map((k) => `${str(k)}:${str(m[k])}`).join(",") + "}";
}

function actionJson(a: Action): string {
  return a.action === "add"
    ? `"action":"add","device":${str(a.device)},"name":${str(a.name)}`
    : `"action":"revoke","device":${str(a.device)}`;
}

export function deviceBytes(e: DeviceEntry): Uint8Array {
  return enc.encode(
    `{"domain":"onecloud-device-v1","seq":${e.seq},"prev":${str(e.prev)},${actionJson(e)},"signer":${str(e.signer)}}`,
  );
}

export function headBytes(h: Head): Uint8Array {
  return enc.encode(`onecloud-head-v3\n${h.seq}\n${h.snapshot}\n${h.prev}\n${h.devices}\n${h.epoch}\n${h.device}\n`);
}

export function epochBytes(r: EpochRecord): Uint8Array {
  return enc.encode(
    `{"domain":"onecloud-epoch-v1","epoch":${r.epoch},"prev":${str(r.prev)},"head":${r.head},` +
      `"devices":${r.devices},"keys":${map(r.keys)},"snapshots":${map(r.snapshots)},"signer":${str(r.signer)}}`,
  );
}

export function grantBytes(g: Grant): Uint8Array {
  return enc.encode(`onecloud-grant-v1\n${g.epoch}\n${g.device}\n${g.sealed}\n${g.signer}\n`);
}

export function joinBytes(j: JoinRequest): Uint8Array {
  return enc.encode(`onecloud-join-v1\n${j.device}\n${j.name}\n${j.root}\n`);
}

export async function authBytes(method: string, path: string, ts: number, body: Uint8Array): Promise<Uint8Array> {
  return enc.encode(`onecloud-auth-v1\n${method}\n${path}\n${ts}\n${await sha256(body)}\n`);
}

/** `hash()` of a signed object: SHA-256 over its signed bytes, then its signature (hex text). */
async function hashOf(bytes: Uint8Array, sig: string): Promise<string> {
  const sigBytes = enc.encode(sig);
  const all = new Uint8Array(bytes.length + sigBytes.length);
  all.set(bytes);
  all.set(sigBytes, bytes.length);
  return sha256(all);
}

export const deviceHash = (e: DeviceEntry) => hashOf(deviceBytes(e), e.sig);
export const headHash = (h: Head) => hashOf(headBytes(h), h.sig);
export const epochHash = (r: EpochRecord) => hashOf(epochBytes(r), r.sig);

export type Members = { valid: Map<string, string>; revoked: Map<string, string> };

/** Members after the first `seq` entries. */
export function membersAt(entries: DeviceEntry[], seq: number): Members {
  const m: Members = { valid: new Map(), revoked: new Map() };
  for (const e of entries.slice(0, seq)) {
    if (e.action === "add") m.valid.set(e.device, e.name);
    else {
      const name = m.valid.get(e.device);
      if (name !== undefined) {
        m.valid.delete(e.device);
        m.revoked.set(e.device, name);
      }
    }
  }
  return m;
}

/** Why an entry can't follow `entries`; null if it can. Same rules as DeviceChain::push. */
export async function checkDevice(root: string, entries: DeviceEntry[], e: DeviceEntry): Promise<string | null> {
  const last = entries.at(-1);
  if (e.seq !== entries.length + 1 || e.prev !== (last ? await deviceHash(last) : "")) return "broken";
  if (!(await verify(e.signer, e.sig, deviceBytes(e)))) return "bad signature";
  const m = membersAt(entries, entries.length);
  const byRoot = e.signer === root;
  if (entries.length === 0 && !(byRoot && e.action === "add")) return "bad genesis";
  if (!byRoot && !m.valid.has(e.signer)) return "unauthorized signer";
  if (e.action === "add" && (m.valid.has(e.device) || m.revoked.has(e.device) || e.device === root)) return "bad add";
  if (e.action === "revoke" && !m.valid.has(e.device)) return "bad revoke";
  return null;
}

/** Why a head can't follow `prev`; null if it can. Same rules as head::check. */
export async function checkHead(prev: Head | undefined, h: Head, entries: DeviceEntry[]): Promise<string | null> {
  if (h.seq !== (prev?.seq ?? 0) + 1 || h.prev !== (prev ? await headHash(prev) : "")) return "broken";
  if (!(await verify(h.device, h.sig, headBytes(h)))) return "bad signature";
  if (h.devices > entries.length) return "unknown devices";
  if (prev && h.devices < prev.devices) return "stale devices";
  if (prev && h.epoch < prev.epoch) return "stale epoch";
  if (!membersAt(entries, h.devices).valid.has(h.device)) return "untrusted device";
  return null;
}

/** Why a record can't follow `records`; null if it can. Same rules as EpochChain::push. */
export async function checkEpoch(records: EpochRecord[], r: EpochRecord, entries: DeviceEntry[]): Promise<string | null> {
  const last = records.at(-1);
  if (r.epoch !== (last ? last.epoch + 1 : 0) || r.prev !== (last ? await epochHash(last) : "")) return "broken";
  if (!(await verify(r.signer, r.sig, epochBytes(r)))) return "bad signature";
  if (r.devices > entries.length || !membersAt(entries, r.devices).valid.has(r.signer)) return "untrusted signer";
  return null;
}

export async function grantSigned(g: Grant): Promise<boolean> {
  return verify(g.signer, g.sig, grantBytes(g));
}

export async function joinSigned(j: JoinRequest): Promise<boolean> {
  return verify(j.device, j.sig, joinBytes(j));
}

/** The key that signed a request, if the `Authorization` header checks out. */
export async function requestKey(
  header: string | null,
  method: string,
  path: string,
  body: Uint8Array,
  now: number,
): Promise<string | null> {
  const m = header?.match(/^OneCloud ([0-9a-f]{64}) (\d+) ([0-9a-f]{128})$/);
  if (!m) return null;
  const [, key, tsText, sig] = m;
  const ts = Number(tsText);
  if (Math.abs(now - ts) > 300) return null;
  return (await verify(key, sig, await authBytes(method, path, ts, body))) ? key : null;
}
