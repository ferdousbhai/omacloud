# Your own storage

Instead of signing in to Omacloud storage, you can keep everything in an S3
bucket you rent yourself. You pay the provider for what you store and
transfer, at their prices; Omacloud itself charges nothing. Any S3-compatible storage works. These are the ones checked so far,
and what to watch for with each. The Omacloud app's setup page shows the
same steps.

You can also use Dropbox, through a Dropbox app you create. See [Dropbox](#dropbox)
below.

For S3 storage:

- With Hetzner or other S3 storage, Omacloud can make the bucket itself;
  you only make a key. Otherwise keep the bucket private (the usual
  default), so only your keys reach it.
- Make a key that can read and write **that bucket**. If the provider can
  limit a key to one bucket, do: a key that reaches only this bucket is all
  a lost computer could ever reach.
- Copy the secret key when it's shown; most providers show it only once.
- Leave **object lock** off unless you want delete protection. With it on,
  Omacloud needs a second, plain bucket for keeping your computers in step
  (the switch at the bottom of the setup page, or `--coordination-bucket`).

## Hetzner Object Storage

Tested end to end (2026-09-30, Falkenstein): setup, joining, sync, key
rotation, plain restic restores, a computer removed and the bucket key
changed, object lock with a second bucket.

1. In the [Hetzner Console](https://console.hetzner.com/projects) (the app's
   Get a Key button opens it), open your project, then Security, S3
   credentials, and Generate credentials.
2. Paste both keys into Omacloud's setup. Omacloud makes a private bucket
   for itself, without object lock, in the location you pick. To use a
   bucket you made yourself, switch on "Use a bucket I already have".

Notes:

- Hetzner bills a monthly base price while you have any Object Storage,
  plus usage beyond what it includes; see their pricing page.
- Its keys reach every bucket in the project. To keep Omacloud's key away
  from other buckets, give Omacloud a project of its own.
- With object lock, Hetzner can't do the create-only writes Omacloud uses
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
  and Omacloud keeps working under them.
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
read and write it. For keeping computers in step Omacloud needs create-only
writes (`If-None-Match: *`) on the bucket; MinIO and SeaweedFS have them.

## Dropbox

Dropbox support has not yet been tested against a live Dropbox account. It
uses Dropbox's App folder access, so its token cannot reach the rest of your
Dropbox. Omacloud puts its encrypted restic repository and coordination
records under `Omacloud` in that app folder. The Dropbox desktop client may
also download those encrypted objects if you sync the app folder locally;
exclude that folder in Dropbox if you do not want a second local copy.

1. In the [Dropbox App Console](https://www.dropbox.com/developers/apps),
   create a **Scoped access** app with **App folder** access.
2. On its Permissions tab, enable `files.metadata.read`,
   `files.metadata.write`, `files.content.read`, and `files.content.write`.
   Save the change before authorizing.
3. Copy the app key and app secret from its Settings tab into Omacloud.
   Click **Authorize with Dropbox**, approve access, and paste the code
   Dropbox shows into Omacloud. Then click **Create Dropbox Account**.

The command line equivalent is `omacloud init --repo opendal:dropbox --opt
client_id=<app key>`; it asks for the app secret and authorization code. The
authorization asks for offline access, which yields a refresh token. Omacloud
keeps that token and the app secret in the desktop keyring, or in its private
config file when no keyring is available. Other computers join with a join
code as usual. To cut off a removed computer, authorize again under Devices.
Omacloud keeps the old token in a private local file until every remaining
computer switches, then revokes that token through Dropbox's API. Keep the
computer that started the change available until revocation completes;
`omacloud bucket status` shows its progress. Unlinking the entire app in
Dropbox before then will stop sync.

Dropbox's API is different from S3: Omacloud uses Dropbox's atomic add mode
for coordination records and ordinary uploads for encrypted repository data.
The first repository setup creates 256 restic data directories and may take
longer than with an object bucket. Existing S3 accounts are not moved by
choosing Dropbox; set up a new account for that location.
