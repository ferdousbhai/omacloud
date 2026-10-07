# Changelog

## 0.0.14 (2026-10-07)

- Dropbox is available as your own storage. Create an App folder scoped
  Dropbox app, authorize it from setup, and Omacloud keeps its encrypted
  repository and coordination records there. Other computers join by code;
  after removing one, a new authorization is shared with the remaining
  computers and the old token is revoked when they have switched.
- Deleting can be undone on Omacloud storage. Whatever your computers
  delete or replace there is kept for 14 days, encrypted like everything
  else and not counted in your quota, so if a removed computer or someone
  in your Google account wipes your files, ask at privacy@omacloud.computer:
  everything deleted or replaced since the time you give is put back
  (files added since stay).
- Up to your quota's worth of deleted files is kept: past that, deleting
  and replacing files on Omacloud storage is refused until some expires.
- An object on Omacloud storage can be at most 5 GiB (Omacloud's own are
  far smaller).
- Chromium follows you: bookmarks, search engines, browser settings and
  extensions sync between your computers with settings sync, and edits
  made on two computers merge: a bookmark added on each, or one moved here
  and renamed there, both land. The same thing changed on both (one
  bookmark renamed twice) keeps one computer's version, and `omacloud
  settings` and the app say which, on every computer, for a week. Changes from another computer are written into
  Chromium once it's closed, never while it runs; `omacloud settings` and
  the app say when some wait. An extension added on one computer installs
  on the others at Chromium's next start, without root; for a profile
  other than the first, the app offers it to install. History, passwords,
  cookies, open tabs and extensions' own data stay on each computer, as do
  the home page, startup pages and default search engine, which Chromium
  guards against change.
- Saved Wi-Fi networks sync too, keys and all, through NetworkManager and
  without root. A network forgotten on one computer is forgotten on the
  others that had it. One saved on two computers with different keys keeps
  each computer's own key until it's changed on one of them, and a network's old settings are backed up
  before a change from another computer replaces them. A network you're
  connected to isn't forgotten under you: it goes once you disconnect. A
  change NetworkManager turns down is shown as not applied, not tried
  again every sync. A write into Chromium or NetworkManager that fails
  for another reason is tried again, less often each time, and `omacloud
  settings` says why. Enterprise (802.1X) and WEP networks stay on each
  computer, and with iwd instead of NetworkManager, Wi-Fi doesn't sync
  (iwd keeps its networks where only root can read them); `omacloud
  settings` says which.
- More of your setup follows you: `.bash_aliases`, `.inputrc`, git's
  settings, fish's `config.fish` and functions, Neovim's `init.lua` and
  `lua/`, and the settings of mise, lazygit, mpv and fastfetch. Shell
  histories stay on each computer, and so do `.bash_profile`, `.profile`
  and fish's `conf.d`, where installers add lines that only work where
  they ran.
  `.npmrc`, which can hold a token, goes in the keys you seal with
  `omacloud secrets save`, as does git's `credentials` file.
- Your AI agents' setup follows you too: the settings, instructions,
  skills, hooks and themes of Claude Code, Codex, Gemini, opencode, Cursor,
  pi, Grok and Copilot, and herdr's config. Their sign-ins, sessions,
  history, caches and databases stay on each computer, and Omacloud
  refuses any credential, auth or token file by name whatever else says
  to sync it. Below a skills or hooks folder only text files up to 2 MiB
  sync, never `node_modules`, `.git` or caches, and a file or folder you
  link in from elsewhere is left alone (and listed), rather than stopping
  settings sync. API keys written into these settings files sync with
  them, end-to-end encrypted, readable by every computer of yours.
  Codex's trusted projects and pi's version note stay on each computer;
  two computers' first Codex configs combine, servers and all.
- `omacloud settings`, `omacloud status` and the app's Omarchy page list
  what syncs (shell and dotfiles, AI agents, desktop and apps, Chromium
  by profile, Wi-Fi) and how each stands: in sync, waiting for Chromium to close,
  extensions to install, edits settled lately, a change to choose, or not
  available and why.

## 0.0.13 (2026-10-07)

Nothing you save is lost to a sync in progress.

- A file you save while Omacloud is bringing in changes from another
  computer is no longer overwritten or deleted: your version stays (as a
  conflict copy, beside the other computer's) and syncs everywhere. A
  setting stays as you saved it, and the other version waits for
  `omacloud settings resolve`.
- A file that's already here, the same as another computer's (copied over
  before its folder synced), is left as it is rather than kept as a
  conflict copy: it only takes the other computer's time.
- Changes the file watcher missed are found: when the system drops events,
  and every ten minutes, Omacloud looks over all your synced files. A synced
  folder deleted and made again is watched again.
- After an upgrade, the running Omacloud restarts into the new version on
  its own, instead of at your next login.
- A merged or resolved setting is saved the way an editor saves it, so a
  crash midway can't leave it cut short.
- Settings backups and held versions are readable by you alone, and only the
  newest 100 backups are kept.
- A stray entry in the account's storage is reported as an error instead of
  stopping Omacloud, and checking a file that's already in place no longer
  reads it into memory whole.
- Omacloud storage has a waitlist. Signing in without an invitation puts
  you on it and says so; on omacloud.computer you can ask with just your
  email, confirmed by a link emailed to you. An invitation now comes by
  email, with how to start.
- Too many sign-ins in a minute are turned away with a message saying to
  wait, instead of a bare failure.
- Uploads side by side can no longer take an account past its quota, and
  uploads left unfinished for a day are cleared away.
- An account holds at most 20 storage keys: signing in past that retires
  the oldest, and a computer still using it signs in again.

## 0.0.12 (2026-10-06)

- The Recovery page's button reads Create Recovery Card (it was Make a
  Card), and the card is called a recovery card throughout the app.

## 0.0.11 (2026-10-05)

A trusted contact can help you back into your account.

- A trusted contact: someone you trust, like your partner, keeps a printed
  card that gets you back into your account if you lose your recovery code
  and every computer. The card works only with your Google sign-in, so on
  its own it opens nothing. Recovery in the app makes, prints and removes
  it; on a new computer, enter the card beside Sign In with Google. Needs
  Omacloud storage. From the terminal: `omacloud recovery contact` and
  `omacloud init --hosted --contact-card`.
- Setup's Sign In with Google takes a recovery code too, for when no
  computer is left to approve a new one.
- The app's Recovery page no longer splits the recovery code into shares;
  `omacloud recovery split` and `combine` still do.
- A computer that joined after a storage key change (approved, or with the
  recovery code) can sync: it no longer stops with "the new bucket key
  wasn't sealed to this computer".

## 0.0.10 (2026-10-05)

A lost computer is cut off from Omacloud storage too.

- Removing a computer from an Omacloud storage account switches the others
  to a new storage key, which Omacloud makes, and once all have switched
  the old keys are retired: the removed computer can't reach your storage.
  No key to make or paste. `omacloud bucket set-key` does the same from the
  terminal.
- A computer whose storage key was retired while it was away signs in with
  Google again: `omacloud bucket set-key --here-only`.

## 0.0.9 (2026-10-05)

Omacloud storage: sign in with Google and there's nothing else to set up.

- Setup's first choice is Sign In with Google. Your files are kept on
  Omacloud storage (omacloud.computer, in the EU), still encrypted on your
  computer before they leave it; Omacloud can't read them. It's by
  invitation for now. Your own bucket remains the other choice, as before.
- Signing in on another computer of yours asks to join; approve it by its
  fingerprint, as with a join code. `omacloud init --hosted` does the same
  from the terminal.
- An idle computer checks for changes with one small request instead of a
  full sync: about a tenth of the requests, which saves battery and, with
  providers that charge per request, money. A full sync still runs every
  ten minutes.
- New repositories keep pack files under 64 MiB, so no upload exceeds what
  storage behind Cloudflare accepts.

## 0.0.8 (2026-10-03)

OneCloud is now Omacloud: the app, the `omacloud` and `omacloud-app`
commands, `~/.config/omacloud`, `.omacloudignore`, the `[omacloud]` package
repository and github.com/ferdousbhai/omacloud.

- A clean break: stored formats changed with the name, so an account made
  with OneCloud doesn't open here. Set up again; your files are still on
  your computer. install.sh removes the old package and repository.
- The daemon fix from 0.0.7 (connections the provider closes) carries over.

## 0.0.7 (2026-10-03)

- Fix: background sync was killed a few minutes in when the provider closed
  an idle connection (Hetzner does after a minute or two): the next request
  raised SIGPIPE, which Omacloud had set to end the process. It's ignored
  again, so a closed connection is an error the daemon retries; output piped
  into `head` still ends quietly.
- The daemon's log no longer repeats the storage library's progress lines
  every few seconds.

## 0.0.6 (2026-10-02)

Setting up with Hetzner is one page.

- Get a Key opens the Hetzner Console; generate credentials there and paste
  both keys. Omacloud makes a private bucket for itself (`omacloud-` and a
  random name), so there's no bucket to create, name or configure. "Use a
  bucket I already have" is still there.
- `omacloud init --repo opendal:s3` without `--opt bucket=` makes the
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
- `omacloud init --folders Documents,Pictures --settings` for the same from
  the terminal.
- Setup's Hetzner steps no longer read "private" as a bucket name.

## 0.0.4 (2026-10-01)

- Reproducible builds: two builds of the same tag give byte-identical
  binaries wherever they run, and the binaries no longer carry the
  builder's home directory. CONTRIBUTING.md says how to check a release.
- docs/providers.md: setting up Hetzner, Cloudflare R2, Backblaze B2 or
  other S3 storage, and what was tested on each.

## 0.0.3 (2026-10-01)

Omacloud is your own bucket only: the code for running it through a server
of ours is gone, and with it the commands that needed one. Nothing changes
for an existing setup.

- `omacloud share` is gone until folders can be shared through your own
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
- Opening Omacloud while it's open brings its window forward instead of a
  second one.
- Plain wording about cost: you pay your storage provider; Omacloud charges
  nothing.

## 0.0.1 (2026-10-01)

The first release. Everything lives in your own S3 bucket (Hetzner, R2, B2,
MinIO), and nobody sits in between.

- Syncs Desktop, Documents and Pictures in place on every computer, as iCloud
  does; `omacloud folders add|skip` for others. Folders match by what they
  are, so a German computer's `Dokumente` is another's `Documents`.
- Omarchy settings from the dots manifest's shared tier, with three way
  merges and backups; package lists per computer; ssh and gpg keys sealed so
  only the recovery code opens them.
- End to end encrypted, in plain restic format: `omacloud export` gives what
  plain restic needs to restore without omacloud.
- Devices in a signed chain: join with a join code and approval, or the
  recovery code; remove a device and change the key.
- Cut off a lost computer: `omacloud bucket set-key` hands every other
  computer a new bucket key, sealed so a removed one gets nothing;
  `omacloud bucket status` says when the old key can go. A computer that
  was away until the old key was gone is told how to catch up
  (`omacloud bucket set-key --here-only`).
- The bucket key lives in the desktop keyring, not in a config file.
  Secrets never go on a command line: the secret key, recovery code and
  join code come from the environment or a prompt that doesn't echo.
- Versions of every file, restore to any of them.
- Social recovery: split the recovery code among people you trust.
- The Omacloud app (`omacloud-app`, GTK): set up a computer, see when it
  last synced, folders, devices (removing one walks through the bucket key
  change), Omarchy settings, ssh and gpg keys, recovery. Fits narrow
  windows. Desktop notifications from the daemon.
- A new computer gets small files and documents first, pictures last, and an
  interrupted first sync resumes.
- Bandwidth cap, pause on battery, delete protection on the bucket respected.

Not yet: sharing with other people, online-only files.
