# Your bucket

Omacloud keeps everything in an S3 bucket you rent yourself. You pay the
provider for what you store and transfer, at their prices; Omacloud charges
nothing. Each person needs a bucket and a key of their own.

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
