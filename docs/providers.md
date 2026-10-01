# Storage providers

OneCloud keeps everything in an S3 bucket you rent. You pay the provider for
what you store and transfer, at their prices; OneCloud itself charges
nothing. Any S3-compatible storage works. These are the ones checked so far,
and what to watch for with each. The OneCloud app's setup page shows the
same steps.

Whatever the provider:

- Make the bucket **private**.
- Make a key that can read and write **that bucket**. If the provider can
  limit a key to one bucket, do: a key that reaches only this bucket is all
  a lost computer could ever reach.
- Copy the secret key when it's shown; most providers show it only once.
- Leave **object lock** off unless you want delete protection. With it on,
  OneCloud needs a second, plain bucket for keeping your computers in step
  (the switch at the bottom of the setup page, or `--coordination-bucket`).

## Hetzner Object Storage

Tested end to end (2026-09-30, Falkenstein): setup, joining, sync, key
rotation, plain restic restores, a computer removed and the bucket key
changed, object lock with a second bucket.

1. In the [Hetzner Console](https://console.hetzner.com/projects), open
   Object Storage and create a bucket: private, Object Lock disabled. Pick
   the location you'll choose in OneCloud (Falkenstein, Nuremberg or
   Helsinki).
2. Under Security, S3 credentials, generate credentials and copy both keys.

Notes:

- Hetzner bills a monthly base price while you have any Object Storage,
  plus usage beyond what it includes; see their pricing page.
- Its keys reach every bucket in the project. To keep OneCloud's key away
  from other buckets, give OneCloud a project of its own.
- With object lock, Hetzner can't do the create-only writes OneCloud uses
  to keep computers in step, so the second bucket is required there.

## Cloudflare R2

Tested (2026-09-29): sync through a key limited to one bucket, bucket locks
refusing deletes and overwrites, old data deleted once its lock ends.

1. In the [Cloudflare dashboard](https://dash.cloudflare.com/?to=/:account/r2/overview),
   open R2 and create a bucket.
2. Under API tokens, create a token with Object Read & Write on that bucket,
   and copy its Access Key ID and Secret Access Key. Your account ID is on
   the R2 overview page.

Notes:

- A bucket created in the EU jurisdiction has its own endpoint: turn on
  "The bucket is in the EU jurisdiction" in setup.
- R2 has no object lock; its bucket locks give the same delete protection,
  and OneCloud keeps working under them.
- R2 doesn't charge for downloads, which helps when a new computer pulls
  everything.

## Backblaze B2

Not tested end to end yet (#3). It speaks S3 and can limit a key to one
bucket.

1. In [Backblaze](https://secure.backblaze.com/b2_buckets.htm), create a
   private bucket and note its S3 endpoint, like
   `s3.eu-central-003.backblazeb2.com`.
2. Under Application Keys, add a key with read and write access to that
   bucket only, and copy its keyID and applicationKey.

## Other S3 storage (MinIO and others)

You need the endpoint, the region (or `auto`), a bucket, and a key that can
read and write it. For keeping computers in step OneCloud needs create-only
writes (`If-None-Match: *`) on the bucket; MinIO and SeaweedFS have them.
