//! Failures as the output contract's error codes and exit codes. A command
//! raises a [`CliError`] for what it knows (not set up, not found, a refused
//! code); it travels in the `anyhow::Error` and [`classify`] finds it. The
//! rest is recognised by type: the sync engine's membership errors, the
//! storage refusing the key or being out of reach, a network failure. Anything
//! else is `error`, exit 1.
//!
//! | code | exit |
//! |---|---|
//! | `error` (anything unrecognised), `verification_failed`, `pending`, `command_failed`, `cancelled` | 1 |
//! | `usage`, `confirmation_required`, `terminal_required`, `ambiguous`, `already_set_up`, `exists`, `unsupported`, `same_key` | 2 |
//! | `not_set_up`, `storage_gone`, `not_approved`, `removed`, `recovery_code_rejected`, `join_code_rejected`, `no_account`, `key_refused`, `shares_rejected`, `settings_off` | 3 |
//! | `not_found` | 4 |
//! | `unavailable` | 5 |

use omacloud_core::{
    DeviceError, HeadError, Membership,
    bucket::{NoCreateOnly, key_refused as storage_refused_key, unreachable},
    epoch::EpochError,
};

pub use crate::output::CliError;
use crate::output::{EXIT_ERROR, EXIT_NOT_FOUND, EXIT_SETUP, EXIT_UNAVAILABLE, EXIT_USAGE};

/// A computer set up on storage Omacloud dropped.
pub fn storage_gone(kind: &str) -> CliError {
    CliError::new(
        "storage_gone",
        EXIT_SETUP,
        format!("this computer was set up on {kind}, which Omacloud no longer uses"),
    )
    .hint("run `omacloud init` with your bucket to set it up again (the old config is kept aside); the files here stay as they are")
}

/// The storage refusing a key this command was just given.
pub fn key_refused(message: impl Into<String>) -> CliError {
    setup("key_refused", message).hint("check the key's id and secret, and that it may read and write this bucket")
}

pub fn usage(message: impl Into<String>) -> CliError {
    CliError::new("usage", EXIT_USAGE, message)
}

pub fn unsupported(message: impl Into<String>) -> CliError {
    CliError::new("unsupported", EXIT_USAGE, message)
}

pub fn not_found(message: impl Into<String>) -> CliError {
    CliError::new("not_found", EXIT_NOT_FOUND, message)
}

pub fn setup(code: &str, message: impl Into<String>) -> CliError {
    CliError::new(code, EXIT_SETUP, message)
}

pub fn unavailable(message: impl Into<String>) -> CliError {
    CliError::new("unavailable", EXIT_UNAVAILABLE, message)
}

pub fn not_set_up() -> CliError {
    setup("not_set_up", "this computer isn't set up with Omacloud")
        .hint("run `omacloud init` in a terminal, or set it up in the Omacloud app")
}

pub fn recovery_code_rejected(message: impl Into<String>) -> CliError {
    setup("recovery_code_rejected", message).hint("check the recovery code and try again")
}

pub fn settings_off(what: &str) -> CliError {
    setup(
        "settings_off",
        format!("{what} travel with settings sync, which is off here"),
    )
    .hint("turn it on with `omacloud settings on`")
}

/// A command that prints a secret, asked to print it anywhere but a
/// person's terminal.
pub fn terminal_required(what: &str) -> CliError {
    CliError::new(
        "terminal_required",
        EXIT_USAGE,
        format!("this prints {what}, so it only prints to a terminal, never as JSON or into a pipe"),
    )
    .hint(
        "run it yourself in a terminal (stdin and stdout), without --json, --quiet, --ids-only or --count, and \
         keep what it shows private",
    )
}

/// A destructive command without `--yes`, where nobody can be asked.
pub fn confirmation_required(what: &str) -> CliError {
    CliError::new("confirmation_required", EXIT_USAGE, format!("{what} needs confirming")).hint("pass --yes")
}

