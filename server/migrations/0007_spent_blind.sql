-- The blind message each spent token was traded for (its sha256), so an
-- exchange asked again, its answer lost, is signed again rather than paid
-- nothing. A blind message can't be tied to the token it becomes.
ALTER TABLE net_spent ADD COLUMN blind TEXT;
