// Secrets (`wrangler secret put`), which `wrangler types` doesn't list.
interface Secrets {
  /** New accounts need this code until billing exists; unset allows anyone. */
  INVITE_CODE?: string;
  STORAGE_KEY_ID?: string;
  STORAGE_SECRET?: string;
}
interface Env extends Secrets {}
declare namespace Cloudflare {
  interface Env extends Secrets {}
}
