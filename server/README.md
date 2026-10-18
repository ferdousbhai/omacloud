# omacloud.computer

A Cloudflare Worker serving Omacloud's public pages (home, privacy, terms,
`/llms.txt` and `/health`) and the storage network's API under
`/api/net/` (docs/network-api.md): the node directory, each bucket's
sealed index, each account's coordination records, and storage tokens.
It keeps no logs (observability is off), sets no cookies and has no
accounts or sign-in. Writes are allowed by capabilities the computers
derive, kept only as hashes; of a request's address it keeps only the
/24 or /48, for limits.

- `src/`: the Worker (`pages.ts` the pages; `limit.ts` rate limiting).
- `src/net/`: the network API. `router.ts` the paths; `auth.ts` node
  signatures, capabilities and prefixes; `nodes.ts` the directory;
  `index.ts` the bucket index; `coord.ts` coordination; `credits.ts` blind
  RSA signatures (RFC 9474) and spent tokens; `counts.ts` per-prefix
  limits.
- `migrations/`: the D1 schema, applied with
  `cf d1 migrations apply <database id>`. `0005_drop_hosted.sql` drops the
  tables of the hosted storage, Google sign-in and waitlist that came before;
  `0006_network.sql` adds the network's.
- `cloudflare.config.ts`: the Worker's configuration, for `cf`.
  `STARTER_TOKENS` is the free tokens a prefix gets each 30 days.
- `deploy.sh`: type checks, applies new migrations (the code expects them),
  then deploys with `cf`. `deploy.sh --first` also makes the secret
  `CREDIT_KEY`.
- `npm run check`: types and unit tests (`test/d1.ts` stands in for D1).

## The credit key

`CREDIT_KEY` is the RSA key that signs storage tokens, as a private JWK
(JSON). `deploy.sh --first` makes one and hands it to `cf` through a pipe;
it's made once, since a new key voids every token out there. By hand:

```sh
node -e 'const {privateKey} = require("node:crypto").generateKeyPairSync("rsa", {modulusLength: 2048}); process.stdout.write(JSON.stringify(privateKey.export({format: "jwk"})))'
```

Computers get the public half from `GET /api/net/credits/key`.
