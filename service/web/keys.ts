// The account root key in the browser, from the recovery code, as
// onecloud-core derives it (devices::root_key). It lives in this page's
// memory only: requests are signed here and the code is never sent.

import { authBytes, hex, sha256 } from "../src/proto";

const enc = new TextEncoder();

/** The code as the root key reads it: letters and digits, lower case. */
export function normalize(code: string): string {
  return code.replace(/[^0-9a-z]/gi, "").toLowerCase();
}

export type Signer = { publicKey: string; sign(msg: Uint8Array): Promise<string> };

// PKCS#8 wrapping of a raw Ed25519 seed (RFC 8410)
const PKCS8_PREFIX = Uint8Array.from([
  0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
]);

/** A signer for an Ed25519 seed; the private key can't be read back out. */
export async function signerFromSeed(seed: Uint8Array): Promise<Signer> {
  const pkcs8 = new Uint8Array(PKCS8_PREFIX.length + 32);
  pkcs8.set(PKCS8_PREFIX);
  pkcs8.set(seed, PKCS8_PREFIX.length);
  const extractable = await crypto.subtle.importKey("pkcs8", pkcs8, { name: "Ed25519" }, true, ["sign"]);
  // the public half, via the JWK's x; then keep only a non-extractable copy
  const jwk = (await crypto.subtle.exportKey("jwk", extractable)) as JsonWebKey;
  const publicKey = hex(Uint8Array.from(atob(jwk.x!.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0)));
  const key = await crypto.subtle.importKey("pkcs8", pkcs8, { name: "Ed25519" }, false, ["sign"]);
  pkcs8.fill(0);
  return {
    publicKey,
    sign: async (msg) => hex(await crypto.subtle.sign({ name: "Ed25519" }, key, msg as Uint8Array<ArrayBuffer>)),
  };
}

/** The root signer for a recovery code, or null if it isn't one. */
export async function rootFromCode(code: string): Promise<Signer | null> {
  const n = normalize(code);
  if (n.length !== 28) return null;
  const seed = new Uint8Array(await crypto.subtle.digest("SHA-256", enc.encode(`onecloud-recovery-v1\n${n}`)));
  return signerFromSeed(seed);
}

/** onecloud's fingerprint of a hex public key: 4 groups of 4 hex digits. */
export async function fingerprint(keyHex: string): Promise<string> {
  const d = await sha256(keyHex);
  return d.slice(0, 16).match(/.{4}/g)!.join("-");
}

/** The `Authorization` header for a request signed by `signer`. */
export async function authorization(signer: Signer, method: string, path: string, body: Uint8Array): Promise<string> {
  const ts = Math.floor(Date.now() / 1000);
  const sig = await signer.sign(await authBytes(method, path, ts, body));
  return `OneCloud ${signer.publicKey} ${ts} ${sig}`;
}
