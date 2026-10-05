# Omacloud storage

The service behind Sign In with Google: a Cloudflare Worker at
omacloud.computer (sign-in, and the home, privacy and terms pages) and
storage.omacloud.computer (an S3 gateway to one Hetzner bucket, a folder per
account). It never sees anything decryptable: computers encrypt before they
upload. `docs/design.md` has the model; `src/gateway.ts` says what gets
through.

- `src/`: the Worker. `migrations/`: the D1 schema (accounts, keys,
  invites, sign-ins), applied with `cf d1 migrations apply <database id>`.
- `cloudflare.config.ts`: the Worker's configuration, for `cf`.
- `deploy.sh`: deploys with `cf`. Secrets are typed at a hidden prompt and
  piped to Cloudflare; the master key, which account keys derive from, is
  made once by `--first` and never replaced.
- `npm run check`: types and unit tests. `scripts/gateway-sync.sh` (from the
  repository root) runs the whole thing locally: SeaweedFS as the bucket, a
  stand-in for Google, computers signing in and syncing.

Running it, with `cf` signed in:

```sh
db=414e15fe-bc1b-4936-9754-ada7e8634ea1
cf d1 query $db --sql "INSERT INTO invites (email, created) VALUES ('someone@example.com', unixepoch())"
cf d1 query $db --sql "SELECT id, email, used, quota, disabled FROM accounts"
cf d1 query $db --sql "UPDATE accounts SET quota = 1000000000000 WHERE email = 'someone@example.com'"
cf d1 query $db --sql "UPDATE accounts SET disabled = 1 WHERE email = 'someone@example.com'"
```
