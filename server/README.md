# Omacloud storage

The service behind Sign In with Google: a Cloudflare Worker at
omacloud.computer (sign-in, and the home, privacy and terms pages) and
storage.omacloud.computer (an S3 gateway to one Hetzner bucket, a folder per
account). It never sees anything decryptable: computers encrypt before they
upload. `docs/design.md` has the model; `src/gateway.ts` says what gets
through.

- `src/`: the Worker. `migrations/`: the D1 schema (accounts, keys,
  invites, the waitlist, sign-ins, trusted contact pads, the trash and
  restores), applied with `cf d1 migrations apply <database id>`.
- `cloudflare.config.ts`: the Worker's configuration, for `cf`.
- `invite.sh`: the waitlist and invitations (below).
- `restore.sh`: putting an account back as it was (below).
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

Whatever a key deletes or overwrites (a delete, a batch delete, a put or a
finished upload over an object) is first copied, in the bucket, to
`.trash/<account>/<time>/<key>`, outside every account's folder, where no
key reaches; if the copy fails, so does the request. Restic's locks and the
`changed` marker skip it (`src/trash.ts` says why). Objects are at most
5 GiB, so every one can be copied. The trash is kept 14 days, all of it,
and isn't counted in the quota, but is capped at it: once an account's
trash holds its quota's worth, the gateway refuses its deletes and
overwrites (`TooMuchDeleted`; new objects are still made) until some
expires, and emails the admin, once a day, naming the account by id. To
put an account back as it was at a time in those 14 days (after a removed
computer, or someone in the owner's Google account, deleted everything):

```sh
server/restore.sh someone@example.com 2026-10-07T09:30:00Z   # or an account id, or unix seconds
server/restore.sh status                                     # the latest, and when each was done
```

A restore puts back everything deleted or overwritten after that time, as
it was then (and anything made after it and deleted since, as it was last).
It removes nothing: files added after the time stay, as restic ignores
files nothing refers to, and computers that saw newer coordination records
would take their removal for history rolled back and refuse to sync.
Junk someone added stays too, and is for the admin to remove. What a
restore replaces goes to the trash too. Join requests aren't kept (a
restore would bring back ones already answered), so none come back; an
older grant or key acknowledgment can, which changes nothing. If whoever did it may still
hold a key, close the account first and open it again once it's done. The
account's trash isn't purged while a restore waits; a restore that fails 5
runs in a row is given up on (the admin is emailed, and `status` says so),
and the next goes ahead. Deleting an account's folder from the bucket
directly leaves its trash to purge within 14 days.

The scheduled job (every 15 minutes) sends the invitation emails from
invites@omacloud.computer, the daily count, and forgets typed addresses
whose link expired; it carries out restores a batch at a time, purges the
trash and counts usage. Running it, with `cf` signed in:

```sh
db=414e15fe-bc1b-4936-9754-ada7e8634ea1
cf d1 query $db --sql "SELECT id, email, used, quota, disabled FROM accounts"
cf d1 query $db --sql "UPDATE accounts SET quota = 1000000000000 WHERE email = 'someone@example.com'"
cf d1 query $db --sql "UPDATE accounts SET disabled = 1 WHERE email = 'someone@example.com'"
```
