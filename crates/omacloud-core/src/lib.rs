//! omacloud sync engine.
//!
//! Folders sync live through restic repositories: [`sync::Engine`] pushes
//! local changes as snapshots and pulls other devices' snapshots, and
//! [`head`] keeps the order of snapshots signed and verifiable, so the
//! coordinator can relay history but not rewrite it.

pub mod account;
pub mod agents;
pub mod browse;
pub mod chromium;
pub mod devices;
pub mod epoch;
pub mod head;
pub mod ignore;
pub mod merged;
pub mod repo;
pub mod secrets;
pub mod settings;
pub mod shamir;
pub mod sync;
pub mod throttle;
pub mod wifi;

pub use devices::{Anchor, DeviceChain, DeviceError, JoinRequest, fingerprint};
pub use head::{Coordinator, DirCoordinator, Head, HeadError, HeadTracker};
pub use ignore::Ignores;

/// How long earlier versions and deleted files stay restorable: the least
/// of what iCloud, Dropbox and Google Drive keep.
pub const VERSIONS_KEPT: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 3600);
pub use repo::RepoSpec;
pub use rustic_core::{jiff, repofile::MasterKey};
pub use sync::{
    Deleted, Engine, Folders, Membership, PACKAGES, Rotated, SECRETS, SettingsSetup, SettingsStatus, Setup, State,
    Stats, Version,
};
