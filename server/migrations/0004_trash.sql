-- Deleted and overwritten objects are kept for 14 days in the bucket's
-- trash, outside every account's folder (src/trash.ts), so a mistake or a
-- break-in can be undone. Per account: the bytes it holds (not counted in
-- the quota, but capped at it: at the cap, deletes are refused), its oldest
-- entry and its latest, so the scheduled job knows whom to purge without
-- listing everyone's. No reference to accounts: the trash of an account
-- that's gone still purges.
CREATE TABLE trash (
    account TEXT PRIMARY KEY,
    bytes INTEGER NOT NULL DEFAULT 0,
    -- when the oldest entry was made; NULL: none (0: unknown, walk it)
    oldest INTEGER,
    -- when the latest was counted
    added INTEGER NOT NULL DEFAULT 0,
    -- when the admin was last told it reached its cap
    alerted INTEGER,
    -- due a counting walk (the daily sweep asks for one); a walk under
    -- way: where it got to, the bytes and oldest entry made before
    -- `walk_from` it has seen, and the bytes added since `walk_from`
    recount INTEGER NOT NULL DEFAULT 0,
    walk_from INTEGER,
    walk_token TEXT,
    walk_sum INTEGER NOT NULL DEFAULT 0,
    walk_oldest INTEGER,
    walk_added INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX trash_oldest ON trash (oldest);

-- An admin's request (restore.sh) to put an account back as it was at
-- `at`, carried out by the scheduled job a batch at a time: `cursor` is the
-- last trash entry looked at, `restored` how many objects were put back.
-- After several failed runs in a row (`attempts`) it's given up: `failed`.
CREATE TABLE restores (
    id INTEGER PRIMARY KEY,
    account TEXT NOT NULL,
    at INTEGER NOT NULL,
    requested INTEGER NOT NULL,
    cursor TEXT,
    restored INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    done INTEGER,
    failed INTEGER
);
CREATE INDEX restores_done ON restores (done);

-- the objects a restore in progress has put back (or found as they were):
-- a later entry for the same object is a later version, and is skipped
CREATE TABLE restored (
    restore INTEGER NOT NULL,
    key TEXT NOT NULL,
    PRIMARY KEY (restore, key)
) WITHOUT ROWID;
