-- An account is a Google identity and a folder in the bucket, named by the
-- account's id. Keys' secrets aren't stored: each derives from the master
-- key and the key's id, so the database alone opens nothing.
CREATE TABLE accounts (
    id TEXT PRIMARY KEY,
    google_sub TEXT NOT NULL UNIQUE,
    email TEXT NOT NULL,
    created INTEGER NOT NULL,
    quota INTEGER NOT NULL,
    used INTEGER NOT NULL DEFAULT 0,
    -- written to since usage was last counted
    dirty INTEGER NOT NULL DEFAULT 0,
    -- a count in progress: where the listing got to, and the bytes so far
    scan_token TEXT,
    scan_total INTEGER NOT NULL DEFAULT 0,
    disabled INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX accounts_dirty ON accounts (dirty);

CREATE TABLE keys (
    id TEXT PRIMARY KEY,
    account TEXT NOT NULL REFERENCES accounts(id),
    created INTEGER NOT NULL,
    revoked INTEGER
);

CREATE TABLE invites (
    email TEXT PRIMARY KEY,
    created INTEGER NOT NULL
);

CREATE TABLE signins (
    id TEXT PRIMARY KEY,
    port INTEGER NOT NULL,
    state TEXT NOT NULL,
    challenge TEXT NOT NULL,
    created INTEGER NOT NULL,
    -- set once Google says who it is: the one-time code the computer
    -- trades for a key
    grant_code TEXT UNIQUE,
    account TEXT,
    used INTEGER NOT NULL DEFAULT 0
);
