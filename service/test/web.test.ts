import { describe, expect, it } from "vitest";
import { type DeviceEntry, checkDevice } from "../src/proto";
import { revokeEntry, verifyChain } from "../web/chain";
import { fingerprint, normalize, rootFromCode, signerFromSeed } from "../web/keys";
import fixtures from "./fixtures.json";

const devices = fixtures.devices as DeviceEntry[];

describe("the web app's keys", () => {
  it("derives the root from a recovery code as onecloud-core does", async () => {
    const root = await rootFromCode(fixtures.recovery.code);
    expect(root?.publicKey).toBe(fixtures.recovery.root);
    expect(await fingerprint(fixtures.recovery.root)).toBe(fixtures.recovery.fingerprint);
    // case, spaces and dashes don't matter; length does
    expect((await rootFromCode(`  ${fixtures.recovery.code.toUpperCase().replace(/-/g, " ")} `))?.publicKey).toBe(
      fixtures.recovery.root,
    );
    expect(await rootFromCode("abcd-efgh")).toBeNull();
    expect(normalize("Ab-12 cD")).toBe("ab12cd");
  });

  it("verifies the device chain and signs a revoke the rules accept", async () => {
    const root = await signerFromSeed(new Uint8Array(32).fill(1));
    expect(root.publicKey).toBe(fixtures.root);
    expect(await verifyChain(fixtures.root, devices)).toBeNull();
    // a list the service tampered with doesn't verify
    const renamed = devices.map((e, i) => (i === 0 ? { ...e, name: "evil" } : e));
    expect(await verifyChain(fixtures.root, renamed)).toMatch(/entry 1/);
    expect(await verifyChain(fixtures.root, devices.slice(1))).toMatch(/entry 1/);

    const valid = devices.find((e) => e.action === "add" && !devices.some((r) => r.action === "revoke" && r.device === e.device));
    const e = await revokeEntry(root, devices, valid!.device);
    expect(await checkDevice(fixtures.root, devices, e)).toBeNull();
    // only the root's key signs for the root
    const other = await signerFromSeed(new Uint8Array(32).fill(9));
    expect(await checkDevice(fixtures.root, devices, await revokeEntry(other, devices, valid!.device))).toBe(
      "unauthorized signer",
    );
  });
});
