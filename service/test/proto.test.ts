// Objects signed by onecloud-core (crates/onecloud-core/tests/fixtures.rs)
// must verify here byte for byte, and tampered ones must not.

import { describe, expect, it } from "vitest";
import fixtures from "./fixtures.json";
import {
  type DeviceEntry,
  type EpochRecord,
  type Head,
  checkDevice,
  checkEpoch,
  checkHead,
  deviceHash,
  epochHash,
  grantSigned,
  headHash,
  joinSigned,
  requestKey,
} from "../src/proto";

const devices = fixtures.devices as DeviceEntry[];
const heads = fixtures.heads as Head[];
const epoch = fixtures.epoch as EpochRecord;
const enc = new TextEncoder();

describe("formats match onecloud-core", () => {
  it("hashes device entries, heads and records the same way", async () => {
    expect(await Promise.all(devices.map(deviceHash))).toEqual(fixtures.device_hashes);
    expect(await Promise.all(heads.map(headHash))).toEqual(fixtures.head_hashes);
    expect(await epochHash(epoch)).toBe(fixtures.epoch_hash);
  });

  it("verifies the device chain, including a non-ASCII quoted name", async () => {
    for (let i = 0; i < devices.length; i++) {
      expect(await checkDevice(fixtures.root, devices.slice(0, i), devices[i])).toBeNull();
    }
    const renamed = { ...devices[0], name: "laptop" } as DeviceEntry;
    expect(await checkDevice(fixtures.root, [], renamed)).toBe("bad signature");
  });

  it("verifies heads, records, grants, join requests and request signatures", async () => {
    expect(await checkHead(undefined, heads[0], devices)).toBeNull();
    expect(await checkHead(heads[0], heads[1], devices)).toBeNull();
    expect(await checkEpoch([], epoch, devices)).toBeNull();
    expect(await grantSigned(fixtures.grant)).toBe(true);
    expect(await joinSigned(fixtures.join)).toBe(true);
    const { method, path, ts, body, header } = fixtures.auth;
    expect(await requestKey(header, method, path, enc.encode(body), ts)).toBe(fixtures.a);
  });

  it("refuses tampering", async () => {
    expect(await checkHead(heads[0], { ...heads[1], snapshot: "evil" }, devices)).toBe("bad signature");
    expect(await checkEpoch([], { ...epoch, snapshots: { aa: "evil" } }, devices)).toBe("bad signature");
    expect(await grantSigned({ ...fixtures.grant, device: fixtures.a })).toBe(false);
    expect(await joinSigned({ ...fixtures.join, root: fixtures.a })).toBe(false);
    const { method, path, ts, body, header } = fixtures.auth;
    expect(await requestKey(header, method, path, enc.encode(body + " "), ts)).toBeNull();
    expect(await requestKey(header, "GET", path, enc.encode(body), ts)).toBeNull();
    expect(await requestKey(header, method, path, enc.encode(body), ts + 301)).toBeNull();
  });
});
