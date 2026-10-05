//! omacloud sync engine.
//!
//! Folders sync live through restic repositories: [`sync::Engine`] pushes
//! local changes as snapshots and pulls other devices' snapshots, and
//! [`head`] keeps the order of snapshots signed and verifiable, so the
//! coordinator can relay history but not rewrite it.

pub mod account;
pub mod bucket;
pub mod bucket_key;
pub mod contact;
pub mod devices;
pub mod epoch;
pub mod head;
pub mod ignore;
pub mod repo;
pub mod secrets;
pub mod settings;
pub mod shamir;
pub mod sync;
pub mod throttle;

pub use bucket::BucketCoordinator;
pub use devices::{Anchor, DeviceChain, DeviceError, JoinRequest, fingerprint};
pub use head::{Coordinator, DirCoordinator, Head, HeadError, HeadTracker};
pub use ignore::Ignores;
pub use repo::RepoSpec;
pub use rustic_core::{jiff, repofile::MasterKey};
pub use sync::{
    Engine, Folders, Membership, PACKAGES, Rotated, SECRETS, SettingsSetup, SettingsStatus, Setup,
    State, Stats, Version,
};
