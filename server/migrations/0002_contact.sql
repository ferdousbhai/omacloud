-- A trusted contact's pad: half of the account's recovery code, given only
-- to a Google sign-in of the account. The contact keeps the other half (see
-- crates/omacloud-core/src/contact.rs); either alone says nothing.
ALTER TABLE accounts ADD COLUMN contact_pad TEXT;