/// The contract's error for `e`: a [`CliError`] it carries, else what its
/// type says, else `error`, exit 1.
pub fn classify(e: &anyhow::Error) -> CliError {
    if let Some(known) = e.chain().find_map(|c| c.downcast_ref::<CliError>()) {
        return known.clone();
    }
    let message = format!("{e:#}");
    match e.downcast_ref::<Membership>() {
        Some(Membership::NotApproved) => {
            return setup("not_approved", message)
                .hint("on a computer that syncs, run `omacloud devices list` and approve this one by its fingerprint");
        }
        Some(Membership::Revoked) => {
            return setup("removed", message)
                .hint("its files here stay; to sync it again, move its config and data directory aside and run `omacloud init`");
        }
        None => {}
    }
    if is_waiting(e) {
        return CliError::new("pending", EXIT_ERROR, message).hint("try again in a minute");
    }
    if is_rollback(e) {
        return CliError::new("verification_failed", EXIT_ERROR, message).hint(
            "the bucket's records are older than this computer has seen: it was restored from an old \
             copy, or someone with its key rolled it back; check the account from another computer",
        );
    }
    if is_unverified(e) {
        return CliError::new("verification_failed", EXIT_ERROR, message)
            .hint("the storage served a history this computer refuses; check the account from another computer");
    }
    if e.downcast_ref::<NoCreateOnly>().is_some() {
        return unsupported(message).hint(
            "use another bucket or provider; on Hetzner a bucket with object lock keeps its records in a second, \
             plain bucket (--coordination-bucket)",
        );
    }
    if storage_refused_key(e) {
        return key_refused(format!("the storage refused this computer's key: {message}")).hint(
            "if the account changed its key while this computer was away, get the new key from your provider \
             and run `omacloud bucket set-key --access-key-id <id> --here-only`",
        );
    }
    if unreachable(e) || e.chain().any(network_failure) {
        return unavailable(message).hint("check the network connection and that the storage is up, then try again");
    }
    CliError::new("error", EXIT_ERROR, message)
}

/// A key rotation in progress, or a grant not written yet: worth waiting for.
pub fn is_waiting(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<EpochError>(),
        Some(EpochError::Pending { .. } | EpochError::NoKey { .. })
    )
}

/// Records older than what this computer saw: the bucket was restored from
/// an old copy, or someone with its key rolled it back.
pub fn is_rollback(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<HeadError>(), Some(HeadError::Rollback { .. }))
        || matches!(e.downcast_ref::<DeviceError>(), Some(DeviceError::Rollback { .. }))
        || matches!(e.downcast_ref::<EpochError>(), Some(EpochError::Rollback { .. }))
}

/// A history this computer refuses: heads, devices or keys that don't verify.
pub fn is_unverified(e: &anyhow::Error) -> bool {
    e.downcast_ref::<HeadError>().is_some()
        || e.downcast_ref::<DeviceError>().is_some()
        || e.downcast_ref::<EpochError>().is_some()
}

/// A connection that couldn't be made or was lost.
fn network_failure(e: &(dyn std::error::Error + 'static)) -> bool {
    use std::io::ErrorKind as K;
    e.downcast_ref::<std::io::Error>().is_some_and(|e| {
        matches!(
            e.kind(),
            K::ConnectionRefused
                | K::ConnectionReset
                | K::ConnectionAborted
                | K::NotConnected
                | K::TimedOut
                | K::HostUnreachable
                | K::NetworkUnreachable
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_failures_keep_their_code_and_exit() {
        let e: anyhow::Error = not_found("no device abc").into();
        let e = e.context("approving");
        let c = classify(&e);
        assert_eq!(
            (c.code.as_str(), c.exit, c.message.as_str()),
            ("not_found", 4, "no device abc")
        );
    }

    #[test]
    fn membership_and_network_by_type() {
        let c = classify(&anyhow::Error::new(Membership::NotApproved));
        assert_eq!((c.code.as_str(), c.exit), ("not_approved", 3));
        let c = classify(&anyhow::Error::new(Membership::Revoked));
        assert_eq!((c.code.as_str(), c.exit), ("removed", 3));
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let c = classify(&anyhow::Error::new(io).context("reaching the storage"));
        assert_eq!((c.code.as_str(), c.exit), ("unavailable", 5));
        assert!(c.hint.is_some());
        let c = classify(&anyhow::anyhow!("something else"));
        assert_eq!(
            (c.code.as_str(), c.exit, c.message.as_str()),
            ("error", 1, "something else")
        );
    }
}
