# Omacloud storage format

What an account looks like in its storage, byte for byte, so that someone
with the storage (a Dropbox folder or an S3 bucket), the recovery code or a
device key, and this page can find, verify and read it without Omacloud.
[design.md](design.md) says why it is built this way; this page says only
what is there. Paths in parentheses are source files under `crates/`, with
the function or type that holds the fact.

`scripts/restic-compat.sh` proves the restic part on every change: upstream
restic, knowing only the exported password, checks an account's repository
and restores it. What this page leaves out is listed at the end.

## Conventions

- **Hex** is lowercase, from the `hex` crate (`hex::encode`). A public key is
  32 bytes (64 hex), a signature 64 bytes (128 hex), a hash 32 bytes (64 hex).
- **Keys** are Ed25519 (`ed25519-dalek` 2). A device key or root key is
  named by its public key in hex (`devices.rs`, `public_hex`).
- **Hash** is SHA-256 (`sha2`), except where SHA-512 is named.
- **JSON** is `serde_json` 1.0.151, built without `preserve_order`, so a
  `serde_json::Value` object (the `json!` macro) has its keys sorted, while a
  struct serializes its fields in declaration order. Compact (`to_vec`)
  unless stated. Strings are escaped the serde_json way: `"` `\` and control
  characters only, non-ASCII as raw UTF-8.
- **Sequence numbers** in object names are decimal, zero padded to 20
  digits, with `.json`: `heads/00000000000000000042.json`
  (`omacloud-core/src/bucket.rs`, `BucketCoordinator::key`; `head.rs`,
  `DirCoordinator::path`). Logs count from 1, with no gaps.

## 1. Layout

### Where the account is

A bucket account's location is a `RepoSpec` (`omacloud-core/src/repo.rs`):
an OpenDAL scheme (`opendal:s3` or `opendal:dropbox`) and options,
`root` among them. Everything below lives under `root`
(`RepoSpec::under_root`: `<root>/<name>`).

- S3 (R2, Hetzner, any S3): `bucket` and `endpoint` as given at `init`;
  `root` only if `--root` was given, else the top of the bucket
  (`omacloud/src/main.rs`, the provider match in the `init` arguments).
- Dropbox: `root` defaults to `/Omacloud`, inside the Omacloud app's folder
  (`main.rs`, `Provider::Dropbox`; the app folder is `Apps/Omacloud`, see
  [providers.md](providers.md)).

```
<root>/
  config, keys/, data/, index/, snapshots/   restic repository, epoch 0
  omacloud-e1/, omacloud-e2/, ...            restic repositories of later epochs
  omacloud-coordination/                     signed records (below)
  omacloud-links/<16 hex>                    a web link's sealed keys, minutes at most
  omacloud-inbox/<24 hex>                    files a browser uploaded, sealed
  omacloud-inbox/changed                     16 random bytes, rewritten after uploads
  omacloud-browsers/<24 hex>                 a browser that stays signed in, as it said, sealed
