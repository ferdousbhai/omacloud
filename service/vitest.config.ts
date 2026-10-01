import { cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [
    cloudflareTest({
      wrangler: { configPath: "./wrangler.jsonc" },
      miniflare: {
        bindings: {
          STORAGE_ENDPOINT: "https://s3.test",
          STORAGE_KEY_ID: "test-key",
          STORAGE_SECRET: "test-secret",
          STORAGE_QUOTA_BYTES: "1000",
          INVITE_CODE: "test-invite",
        },
      },
    }),
  ],
  test: { include: ["test/**/*.test.ts"] },
});
