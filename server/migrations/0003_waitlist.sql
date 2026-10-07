-- People waiting for an invitation, by email (lower case). A Google sign-in
-- from the app that isn't invited lands here confirmed: Google checked the
-- address. An address typed on the website is confirmed by the link emailed
-- to it; until then only the hash of the link's token is kept, and the
-- scheduled job removes it once the link expires.
CREATE TABLE waitlist (
    email TEXT PRIMARY KEY,
    google_sub TEXT,
    created INTEGER NOT NULL,
    last_seen INTEGER NOT NULL,
    -- when it was confirmed; NULL: not yet
    confirmed INTEGER,
    token_hash TEXT UNIQUE,
    -- when the confirmation link went out
    token_sent INTEGER
);
CREATE INDEX waitlist_confirmed ON waitlist (confirmed);

-- when the invitation email went out. Invitations made before there were
-- emails count as told, so a deploy emails nobody out of the blue.
ALTER TABLE invites ADD COLUMN notified INTEGER;
UPDATE invites SET notified = created;
-- an email that didn't go is tried again later, each time waiting longer
-- (up to a day), so one address that can't be emailed holds up no other
ALTER TABLE invites ADD COLUMN notify_tries INTEGER NOT NULL DEFAULT 0;
ALTER TABLE invites ADD COLUMN notify_after INTEGER NOT NULL DEFAULT 0;

-- when each scheduled job (the admin's waitlist digest) last did its thing
CREATE TABLE jobs (
    name TEXT PRIMARY KEY,
    ran INTEGER NOT NULL
);