```

### The restic repositories

Each epoch's files are a plain restic repository, repository version 2
(`vendor/rustic_core/src/commands/init.rs`, `ConfigFile::new(2, ..)`),
created by rustic_core 0.13 (`repo.rs`, `RepoSpec::init`) with packs capped
at 64 MiB and padding on.
Read it with restic or rustic and the repository password; the format is
restic's own:
[restic design, repository format](https://restic.readthedocs.io/en/stable/100_references.html#design).
The two patches in `vendor/rustic_core/OMACLOUD.md` keep it a valid restic
repository:

- **Pack padding.** The repository config carries `"pack_padding": true`
  (`vendor/rustic_core/src/repofile/configfile.rs`), an extra field restic
  ignores (`scripts/restic-compat.sh`). Each pack gets one extra data blob of random bytes, indexed like
  any blob (id = SHA-256 of its plaintext) and referenced by no tree, sized
  so the pack's length is a Padmé size
  (`vendor/upstream-patches/0001-pack-padding.patch`, `pad`, `padme`).
- **`splice_tree`** changes how snapshots are written, not what they are.

Where epoch `n` lives (`repo.rs`, `RepoSpec::for_epoch`): epoch 0 at
`root`; epoch `n > 0` at `<root>/omacloud-e<n>` in a bucket, or
`<path>.e<n>` for a local repository. After a rotation checks out, the old
epoch's restic files are deleted (`sync.rs`, `Engine::rotate`,
`RepoSpec::destroy`); `destroy` deletes only restic's file types, so the
`omacloud-*` folders under `root` stay. Normally only the current epoch's
repository holds data. A bucket under delete protection may leave an old
one behind for a while (`State::leftover`).

On Dropbox the repository is created without the 256 `data/` subfolders;
Dropbox makes them as packs arrive (`vendor/rustic_backend/OMACLOUD.md`).

**What a snapshot holds.** A snapshot's tree is the synced folder itself
(home, when folders of home are chosen), not a tree from `/`: the
snapshot's `paths` names the folder, and its `tree` is the folder's contents
spliced together (`sync.rs`, `Engine::push`). `hostname` is the device's name. At the top of the
tree:

| tree path | what | source |
|---|---|---|
| `Desktop/`, `Documents/`, ... | the synced folders | `sync.rs` |
| `.omacloud/settings/<path under home>` | shared settings files | `settings.rs`, `PREFIX` |
| `.omacloud/machines/<device key>/<path under home>` | one computer's backed-up-only settings | `settings.rs`, `MACHINES` |
| `.omacloud/merged/<group>/.../<doc>.json` | merged documents (`chromium`, `wifi`), and `conflicts` records | `merged.rs`, `PREFIX`; `sync.rs`, `CONFLICTS` |
| `.omacloud/packages/<device name>.txt` | package list | `sync.rs`, `PACKAGES`, `update_package_list` |
| `.omacloud/secrets/<device name>.sealed` | secrets bundle (section 3) | `sync.rs`, `SECRETS`, `put_secrets` |
| `.omacloud/choices/<device key>.json` | what syncs on that computer (section 2) | `sync.rs`, `CHOICES`, `record_choices` |

Packages and secrets are named by device name, choices and machines by
device key (`sync.rs`, `update_package_list`, `put_secrets`,
`record_choices`, `own_machine`). A package list reads
`# explicitly installed packages on <name>`, then `[repo]` and `[foreign]`
sections, one package per line (`main.rs`, `package_list`). The merged
documents' schemas are not covered here (`merged.rs`, `wifi.rs`,
`chromium/`).

### Coordination

