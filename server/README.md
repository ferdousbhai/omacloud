# omacloud.computer

A Cloudflare Worker serving Omacloud's public pages: home, privacy, terms,
`/llms.txt` and `/health`. It keeps no logs (observability is off), sets
no cookies and has no accounts or secrets. The storage network's API will
live in `src/net/`, using the D1 database `DB`.

- `src/`: the Worker (`pages.ts` the pages; `limit.ts` rate limiting,
  kept for the network API).
- `migrations/`: the D1 schema, applied with
  `cf d1 migrations apply <database id>`. `0005_drop_hosted.sql` drops the
  tables of the hosted storage, Google sign-in and waitlist that came before.
- `cloudflare.config.ts`: the Worker's configuration, for `cf`.
- `deploy.sh`: type checks, applies new migrations (the code expects them),
  then deploys with `cf`.
- `npm run check`: types and unit tests.
