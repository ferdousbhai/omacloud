import { describe, expect, it } from "vitest";
import { validPath, validPrefix } from "../src/storage";

const id = "ab" + "c".repeat(62);

describe("storage paths", () => {
  it("allows only restic repository paths, per epoch", () => {
    for (const p of ["config", `keys/${id}`, `snapshots/${id}`, `index/${id}`, `data/ab/${id}`, `e1/config`, `e12/data/ab/${id}`]) {
      expect(validPath(p), p).toBe(true);
    }
    for (const p of [
      "",
      "../other/config",
      `data/cd/${id}`, // pack in the wrong directory
      `data/ab/${id}/x`,
      `keys/${id.toUpperCase()}`,
      `e0/config`,
      `/config`,
      `accounts/x/config`,
      `locks/${id}`,
    ]) {
      expect(validPath(p), p).toBe(false);
    }
    expect(validPrefix("data/")).toBe(true);
    expect(validPrefix("e2/keys/")).toBe(true);
    expect(validPrefix("")).toBe(false);
    expect(validPrefix("data")).toBe(false);
  });
});
