# Changelog

## 0.0.7 (2026-10-03)

- Fix: background sync was killed a few minutes in when the provider closed
  an idle connection (Hetzner does after a minute or two): the next request
  raised SIGPIPE, which OneCloud had set to end the process. It's ignored
  again, so a closed connection is an error the daemon retries; output piped
  into `head` still ends quietly.
- The daemon's log no longer repeats the storage library's progress lines
  every few seconds.

## 0.0.6 (2026-10-02)

Setting up with Hetzner is one page.

- Get a Key opens the Hetzner Console; generate credentials there and paste
  both keys. OneCloud makes a private bucket for itself (`onecloud-` and a
  random name), so there's no bucket to create, name or configure. "Use a
  bucket I already have" is still there.
- `onecloud init --repo opendal:s3` without `--opt bucket=` makes the
  bucket, for any S3 storage whose key can.
- Plain errors when the provider refuses the key, or the key can't create
  buckets.
- With R2 and B2 you still make a bucket and a key just for it: a key that
  could create buckets there would reach every bucket in the account.

## 0.0.5 (2026-10-02)

Choosing what syncs, as on iCloud.

- Setup asks what to sync, with switches and sizes: Documents, Pictures,
  Music, Videos, Downloads and Omarchy settings. Documents, Pictures and
  Omarchy settings start on. Desktop is offered only where it's a folder of
  its own (Omarchy points it at your home).
- Overview has the same switches any time, with Open buttons and sizes, and
  Add Folder for any other folder in your home. Switching a folder off asks
  first, then keeps its files on this computer.
- Anything named like `name.nosync` stays on its computer, as on iCloud.
- `onecloud init --folders Documents,Pictures --settings` for the same from
  the terminal.
- Setup's Hetzner steps no longer read "private" as a bucket name.

## 0.0.4 (2026-10-01)

- Reproducible builds: two builds of the same tag give byte-identical
  binaries wherever they run, and the binaries no longer carry the
  builder's home directory. CONTRIBUTING.md says how to check a release.
- docs/providers.md: setting up Hetzner, Cloudflare R2, Backblaze B2 or
  other S3 storage, and what was tested on each.

## 0.0.3 (2026-10-01)

OneCloud is your own bucket only: the code for running it through a server
of ours is gone, and with it the commands that needed one. Nothing changes
for an existing setup.

- `onecloud share` is gone until folders can be shared through your own
  bucket (#8).
- Computers learn of changes by checking the bucket, as before.

## 0.0.2 (2026-10-01)

Easier to set up.

- Setup asks which storage provider you use (Hetzner, Cloudflare R2,
  Backblaze B2 or other S3 storage) and shows the two steps to get a bucket
  and a key there, with a link; the endpoint and region follow from your
  answers. A bucket with object lock gets its second bucket from a switch.
- The recovery code is confirmed by typing its last four characters.
- Background sync starts as soon as a computer is set up, not at the next
  login.
- Devices has Show Join Code, so adding a computer needs no terminal.
- Opening OneCloud while it's open brings its window forward instead of a
  second one.
- Plain wording about cost: you pay your storage provider; OneCloud charges
  nothing.

## 0.0.1 (2026-10-01)

The first release. Everything lives in your own S3 bucket (Hetzner, R2, B2,
MinIO), and nobody sits in between.

- Syncs Desktop, Documents and Pictures in place on every computer, as iCloud
  does; `onecloud folders add|skip` for others. Folders match by what they
  are, so a German computer's `Dokumente` is another's `Documents`.
- Omarchy settings from the dots manifest's shared tier, with three way
  merges and backups; package lists per computer; ssh and gpg keys sealed so
  only the recovery code opens them.
- End to end encrypted, in plain restic format: `onecloud export` gives what
  plain restic needs to restore without onecloud.
- Devices in a signed chain: join with a join code and approval, or the
  recovery code; remove a device and change the key.
- Cut off a lost computer: `onecloud bucket set-key` hands every other
  computer a new bucket key, sealed so a removed one gets nothing;
  `onecloud bucket status` says when the old key can go. A computer that
  was away until the old key was gone is told how to catch up
  (`onecloud bucket set-key --here-only`).
- The bucket key lives in the desktop keyring, not in a config file.
  Secrets never go on a command line: the secret key, recovery code and
  join code come from the environment or a prompt that doesn't echo.
- Versions of every file, restore to any of them.
- Social recovery: split the recovery code among people you trust.
- The OneCloud app (`onecloud-app`, GTK): set up a computer, see when it
  last synced, folders, devices (removing one walks through the bucket key
  change), Omarchy settings, ssh and gpg keys, recovery. Fits narrow
  windows. Desktop notifications from the daemon.
- A new computer gets small files and documents first, pictures last, and an
  interrupted first sync resumes.
- Bandwidth cap, pause on battery, delete protection on the bucket respected.

Not yet: sharing with other people, online-only files.
