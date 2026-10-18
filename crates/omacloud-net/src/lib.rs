//! The Omacloud network: an S3 bucket to the computer using it, held by
//! untrusted Omarchy computers. Trust comes from cryptography (restic's
//! encryption, hash-named objects, per-holder blobs, signatures, blind
//! tokens); availability from copies on independent networks.

pub mod blob;
pub mod client;
pub mod credit;
pub mod node;
pub mod place;
pub mod proto;