`omacloud-coordination/` under `root` (`bucket.rs`,
`BucketCoordinator::beside`). With `coordination_bucket` set (a bucket under
object lock on Hetzner, which can't do create-only writes there), the same
tree is at `/omacloud-coordination` in that second bucket, same endpoint and
key (`bucket.rs`, module docs and `beside`; `main.rs`, `Config::coordination_bucket`).

| object | contents | write |
|---|---|---|
| `account.json` | the anchor | create-only |
| `devices/<seq:020>.json` | device chain entry `seq` | create-only, next seq only |
| `heads/<seq:020>.json` | head `seq` | create-only, next seq only |
| `epochs/<epoch+1:020>.json` | epoch record (epoch 0 is file 1) | create-only, next only |
| `keys/<seq:020>.json` | bucket key record `seq` | create-only, next only |
| `grants/<device key>-<epoch>.json` | grant | overwrite |
| `requests/<device key>.json` | join request | overwrite; deleted when handled |
| `synced/<device key>.json` | a JSON number: the head seq that device last synced | overwrite, quietly |
| `key-acks/<seq>/<device key>` | the JSON `true`: that device switched to key record `seq` | overwrite, quietly |
| `changed` | 32 hex characters, random each time | overwrite |
| `create-only-check` | written twice and deleted by `init`'s check | transient |

Sources: `bucket.rs`, `impl Coordinator for BucketCoordinator`, `CHANGED`,
`CHECK`, `check`.

Create-only means `If-None-Match: *` on S3, Dropbox's add mode on Dropbox
(`bucket.rs`, `write_new`; `vendor/opendal-service-dropbox/OMACLOUD.md`):
the storage refuses a name that exists. A log append is refused unless its
seq is one past the last (`append_seq`). Every create-only write, grant,
request and request removal rewrites `changed`; `synced/` and `key-acks/`
writes don't (`create`, `put`, `put_quietly`, `touch`).

Finding the last entry of a log: list it, take the highest seq, refuse if
the highest is above the count (a gap: anyone with the bucket's key can
write a stray entry), then probe `seq+1`, `seq+2`, ... until one is missing
(`bucket.rs`, `last_seq`).

### The directory form

`DirCoordinator` (`head.rs`), for an account in a folder (`init --provider
folder`: `<folder>/Omacloud/coordination`, the repository beside it at
`<folder>/Omacloud/repo`) and for development and tests, keeps the same
records as files in one directory: `account.json`, `heads/`, `devices/`,
`epochs/`, `grants/`, `requests/`, `synced/`, with the same names and
numbering, except:

- Files are pretty-printed JSON (`to_vec_pretty`), not compact. Nothing is
  signed over the stored bytes, so this changes nothing a reader checks.
- `synced/<device key>` has no `.json`, and holds the seq as plain decimal
  text.
- No `keys/`, `key-acks/` or `changed`: a directory account keeps no bucket
  key and has no marker (the `Coordinator` trait's defaults).
- Create-only is a hard link from a temp file (`.tmp-<pid>-<sha256 hex>` in
  the directory) to the final name, which fails if the name exists
  (`DirCoordinator::create`).

The repository then is a local path, its later epochs at `<path>.e<n>`.

## 2. Records

All coordination records are JSON objects, compact in a bucket. Signed
records are signed over bytes rebuilt from their fields (section 3), never
over the stored text.

### Anchor: `account.json`

```json
{"root":"9c1f…e04a"}
```

The root public key (`devices.rs`, `Anchor`). Unsigned: a hint, which the
recovery code (or a member that already pins the root) confirms.

### Device chain entry: `devices/<seq>.json`

`devices.rs`, `DeviceEntry` and `Action`. `action` is a tag flattened into
the entry:

```json
{"seq":1,"prev":"","action":"add","device":"5d2e…71b0","name":"thinkpad","signer":"9c1f…e04a","sig":"a1…"}
{"seq":2,"prev":"3b9a…c2d1","action":"add","device":"e7a0…19fe","name":"desktop","signer":"5d2e…71b0","sig":"0f…"}
{"seq":3,"prev":"77c4…8a02","action":"revoke","device":"e7a0…19fe","signer":"5d2e…71b0","sig":"c8…"}
```

| field | type | |
|---|---|---|
| `seq` | u64 | from 1 |
| `prev` | hex string | hash of entry `seq-1`, `""` for the first |
| `action` | `"add"` or `"revoke"` | |
| `device` | hex key | the device added or revoked |
| `name` | string | `add` only |
| `signer` | hex key | the root, or a device valid before this entry |
| `sig` | hex | |

### Head: `heads/<seq>.json`

`head.rs`, `Head`. One log for the whole account.

```json
{"seq":42,"snapshot":"d4e5…90ab","prev":"6a1b…77f3","devices":3,"epoch":1,"device":"5d2e…71b0","sig":"9e…"}
```

| field | type | |
|---|---|---|
| `seq` | u64 | from 1 |
| `snapshot` | hex string | restic snapshot id, in epoch `epoch`'s repository |
| `prev` | hex string | hash of head `seq-1`, `""` for the first |
| `devices` | u64 | device chain length the signer had verified |
| `epoch` | u64 | repository epoch; `0` if absent (`#[serde(default)]`) |
| `device` | hex key | signer |
| `sig` | hex | |

### Epoch record: `epochs/<epoch+1>.json`

`epoch.rs`, `EpochRecord`.

```json
{"epoch":1,"prev":"0c3d…5e6f","head":57,"devices":4,
 "keys":{"5d2e…71b0":"8f3a…","9c1f…e04a":"21bc…"},
 "snapshots":{"d4e5…90ab":"f1a2…3344"},
 "signer":"5d2e…71b0","sig":"4d…"}
```

| field | type | |
|---|---|---|
| `epoch` | u64 | from 0 |
| `prev` | hex string | hash of the previous record, `""` for epoch 0 |
| `head` | u64 | seq of the head that starts this epoch; `0` for epoch 0 |
| `devices` | u64 | device chain length the signer saw |
| `keys` | object, hex key → hex | the epoch secret sealed to each member valid at `devices`, and to the root |
| `snapshots` | object, id → id | previous epoch's snapshot id → this epoch's, for every snapshot copied; `{}` for epoch 0 |
| `signer`, `sig` | hex | |

The sealed **epoch secret** (`epoch.rs`, `EpochSecret`), JSON before
sealing; `key` is rustic's `MasterKey`, as in a restic key file's `data`
(`vendor/rustic_core/src/repofile/keyfile.rs`, `MasterKey`, `Mac`):

```json
{"password":"k7m2…(28 chars)","key":{"mac":{"k":"<base64 16 bytes>","r":"<base64 16 bytes>"},"encrypt":"<base64 32 bytes>"}}
```

`password` opens the repository's `keys/` file with restic; `key` is the
master key itself, so no scrypt is needed. The password is a 28 character
`random_secret` (`devices.rs`; `account.rs`, `create`; `sync.rs`, `rotate`).

### Grant: `grants/<device>-<epoch>.json`

`epoch.rs`, `Grant`: the current epoch's secret for a device approved after
the epoch began.

```json
{"epoch":1,"device":"aa01…be77","sealed":"52d0…","signer":"5d2e…71b0","sig":"6b…"}
```

`sealed` is the hex sealed box of the epoch secret JSON. `signer` is the
root or any device ever in the chain, valid or since revoked
(`Grant::valid`).

### Join request: `requests/<device>.json`

`devices.rs`, `JoinRequest`.

```json
{"device":"aa01…be77","name":"laptop","root":"9c1f…e04a","sig":"e2…"}
```

`root` is the root the joining device pinned from the anchor.

### Bucket key record: `keys/<seq>.json`

`bucket_key.rs`, `KeyRecord`. The latest one holds; records are not hash
chained.

```json
{"seq":2,"devices":4,"keys":{"5d2e…71b0":"7a90…","9c1f…e04a":"c3e1…"},"signer":"5d2e…71b0","sig":"11…"}
```

`keys` holds the credentials sealed to each member valid at `devices`, and
to the root, hex. The credentials before sealing (`Credentials`, untagged),
one of:

```json
{"access_key_id":"AKIA…","secret_access_key":"…"}
{"refresh_token":"…"}
```

### Synced and key acknowledgments

`synced/<device key>.json`: a bare JSON number, `42`, the head that device
last synced; tidying keeps that version (`bucket.rs`, `set_synced`;
design.md, Storage). `key-acks/<seq>/<device key>`: the bytes `true`
(`bucket.rs`, `ack_key`). Neither is signed.

### Choices: `.omacloud/choices/<device key>.json`, in the snapshot tree

Pretty-printed JSON with a trailing newline (`sync.rs`, `record_choices`),
built by `main.rs`, `my_choices`:

```json
{
  "doorbell": …,
  "files": {"~/.config/hypr/monitors.conf": "backup"},
  "folders": {"Desktop": "sync", "Documents": "sync", "Pictures": "off"},
  "name": "thinkpad",
  "settings": {"agents": "sync", "apps": "sync", "chromium": "sync", "settings": "sync",
               "shell": "sync", "theme": "sync", "wifi": "off"}
}
```

Values are `"sync"`, `"off"`, `"backup"` or a choice word (`Choice::word`
in `main.rs`). `doorbell` is `doorbell::reach()`, not specified here. Choices
are informational: nothing verifies them beyond the snapshot's own head.

### Web link object: `omacloud-links/<name>`

`name` is 8 random bytes in hex (`bucket.rs`, `Links::leave`). Contents:
the sealed web code (section 3). The web code, JSON
(`main.rs`, `WebCode`, `DropboxKey`, `UploadKey`):

```json
{"v":1,"endpoint":"https://acct.r2.cloudflarestorage.com","bucket":"my-omacloud","root":"","region":"auto",
 "key":"AKIA…","secret":"…","password":"k7m2…","browsers":"omacloud-browsers",
 "upload":{"inbox":"omacloud-inbox","seed":"<base64url 32 bytes>",
  "endorsement":{"key":"0e44…","until":1795000000,"device":"5d2e…71b0","sig":"…"}}}
```

`root` and `password` are the current epoch's (`WebCode::read`, via
`Engine::repository`), so `root` ends in `omacloud-e<n>` after a rotation.
For Dropbox the bucket fields are empty and
`"dropbox":{"app_key":…,"refresh_token":…,"root":…}` is added. `upload` is
only in a web link. Only the newest link is kept; others are deleted when
a new one is made, when the command ends, or when older than ten minutes
(`Links::clear`, `clear_older`; `main.rs`, `WEB_LINK_VALID`).

The link itself is never stored:
`https://omacloud.computer/files/#link=<base64url(seed)>.<base64url(address)>`,
`address` being a presigned GET of the object, good for five minutes, or
Dropbox's temporary link (`main.rs`, `web_link`).

### Browser note: `omacloud-browsers/<24 hex>`

Named by 12 random bytes in hex, chosen by a browser that stays signed in
(`web/remember.js`, `keep`), and sealed as an inbox item is, with the
current repository's key and no signature (`web/app.js`, `record`;
`omacloud/src/main.rs`, `BrowserNote`):

