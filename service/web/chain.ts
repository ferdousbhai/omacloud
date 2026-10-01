// The device list as the page shows it: verified against the root derived
// from the recovery code, entry by entry, with the rules the service and
// every device apply. A service that edited the list would fail here.

import { type DeviceEntry, checkDevice, deviceBytes, deviceHash, membersAt } from "../src/proto";
import type { Signer } from "./keys";

/** Why `entries` don't form this root's device chain; null if they do. */
export async function verifyChain(root: string, entries: DeviceEntry[]): Promise<string | null> {
  for (let i = 0; i < entries.length; i++) {
    const why = await checkDevice(root, entries.slice(0, i), entries[i]);
    if (why) return `entry ${i + 1}: ${why}`;
  }
  return null;
}

/** The entry that removes `device`, signed by the root, to follow `entries`. */
export async function revokeEntry(root: Signer, entries: DeviceEntry[], device: string): Promise<DeviceEntry> {
  const last = entries.at(-1);
  const e: DeviceEntry = {
    seq: entries.length + 1,
    prev: last ? await deviceHash(last) : "",
    action: "revoke",
    device,
    signer: root.publicKey,
    sig: "",
  };
  e.sig = await root.sign(deviceBytes(e));
  return e;
}

export const members = (entries: DeviceEntry[]) => membersAt(entries, entries.length);
