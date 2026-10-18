-- Hosted storage, Google sign-in and the waitlist are gone: their tables go too.
DROP INDEX IF EXISTS restores_done;
DROP INDEX IF EXISTS trash_oldest;
DROP INDEX IF EXISTS waitlist_confirmed;
DROP INDEX IF EXISTS accounts_dirty;
DROP TABLE IF EXISTS restored;
DROP TABLE IF EXISTS restores;
DROP TABLE IF EXISTS trash;
DROP TABLE IF EXISTS jobs;
DROP TABLE IF EXISTS waitlist;
DROP TABLE IF EXISTS signins;
DROP TABLE IF EXISTS invites;
DROP TABLE IF EXISTS keys;
DROP TABLE IF EXISTS accounts;
