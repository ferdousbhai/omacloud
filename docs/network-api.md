# The network's API at the Worker

What computers ask `https://omacloud.computer/api/net/`. The Worker keeps
the node directory, each bucket's sealed index, each account's coordination
records, and issues storage tokens. It has no accounts, users or sign-in:
writes are allowed by capabilities the computers derive themselves, and
the Worker stores only their hashes. It can't read index entries or
coordination bodies (sealed by the clients), and nothing links a node to
an account or a token to whoever spent it.

JSON in and out. Errors are `{"error": "<message>"}` with a 4xx/5xx status.
Binary values are hex unless noted. `now` is Unix seconds.

## Proofs

- **Node signature**, for what a node says about itself: headers
  `x-node` (its 32-byte Ed25519 public key, the iroh endpoint id),
  `x-time` (Unix seconds, within 300 s of the Worker's clock) and `x-sig`
  (64-byte Ed25519 signature) over the UTF-8 string
  `omacloud-node-v1\n{METHOD}\n{path}\n{x-time}\n{hex sha256 of the body}`.
  `path` is the URL path without the query.
- **Capability**: header `x-cap`, a secret the client derived. The Worker
  keeps `hex(sha256(cap))` and compares hashes.

## Nodes

- `POST /api/net/nodes`, node-signed. Body `{"addr": <iroh EndpointAddr as
  serde_json gives it, kept verbatim>, "capacity": <bytes it gives; 0 unlists it>}`. Upserts
  the node, notes this hour as seen, and answers
  `{"asn": <number>, "prefix": "<prefix>"}`: the autonomous system
  (`request.cf.asn`, 0 when absent) and the /24 (IPv4, `a.b.c.0/24`) or /48
  (IPv6, `x:y:z::/48`) of `CF-Connecting-IP`, as the Worker saw them. A
  node can't declare its own. Nodes heartbeat every 15 minutes. At most 20
  new node ids per prefix per day.
- `GET /api/net/nodes` answers `{"nodes": [{"id", "asn", "prefix", "uptime",
  "probation", "since", "addr"}]}`: every node seen in the last 72 hours with
  `capacity > 0`. `since` is when it was first seen (Unix seconds). `uptime` is hours seen in the last 30 days over
  `min(720, hours since first seen)`, at most 1. `probation` is true for a
  node first seen less than 14 days ago.

## Index

Scoped by header `x-scope` (a 128-bit secret as 32 hex characters, derived
per epoch); the Worker keys entries by `sha256(scope)`. `id` is 32 hex
characters; `sealed` is base64, at most 64 KiB.

- `GET /api/net/index?after={id}` answers `{"entries": [{"id", "sealed"}],
  "next"}`: up to 1000 entries after `id` (from the start without it), in
  id order, and `next` (the last id) when there may be more.
- `GET /api/net/index/{id}` answers `{"sealed"}`, or 404.
- `PUT /api/net/index/{id}` with `{"sealed"}` adds or replaces it: 204.
- `DELETE /api/net/index/{id}`: 204, whether or not it was there.

## Coordination

An account is a random 128-bit id (32 hex characters), `{account}` below.
Bodies are opaque strings (the clients seal them), at most 256 KiB. Reads
need only the account id, which a computer gets in a join code; writes
need the account's capability, except join requests.

- `PUT /api/net/coord/{account}` with `x-cap`: creates the account with
  that capability. 201, or 409 when it exists.
- `PUT /api/net/coord/{account}/cap` with the current `x-cap` and body
  `{"cap": "<new>"}`: replaces the capability (a new epoch). 204.
- `GET /api/net/coord/{account}/{log}?after={n}`, `log` one of `heads`,
  `devices`, `epochs`: `{"records": [{"seq", "body"}], "next"}` with `seq >
  n`, in order, at most 1000; `next` (the last seq) when there may be more.
- `PUT /api/net/coord/{account}/{log}/{seq}` with `x-cap` and `{"body"}`:
  create-only. 201, or 409 when that position is taken.
- `GET /api/net/coord/{account}/anchor`: `{"body"}` or 404.
  `PUT` (with `x-cap`, create-only): 201 or 409.
- `GET /api/net/coord/{account}/grants?name={prefix}`:
  `{"records": [{"name", "body"}]}` whose name starts with the prefix.
  `PUT` and `DELETE /api/net/coord/{account}/grants/{name}` (with
  `x-cap`): 204.
- `GET /api/net/coord/{account}/nodes`: `{"records": [{"name", "body"}]}`,
  the account's computers' node ids (sealed), so none is chosen to hold the
  account's copies. `PUT` and `DELETE /api/net/coord/{account}/nodes/{name}`
  (with `x-cap`): 204.
- `GET /api/net/coord/{account}/bases`: `{"records": [{"name", "body"}]}`,
  the version each of the account's computers last synced (sealed), kept
  when older versions go. `PUT` and `DELETE /api/net/coord/{account}/bases/{name}`
  (with `x-cap`): 204.
- `GET /api/net/coord/{account}/requests`: `{"records": [{"name", "body"}]}`.
  `PUT /api/net/coord/{account}/requests/{name}` needs no capability (a
  computer asking to join has none), at most 10 per prefix per hour: 204.
  `DELETE` (with `x-cap`): 204.
- `GET /api/net/coord/{account}/marker`: `{"version"}`, a number that
  changes whenever anything of the account is written.

Reads of an account the Worker doesn't have answer as for an empty one
(no records, marker 0), so its computers see a rollback and fill it again;
writes to it answer 404.

`name` and `log` are `[A-Za-z0-9_-]{1,200}`.

## Credits

Tokens are RFC 9474 RSA blind signatures, RSABSSA-SHA384-PSS-Randomized: a
token is `{"msg", "randomizer", "sig"}` (hex), valid when `sig` is an
RSASSA-PSS (SHA-384, salt 48) signature over `randomizer ‖ msg`. One token
is one GiB kept for a month.

- `GET /api/net/credits/key`: `{"spki": "<base64 DER SubjectPublicKeyInfo>"}`.
- `POST /api/net/credits/starter` with `{"blind": ["<hex>"]}`: signs up to
  the starter grant (`STARTER_TOKENS`, 10 unless set) per prefix per 30
  days, `{"signed": ["<hex>"]}`; more asked than left is refused (429).
- `POST /api/net/credits/exchange` with `{"tokens": [<token>], "blind":
  ["<hex>"]}`: checks each token's signature and that it wasn't spent
  (spent ids, `sha256(msg)`, kept for good), marks the unspent good ones
  spent and signs that many of the blind messages, the first ones,
  `{"signed": ["<hex>"], "spent": <n>}`: a token spent before buys nothing
  but doesn't sink the others. More blind messages than good tokens is
  refused (400), nothing spent. At most 100 tokens a request.

A blind signature is `m^d mod n` of the blind message `m` (big-endian, the
modulus' length, `m < n`), checked by `s^e mod n == m` before it's sent.