```json
{"v":1,"name":"iPhone, Safari","lock":"passkey","at":1795000000}
```

`lock` is `"passkey"` or `"device"`; `at` is Unix seconds. A browser that
stops writes the same with `"gone":true` over it, and a computer removes
it. All are removed when the repository key changes (`forget_browsers`).
Like the inbox, they are under the epoch 0 `root`, at the web code's
`browsers`.

### Inbox item: `omacloud-inbox/<24 hex>`

Named by 12 random bytes in hex (`web/app.js`, `send`). Sealed with the
current repository's key (section 3); the plaintext is
(`omacloud-web/src/upload.rs`, `make`; `omacloud-core/src/inbox.rs`, `open`):

```
u32 little endian   header length
header              JSON, below
content             the file's bytes
```

```json
{"endorsement":{"key":"0e44…","until":1795000000,"device":"5d2e…71b0","sig":"…"},
 "mtime":1794990000000,"path":"Documents/plan.md","sig":"…","v":1}
```

`v` must be 1; `mtime` (Unix milliseconds) is optional; `path` is relative
to the synced folder, with no `..`, no component starting with `.`, and not
empty (`inbox.rs`, `safe`). `endorsement` is `inbox.rs`, `Endorsement`:
the session's public key, `until` in Unix seconds, and the member that
signed. `omacloud-inbox/changed` holds 16 random raw bytes, rewritten after
each batch (`web/app.js`).

