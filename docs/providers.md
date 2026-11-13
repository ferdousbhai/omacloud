# Where your files live

Omacloud keeps everything in your own Dropbox, or an S3 bucket you rent yourself.
You pay the provider for what you store and transfer, at their prices; Omacloud
charges nothing. Each person needs a Dropbox, or a bucket and key, of their own.

## Dropbox

The simplest start: `omacloud init --provider dropbox` (or Set Up With Dropbox in the
app), then allow Omacloud in your browser. There's nothing to make or paste. Omacloud
reaches only its `Apps/Omacloud` folder, and everything there is encrypted before it
leaves, so Dropbox can't read it.

Each person signs in to their own Dropbox, so there are no invites: they run the same
command. Dropbox limits how fast an app may write, so a large first upload takes longer
than on a bucket; 300 small files took about 20 seconds up and 30 down. Space is your
Dropbox plan's. If the Dropbox desktop app runs on the computer, exclude
`Apps/Omacloud` from it, or it downloads the encrypted copy too.

After a computer is lost, `omacloud devices revoke <fingerprint> --lost` moves to a new
encryption key and asks you to allow Omacloud again. Once every other computer has
switched, the old token is revoked by itself. A computer with the token could delete
what's in the folder until then, and there's no lock to set here: Dropbox's deleted-files
recovery is what's left, for as long as your plan keeps them.

Leaving: `omacloud export --to <folder>` copies everything here for plain restic to
read; `omacloud export` prints the command for restic through rclone.

## A bucket

Any S3-compatible storage with create-only writes (`If-None-Match: *`) works:
`omacloud init` and `omacloud invite` check, and refuse a bucket without them.

| | Cloudflare R2 | Hetzner Object Storage |
|---|---|---|
| Good for | up to a few hundred GB, or a key limited to one bucket | a family's photos, around 1 TB |
| Free | 10 GB of storage a month, and requests within generous limits | nothing |
| Paid | $0.015 per GB a month; downloads are free | a monthly base price with 1 TB of storage and 1 TB of downloads |
| A key can be limited to one bucket | yes | no: it reaches its whole project |
| Delete protection | bucket locks, with a second bucket for the records | object lock, with a second bucket for the records |

Prices change: check the provider's page. An idle computer reads one object every ten
seconds and does a small sync every ten minutes: by our estimate, about 300,000
reads and 13,000 listings a month, not measured yet. R2 bills listings with writes.
Its free 10 million reads and 1 million writes cover about 30 idle computers; changing
files costs more.

The rule for keys: make one that can read and write **that bucket** and nothing
else if the provider allows it. A key that reaches only this bucket is all a lost
computer could ever reach. Copy the secret when it's shown; most providers show it
once. Keep the bucket private, the usual default.

## Cloudflare R2

1. In the [Cloudflare dashboard](https://dash.cloudflare.com/?to=/:account/r2/overview),
   open R2 and create a bucket.
2. Under API tokens, create a token with Object Read & Write on that bucket,
   and copy its Access Key ID and Secret Access Key. Your account ID is on the
   R2 overview page.
3. `omacloud init --provider r2 --account <account id> --bucket <name>
   --access-key-id <id>`, adding `--eu` for a bucket in the EU jurisdiction.

## Hetzner Object Storage

1. In the [Hetzner Console](https://console.hetzner.com/projects), make a project
   for this person, then a bucket without object lock, in the location you pick.
   A project of its own matters: its keys reach every bucket in the project.
2. Under Security, S3 credentials, generate credentials.
3. `omacloud init --provider hetzner --location fsn1 --bucket <name>
   --access-key-id <id>`.

## Other S3 storage

`omacloud init --provider s3 --endpoint <url> --region <region> --bucket <name>
--access-key-id <id>`. AWS S3, MinIO and SeaweedFS have create-only writes.
Backblaze B2 was reported to refuse them.

## Delete protection

Every computer of an account holds the bucket's key, so a stolen or infected one
could delete the bucket. Encryption doesn't help against that. Delete protection
at the provider does: objects can't be deleted until they're some days old,
whatever key asks. Omacloud can't turn this on for you, as a key that could turn it
on could turn it off. Do it when you make the bucket, for anything you can't lose.
Omacloud's own deletes (old versions, past keys) that the protection refuses wait
and are tried again.

- **R2:** make a second, plain bucket for the records, and pass it as
  `--coordination-bucket <name>`, with a key that reaches both. Then, in the first
  bucket's settings, add a bucket lock rule for the whole bucket with a retention
  of, say, 30 days. The whole bucket, not chosen prefixes: a key rotation moves
  the repository to a new folder in it. The records change in place, so they can't
  sit under a lock.
- **Hetzner:** create the bucket with object lock, and a second, plain bucket
  for the records, as Hetzner can't do create-only writes with object lock on:
  `--coordination-bucket <name>`. Both take the same key.

## Reading in a browser

Omacloud Drive, at omacloud.computer/drive/ ([web/README.md](../web/README.md)).
The page reads your files straight from your storage and decrypts them in the
browser, so no server of Omacloud's carries them. Dropbox needs nothing for that.
A bucket needs one rule, set once: let pages from `https://omacloud.computer`
read it (CORS). It lets a browser on that site show what it fetched; it opens
nothing to anyone without the bucket's key. With a second bucket for the records,
set it on both.

The key your computers hold can't change a bucket's settings, so set it at the
provider:

- **R2:** in the bucket's Settings, under CORS Policy, add:

  ```json
  [{ "AllowedOrigins": ["https://omacloud.computer"], "AllowedMethods": ["GET", "HEAD"], "AllowedHeaders": ["range"], "MaxAgeSeconds": 3600 }]
  ```

- **Other S3 storage:** the same rule through the provider's console or its
  `PutBucketCors`: origin `https://omacloud.computer`, methods `GET` and `HEAD`,
  header `range`.

Without the rule, requests to an R2 bucket still succeed but carry no
`Access-Control-Allow-Origin`, so a browser refuses to hand the page what came back.
