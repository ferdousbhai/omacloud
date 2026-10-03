//! Creating an account and bringing devices into it.
//!
//! Joining needs no repository password: a device is approved by a member,
//! or signs itself in with the recovery code, and then receives the
//! repository key sealed to it (see [`crate::epoch`]). The repository
//! password only matters for leaving omacloud (`omacloud export`).

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use ed25519_dalek::SigningKey;

use crate::{
    RepoSpec,
    devices::{
        Action, Anchor, DeviceChain, JoinRequest, genesis, new_recovery_code, public_hex,
        random_secret, root_key,
    },
    epoch::{EpochChain, EpochRecord, EpochSecret, Grant},
    head::Coordinator,
};

/// A new account. Show the recovery code once.
pub struct Created {
    pub recovery_code: String,
    pub root: String,
    /// The repository password, for `omacloud export`.
    pub password: String,
}

/// Create the repository and the account: a recovery code and its root key,
/// `device` as the first member, and the repository secret sealed to that
/// device and to the root.
///
/// # Errors
///
/// If the coordinator already serves an account, or the repository can't be
/// created (for example because one exists there).
pub fn create(
    coord: &dyn Coordinator,
    repo: &RepoSpec,
    device: &SigningKey,
    name: &str,
) -> Result<Created> {
    ensure!(
        coord.anchor()?.is_none(),
        "an account already exists here; join it instead"
    );
    let recovery_code = new_recovery_code();
    let root_key = root_key(&recovery_code)?;
    let root = public_hex(&root_key);
    ensure!(
        coord.set_anchor(&Anchor { root: root.clone() })?,
        "an account was created here at the same moment; join it instead"
    );
    ensure!(
        coord.append_device(&genesis(&root_key, device, name))?,
        "the device list is not empty"
    );
    // the repository comes after the first device: storage through the
    // service answers members only
    let password = random_secret();
    let key = repo.init(&password)?;
    let mut chain = DeviceChain::new(&root);
    chain.advance(coord)?;
    let secret = EpochSecret {
        password: password.clone(),
        key,
        storage: repo.is_bucket().then(|| repo.without_local_options()),
    };
    let record = EpochRecord::next(device, None, 0, &chain, &secret, BTreeMap::new())?;
    ensure!(coord.append_epoch(&record)?, "the account already has keys");
    Ok(Created {
        recovery_code,
        root,
        password,
    })
}

/// Ask to join. Returns the account root this device now pins; the member
/// who approves checks it against the real one.
///
/// # Errors
///
/// If there is no account here.
pub fn request(coord: &dyn Coordinator, device: &SigningKey, name: &str) -> Result<String> {
    let root = coord
        .anchor()?
        .context("no account here yet; create one first")?
        .root;
    coord.request(&JoinRequest::new(device, name, &root))?;
    Ok(root)
}

/// Join with the recovery code: the root adds this device and grants it the
/// current repository key. Needs no other device.
///
/// # Errors
///
/// If the code doesn't belong to this account, or the account's records
/// don't verify.
pub fn join_with_recovery(
    coord: &dyn Coordinator,
    code: &str,
    device: &SigningKey,
    name: &str,
) -> Result<String> {
    let root_key = root_key(code)?;
    let root = public_hex(&root_key);
    let hint = coord.anchor()?.context("no account here yet")?.root;
    if hint != root {
        bail!("that recovery code belongs to another account");
    }
    let mut chain = DeviceChain::new(&root);
    chain.advance(coord)?;
    let me = public_hex(device);
    if !chain.members().valid.contains_key(&me) {
        let action = Action::Add {
            device: me.clone(),
            name: name.to_string(),
        };
        chain.append(coord, &root_key, action)?;
    }
    let mut epochs = EpochChain::default();
    epochs.advance(coord, &chain)?;
    let (epoch, secret) = epochs.secret(coord, &chain, &root_key)?;
    coord.put_grant(&Grant::new(&root_key, epoch, &me, &secret)?)?;
    Ok(root)
}