An endorsement of a connector for agents ([cloud-agents.md](cloud-agents.md))
also holds `"agent":{"name":"agents","folders":["Documents/Notes"]}`, and is
signed in the JSON form under a domain of its own (compact, keys sorted):
`{"device":…,"domain":"omacloud-agent-key-v1","folders":[…],"key":…,"name":…,"until":…}`.
An upload under one must have its `path` in one of `folders`, and goes over
a file that's there; a browser's never does (`inbox.rs`, `Agent`, `open`;
`omacloud/src/uploads.rs`, `save`).

The inbox is always under the epoch 0 `root`, not under `omacloud-e<n>`
(`bucket.rs`, `Inbox::beside` with `config.repo`; `main.rs`,
`WebCode::with_upload`).

## 3. Cryptography

### Recovery code to root key

`devices.rs`, `root_key`. The code is 28 characters from
`23456789abcdefghjkmnpqrstuvwxyz` (31 symbols, about 139 bits), shown as
seven groups of four joined by `-` (`random_secret`, `new_recovery_code`).
To derive: keep ASCII letters and digits only, lowercase them, require 28,
then

```
seed = SHA-256("omacloud-recovery-v1\n" || code)
root = Ed25519 signing key from the 32-byte seed
```

No KDF stretching: the code's entropy is the protection. A device key is a
random Ed25519 seed, stored on the computer as 64 hex characters in
`~/.local/share/omacloud/device.key` (`head.rs`, `load_or_create_key`;
`main.rs`, `Paths::device_key`).

