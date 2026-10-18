-- The storage network (src/net/, docs/network-api.md). Nothing here names a
-- person or holds an address: nodes are their keys, accounts and index
-- scopes are random ids and hashes of secrets the computers keep, and a
-- request's origin is kept only as its /24 or /48, for limits.

-- Storage nodes, by Ed25519 key (hex). `addr` is the iroh address the node
-- gave, as JSON; `asn` and `prefix` are what the Worker saw it come from.
CREATE TABLE net_nodes (
    id TEXT PRIMARY KEY,
    addr TEXT NOT NULL,
    capacity INTEGER NOT NULL,
    asn INTEGER NOT NULL,
    prefix TEXT NOT NULL,
    first_seen INTEGER NOT NULL,
    last_seen INTEGER NOT NULL
);
CREATE INDEX net_nodes_seen ON net_nodes (last_seen);

-- the hours (Unix seconds / 3600) a node was seen in, for its uptime; kept
-- 30 days
CREATE TABLE net_node_hours (
    node TEXT NOT NULL,
    hour INTEGER NOT NULL,
    PRIMARY KEY (node, hour)
) WITHOUT ROWID;

-- A bucket's index: sealed entries under sha256 of the scope the clients
-- derive, never the scope itself.
CREATE TABLE net_index (
    scope TEXT NOT NULL,
    id TEXT NOT NULL,
    sealed TEXT NOT NULL,
    PRIMARY KEY (scope, id)
) WITHOUT ROWID;

-- Coordination: an account is a random id with the hash of its write
-- capability; `version` goes up with every write of the account.
CREATE TABLE net_accounts (
    id TEXT PRIMARY KEY,
    cap TEXT NOT NULL,
    version INTEGER NOT NULL
);

-- the append-only logs (heads, devices, epochs): a position is taken once
CREATE TABLE net_logs (
    account TEXT NOT NULL,
    log TEXT NOT NULL,
    seq INTEGER NOT NULL,
    body TEXT NOT NULL,
    PRIMARY KEY (account, log, seq)
) WITHOUT ROWID;

-- the named records: the anchor (name ''), grants and join requests
CREATE TABLE net_records (
    account TEXT NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    body TEXT NOT NULL,
    PRIMARY KEY (account, kind, name)
) WITHOUT ROWID;

-- Credits: sha256 of each spent token's message, kept for good. Nothing
-- says who spent it or what it was traded for.
CREATE TABLE net_spent (
    id TEXT PRIMARY KEY
) WITHOUT ROWID;

-- counters per prefix (new nodes a day, starter grants a month, join
-- requests an hour), each gone when its window ends
CREATE TABLE net_counts (
    kind TEXT NOT NULL,
    key TEXT NOT NULL,
    n INTEGER NOT NULL,
    until INTEGER NOT NULL,
    PRIMARY KEY (kind, key)
) WITHOUT ROWID;
CREATE INDEX net_counts_until ON net_counts (until);
