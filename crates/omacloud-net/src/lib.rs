//! The Omacloud network: an S3 bucket to the computer using it, held by
//! untrusted Omarchy computers. Trust comes from cryptography (restic's
//! encryption, hash-named objects, per-holder blobs, signatures, blind
//! tokens); availability from copies on independent networks.

pub mod blob;
pub mod bucket;
pub mod client;
pub mod coord;
pub mod credit;
pub mod gateway;
pub mod node;
pub mod place;
pub mod proto;
pub mod records;
pub mod registry;
pub mod seal;
pub mod upkeep;
pub mod worker;