A **fingerprint** (shown for approval) is the first 16 hex characters of
`hex(SHA-256(ascii hex public key))`, in groups of four:
`3f2a-91c0-7be4-0d15` (`devices.rs`, `fingerprint`).

### Signatures

Plain Ed25519 (`ed25519-dalek` 2, `Signer::sign`, `Verifier::verify`;
`devices.rs`, `verify_sig`). Each record signs a domain-separated message
rebuilt from its fields. Two forms are used:

**Line form**: fields joined by `\n`, with a trailing `\n`, numbers in
decimal. Nothing is escaped, so a field holding `\n` is ambiguous; no field
written by Omacloud does (a device name is checked at `init`; see the end).

| record | signed bytes | source |
|---|---|---|
| head | `omacloud-head-v3\n{seq}\n{snapshot}\n{prev}\n{devices}\n{epoch}\n{device}\n` | `head.rs`, `Head::signed_bytes` |
| grant | `omacloud-grant-v1\n{epoch}\n{device}\n{sealed}\n{signer}\n` | `epoch.rs`, `Grant::signed_bytes` |
| join request | `omacloud-join-v1\n{device}\n{name}\n{root}\n` | `devices.rs`, `JoinRequest::signed_bytes` |
| upload key endorsement (a browser's) | `omacloud-upload-key-v1\n{key}\n{until}\n{device}\n` | `inbox.rs`, `Endorsement::signed_bytes` |
| upload | `omacloud-upload-v1\n{path}\n{hex(SHA-256(content))}\n` | `inbox.rs`, `upload_signed_bytes` |

**JSON form**: compact serde_json of a fixed object, the `sig` left out and
a `domain` added.

Device entry (`devices.rs`, `DeviceEntry::signed_bytes`), fields in this
order, the action's fields flattened after `prev`:

```
{"domain":"omacloud-device-v1","seq":2,"prev":"3b9a…","action":"add","device":"e7a0…","name":"desktop","signer":"5d2e…"}
{"domain":"omacloud-device-v1","seq":3,"prev":"77c4…","action":"revoke","device":"e7a0…","signer":"5d2e…"}
```

Epoch record (`epoch.rs`, `EpochRecord::signed_bytes`), in this order,
`keys` and `snapshots` with keys sorted (they are `BTreeMap`s):

```
{"domain":"omacloud-epoch-v1","epoch":1,"prev":"…","head":57,"devices":4,"keys":{…},"snapshots":{…},"signer":"…"}
```

Bucket key record (`bucket_key.rs`, `KeyRecord::signed_bytes`), built with
`json!`, so all keys sorted:

```
{"devices":4,"domain":"omacloud-bucket-key-v1","keys":{…},"seq":2,"signer":"…"}
```

### Hash chains

Device entries, heads and epoch records chain by
`hash = hex(SHA-256(signed bytes || ascii hex sig))`, the next one's `prev`
(`DeviceEntry::hash`, `Head::hash`, `EpochRecord::hash`). The hash covers
every signed field and the signature, so it pins the entry exactly. Bucket
key records aren't chained.

### Sealing to a key

Epoch secrets, grants, bucket keys and secrets bundles are sealed to an
Ed25519 public key as a libsodium sealed box (`devices.rs`, `seal_to`,
`open_with`; crate `crypto_box` 0.9.1, feature `seal`):

- recipient X25519 key = the Ed25519 public key's Montgomery form
  (`VerifyingKey::to_montgomery`, as libsodium's
  `crypto_sign_ed25519_pk_to_curve25519`);
- recipient X25519 secret = `SigningKey::to_scalar_bytes` (the first half of
  SHA-512(seed), as `crypto_sign_ed25519_sk_to_curve25519`);
- sealed box: `ephemeral_pk(32) || XSalsa20-Poly1305(ciphertext || tag 16)`,
  nonce = BLAKE2b-192(ephemeral_pk || recipient_pk), as `crypto_box_seal`.
  So `crypto_box_seal_open` in libsodium opens one.

In JSON records (`keys`, `sealed`) the sealed box is hex of the box, whose
plaintext is the JSON of the secret (`epoch.rs`, `seal`, `open`). Each
epoch record seals to every member valid at `devices` and to the root
(`DeviceChain::holders`, `seal_to_holders`), so the recovery code opens
every epoch's secret; a device added later gets a grant. The bucket key
record seals the same way.

### Secrets bundle

`.omacloud/secrets/<device name>.sealed` in the snapshot tree is the raw
sealed box (not hex) to the **root** key only (`secrets.rs`, `seal`,
`open`), so only the recovery code opens it. Plaintext (`encode`, `decode`):

```
"omacloud-secrets-v1\n"
repeat:
  u32 LE   path length
  path     UTF-8, relative to home, plain components only
  u32 LE   mode (permission bits, & 0o7777)
  u64 LE   data length
  data
```

Entries are sorted by path; the total is at most 16 MiB (`collect`,
`MAX_BYTES`).

### Restic encryption

The repository's files and blobs are restic's: AES-256-CTR with
Poly1305-AES, `IV(16) || ciphertext || MAC(16)`, under the 64-byte master key
(encryption key 32, then Poly1305 `k` 16 and `r` 16), keys files under
scrypt (restic spec; `omacloud-web/src/lib.rs`, `Key`, `Reader::open`).

### Web links

`omacloud-web/src/link.rs`, `key`, `seal`, `unseal`; crate
`aes256ctr_poly1305aes` 0.2.1:

```
key64  = SHA-512("omacloud web link 1" || seed)      seed: 32 random bytes, no newline after the label
object = nonce(16) || AES-256-CTR-Poly1305-AES(key64, nonce, web code JSON)
```

The 64-byte key is laid out as restic's master key: the AES-256 key, then
Poly1305-AES's `k` and `r`, 16 bytes each (the crate's
`Aes256CtrPoly1305Aes::new`), and the object has restic's encrypted file
layout. The seed travels only in the link's fragment.

