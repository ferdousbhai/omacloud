# Omacloud storage

The service behind Sign In with Google: a Cloudflare Worker at
omacloud.computer (sign-in, and the home, privacy and terms pages) and
storage.omacloud.computer (an S3 gateway to one Hetzner bucket, a folder per
account). It never sees anything decryptable: computers encrypt before they
upload. `docs/design.md` has the model; `src/gateway.ts` says what gets
through.

- `src/`: the Worker. `migrations/`: the D1 schema (accounts, keys,
  invites, the waitlist, sign-ins, trusted contact pads), applied with
  `cf d1 migrations apply <database id>`.
- `cloudflare.config.ts`: the Worker's configuration, for `cf`.
- `invite.sh`: the waitlist and invitations (below).
- `deploy.sh`: deploys with `cf`, applying new migrations first (the code
  expects them). It needs the Turnstile widget's site key in
  `cloudflare.config.ts`; `--turnstile` asks for its secret. Secrets are typed
  at a hidden prompt and piped to Cloudflare; the master key, which account
  keys derive from, is made once by `--first` and never replaced.
- `npm run check`: types and unit tests. `scripts/gateway-sync.sh` (from the
  repository root) runs the whole thing locally: SeaweedFS as the bucket, a
  stand-in for Google, computers signing in and syncing.

Storage is by invitation. Whoever signs in from the app without one is put
on the waitlist (Google checked the address); an address typed on the home
page joins once its owner follows the link emailed to it (Turnstile and a
rate limit keep the form for people). The admin (`ADMIN_EMAIL`) gets a
daily email when anyone joined. With `cf` signed in:

```sh
server/invite.sh list                       # the waitlist, oldest first
server/invite.sh someone@example.com ...    # invite: emailed within 15 minutes
```

The scheduled job (every 15 minutes) sends the invitation emails from
invites@omacloud.computer, the daily count, and forgets typed addresses
whose link expired; it also counts usage. Running it, with `cf` signed in:

```sh
db=414e15fe-bc1b-4936-9754-ada7e8634ea1
cf d1 query $db --sql "SELECT id, email, used, quota, disabled FROM accounts"
cf d1 query $db --sql "UPDATE accounts SET quota = 1000000000000 WHERE email = 'someone@example.com'"
cf d1 query $db --sql "UPDATE accounts SET disabled = 1 WHERE email = 'someone@example.com'"
```
