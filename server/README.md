# omacloud.computer

A Cloudflare Worker serving Omacloud's public pages (home, privacy, terms,
`/s` where share links land, `/llms.txt`, `/health`, and `/install.sh`,
a redirect to the latest release's installer) and the storage network's API under
`/api/net/` (docs/network-api.md): the node directory, each bucket's
sealed index, each account's coordination records, and storage tokens.
It keeps no logs (observability is off), sets no cookies and has no
accounts or sign-in. Writes are allowed by capabilities the computers
derive, kept only as hashes; of a request's address it keeps only the
/24 or /48, for limits.

- `src/`: the Worker (`index.ts` the paths; `pages.ts` the pages).
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
- `deploy.sh` (`npm run deploy`): type checks, applies new migrations (the
  code expects them), then deploys with `cf`. `deploy.sh --first` also makes the secret
  `CREDIT_KEY`.
- `backup.sh [dir]`: copies every table off Cloudflare as JSON, into
  `<dir>/<date>/` (default `~/omacloud-backups`), readable by you alone.
- `npm run check`: types and unit tests (`test/d1.ts` stands in for D1).

## If the database is lost

- Cloudflare keeps D1 restorable to any minute of the last 30 days (7 on
  the free plan): `npx cf d1 time-travel restore <database-id>` with a
  `--timestamp`, or a bookmark from `cf d1 time-travel get-bookmark`.
- Every account's computers keep their own sealed copy of the account's
  records and the index of its current bucket. A daemon that finds the
  records older than it saw puts its copy back; `omacloud republish` does
  it by hand. So a Worker that lost them, or a new one, is filled again by
  the accounts themselves.
- Nodes list themselves again within 15 minutes, by heartbeat.
- The set of spent credits lives only here. `backup.sh`'s copy has it; lost
  for good, tokens spent before could be traded in again.

## The credit key

`CREDIT_KEY` is the RSA key that signs storage tokens, as a private JWK
(JSON). `deploy.sh --first` makes one and hands it to `cf` through a pipe;
it's made once, since a new key voids every token out there. By hand:

```sh
node -e 'const {privateKey} = require("node:crypto").generateKeyPairSync("rsa", {modulusLength: 2048}); process.stdout.write(JSON.stringify(privateKey.export({format: "jwk"})))'
```

Computers get the public half from `GET /api/net/credits/key`.
