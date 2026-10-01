// onecloud coordinator: routes each account to its Durable Object.
//
//   /v1/accounts/<root public key, hex>/...   see src/account.ts

export { Account } from "./account";

const ACCOUNT = /^\/v1\/accounts\/([0-9a-f]{64})(\/|$)/;

export default {
  async fetch(request, env): Promise<Response> {
    const url = new URL(request.url);
    const m = url.pathname.match(ACCOUNT);
    if (!m) {
      return url.pathname === "/"
        ? new Response("onecloud coordinator\n")
        : new Response("not found\n", { status: 404 });
    }
    const root = m[1];
    const stub = env.ACCOUNT.get(env.ACCOUNT.idFromName(root));
    const forwarded = new Request(request);
    forwarded.headers.set("x-account-root", root);
    return stub.fetch(forwarded);
  },
} satisfies ExportedHandler<Env>;
