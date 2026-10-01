# service

The OneCloud coordinator service: a Cloudflare Worker with one Durable
Object per account. It orders and relays each account's device chain,
heads, epoch records, grants and join requests. It never sees file
contents, repository keys or storage credentials, and clients verify
everything it serves (see `../docs/design.md`).

It also serves the account page (`public/`, `web/`): sign in with the
recovery code to see storage use and the devices, cut off a lost device, or
delete the account. The code stays in the page, which derives the root key
from it and signs each request (`web/keys.ts`, pinned to Rust by
`test/fixtures.json`). The device list is verified against that root before
it is shown (`web/chain.ts`), so the service can't edit it unnoticed. A strict
Content-Security-Policy (`public/_headers`) allows only the page's own script
and requests. The remaining trust: whoever serves the page could serve a
different script, which is why adding devices and changing keys stay on
devices; a verified bundle (WEBCAT or similar) would close that.

## API

`/v1/accounts/<root public key, hex>/…`, JSON.

| route | who | what |
|---|---|---|
| `GET anchor` | anyone | the account root, if the account exists |
| `PUT anchor` | anyone | create the account; only once, only for the root in the URL |
| `GET devices?after=N`, `POST devices` | members, root, requesters / signed entry | device chain |
| `GET heads?after=N`, `GET heads/latest` | members, root | history |
| `GET heads/wait?after=N` | members, root | long poll: returns when a head passes N, or after 25 s |
| `POST heads` | signed head | append (409 if the position is taken) |
| `POST rotation` | signed head and record | a key rotation's head and epoch record, atomically |
| `GET epochs`, `POST epochs` | members, root, requesters / signed record | epoch records; `POST` only for epoch 0 |
| `GET grants/<device>`, `PUT grants` | that device, members, root / signed grant | keys for devices approved later |
| `GET requests`, `POST requests`, `DELETE requests/<device>` | members / signed request / members | join requests |
| `DELETE account` | members, root | delete the account: its bucket objects, then its records |
| `POST storage/sign` | members | presigned `GET`/`PUT` URLs (15 min) for repository objects |
| `GET storage/list?prefix=`, `POST storage/remove` | members | list and delete repository objects |

Reads are signed by the reading key: `Authorization: OneCloud <key> <unix
seconds> <signature>` over `onecloud-auth-v1\n<METHOD>\n<path+query>\n<seconds>\n<sha256 of body>\n`.
Writes carry their own signatures and are checked with the rules clients
apply (`src/proto.ts` mirrors onecloud-core). On top of those the service
refuses heads from devices removed since the device list the head names,
heads outside the current epoch, and any new epoch except through
`POST rotation`: that closes the two windows the client rules alone leave
open.

## Develop

```sh
pnpm install
pnpm test          # workerd: format fixtures from Rust, the API, the page's keys
pnpm build:web     # bundle web/app.ts into public/app.js (dev and deploy do it)
pnpm check         # types
pnpm dev           # local coordinator on :8787, no Cloudflare account needed
```

`test/fixtures.json` is written by the Rust side (from the repository root,
`ONECLOUD_FIXTURES=$PWD/service/test/fixtures.json cargo test -p onecloud-core --test fixtures`)
and pins every signed format to the Rust implementation. From the repository
root, `scripts/coordinator-e2e.sh` runs the sync engine against this
service under `wrangler dev`.

## Storage for the managed tiers

Devices never hold bucket credentials. They ask for presigned URLs and move
data straight to and from the bucket; listing and deleting go through the
service. Paths must be restic repository paths (`config`, `keys/<id>`,
`snapshots/<id>`, `index/<id>`, `data/<xx>/<id>`, optionally under `e<n>/`
for later key epochs) and land under `accounts/<root>/`, so a device can
only reach its own account, and a removed device loses storage access at
once. Configure with the `STORAGE_ENDPOINT`, `STORAGE_BUCKET` and
`STORAGE_REGION` vars and the `STORAGE_KEY_ID` and `STORAGE_SECRET`
secrets (`.dev.vars` locally). Any S3 compatible bucket works: R2, or B2
behind Cloudflare.

## Deployment

Deploys as `onecloud-coordinator` with 1.0. Live today is the pre-rename
Worker, `https://omacloud-coordinator.ferdousbd.workers.dev` (deployed
2026-09-28, old formats, test data only), with the R2 bucket
`omacloud-storage` in the EU jurisdiction; `wrangler.jsonc` already names
the new Worker and bucket `onecloud-storage`.
The R2 token is scoped to that bucket, object read and write only. Secrets:
`STORAGE_KEY_ID`, `STORAGE_SECRET`, and `INVITE_CODE`: until billing exists,
creating an account needs the invite code (`x-onecloud-invite`, or
`onecloud init --invite`), and each account may store 10 GB
(`STORAGE_QUOTA_BYTES`). `DELETE account` removes an account's objects and
records. Deploy with `pnpm run deploy`.