### Uploads

The browser holds a session key: a random Ed25519 seed (`inbox.rs`,
`session_key`), base64url in the web code's `upload.seed`, its public half
endorsed by a member for twelve hours (`Endorsement`; `main.rs`,
`UPLOAD_KEY_VALID`). For each file it signs the upload line (above) with
the session key, builds the plaintext of section 2, and encrypts it exactly
as restic encrypts a file, with the current repository's master key and a
random 16-byte nonce (`upload.rs`, `make`; `lib.rs`, `Reader::seal`).

A computer accepts one when (`inbox.rs`, `open`; `omacloud/src/uploads.rs`,
`take_all`): it decrypts with the current epoch's key; `v == 1`; the
endorsement verifies under `endorsement.device`; that device is a member
now; the object's last-modified time from the storage, in Unix seconds, is
`<= until`; the session signature verifies under `endorsement.key`; and the
path is safe. It saves the file into the folder, never over a different
existing file.

## 4. Verifying an account by hand

1. **Root.** Read `account.json`. With the recovery code, derive the root
   (section 3) and require it to equal `root`. With only a device key, the
   anchor is unsigned: take `root` from the computer's
   `~/.config/omacloud/config.toml` (`root`, pinned at setup; `main.rs`,
   `Config`), or confirm it some other way.
2. **Device chain.** Read `devices/` from 1 with no gaps. For each entry
   (`devices.rs`, `DeviceChain::push`): `seq` is one more than the last;
   `prev` is the last entry's hash (`""` for 1); the signature verifies
   under `signer`; entry 1 is the root adding a device; `signer` is the root
   or valid now; an `add` names a key never added, never revoked and not the
   root; a `revoke` names a valid device. Members after the first `n`
   entries are adds minus revokes up to `n` (`members_at`).
3. **Epochs.** Read `epochs/` from file 1 (epoch 0). For each
   (`epoch.rs`, `EpochChain::push`): `epoch` counts from 0; `prev` is the
   last record's hash; the signature verifies; `devices` is no more than the
   chain's length and no less than the previous record's; `signer` is valid
   at `devices`.
4. **Heads.** Read `heads/` from 1 with no gaps. For each (`head.rs`,
   `check`): `seq` and `prev` follow; the signature verifies under `device`;
   `devices` is no more than the verified chain's length and not less than
   the previous head's; `epoch` not less than the previous head's; `device`
   valid at `devices` (as of that position, not now: a revoked device's
   older heads stay valid).
5. **Current head.** The head with the highest seq once all of 4 passes
   (`HeadTracker::advance`). If its `epoch` has no record yet, a rotation is
   under way (its head lands before its record); wait and read again
   (`sync.rs`, `sync_rounds`, `EpochError::Pending`). A computer also
   refuses a log that ends before, or differs at, the head it last saw
   (`state.json` in its data directory, `heads.known`); a reader without
   that state can't detect a rollback or a withheld tail, only forgery.
6. **Key.** In the latest epoch record, open `keys[<your key>]`; if absent,
   find `grants/<your key>-<epoch>.json` with that epoch and device, check
   its signature and that its signer is the root or any device ever in the
   chain, and open `sealed` (`EpochChain::secret`). The root's entry is
   always in the record.
7. **Read.** The current head's `snapshot` is a snapshot id in
   `<root>/omacloud-e<epoch>` (or `<root>` for 0). Open that repository with
   the secret's `password` (restic, rustic) or its master key, and check the
   snapshot file's name is the id the head names (restic names files by
   their SHA-256). An older head's snapshot is in the current repository
   under its id mapped through each later record's `snapshots`
   (`EpochChain::map`); snapshots older than the 30-day window are gone, and
   a head pointing at one has nothing left to read (design.md, Storage).
8. **Bucket key record** (to trust a credential change): latest
   `keys/<seq>.json`, signature, `devices <= chain length`, signer valid at
   `devices` (`KeyRecord::verify`).

## 5. Versioning

- Version lives in each signed message's domain:
  `omacloud-recovery-v1`, `omacloud-device-v1`, `omacloud-head-v3`,
  `omacloud-epoch-v1`, `omacloud-grant-v1`, `omacloud-join-v1`,
  `omacloud-bucket-key-v1`, `omacloud-upload-key-v1`, `omacloud-agent-key-v1`, `omacloud-upload-v1`,
  `omacloud web link 1`, and the bundle magic `omacloud-secrets-v1\n`. A
  record whose signature doesn't verify under the current domain is
  refused; there is no fallback to an older one (only `v3` heads verify).
- Explicit version fields: an upload header's `v` (must be 1), the drive
  code's `v` (1), and restic's own config `version` (2).
- `Head.epoch` is `#[serde(default)]`, read as 0 when missing.
- No record type uses `deny_unknown_fields`, so serde ignores unknown
  fields. They are outside every signature (signed bytes are rebuilt from
  the known fields), so a reader must ignore them and never act on one.
  A writer that needs a new signed field needs a new domain.
- Records are JSON throughout; there is no other format field in the
  coordination tree.

## Not specified here

- **Device names.** `init` refuses a name with a control character from
  0.0.33 on (`main.rs`, `Cmd::Init`); an earlier version didn't, so a
  reader should refuse a join request whose name holds a newline, as its
  line form is then ambiguous. Device entries (JSON form) are unaffected.
- **`doorbell` in choices** and the merged documents' schemas
  (`.omacloud/merged/`; `merged.rs`, `wifi.rs`, `chromium/`).
- **A snapshot's `paths`**: the folder's absolute path on the computer that
  wrote it, as rustic parses it (`sync.rs`, `Engine::push`).
- **How long a rotation's head can stand without its epoch record.** In a
  bucket they are two writes (`Coordinator::append_rotation`'s default), so
  a reader can see a head in epoch `n` before record `n`.
