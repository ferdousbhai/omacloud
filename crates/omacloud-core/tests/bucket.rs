//! The coordinator in the user's own bucket, on OpenDAL's filesystem
//! service (the S3 path runs in scripts/s3-sync.sh): the whole account flow
//! with no service.

use std::{collections::BTreeMap, fs, path::Path, sync::Arc};

use anyhow::Result;
use ed25519_dalek::SigningKey;
use omacloud_core::{
    BucketCoordinator, Coordinator, Engine, Head, RepoSpec, Setup, account, devices::public_hex,
};

fn key() -> SigningKey {
    SigningKey::generate(&mut rand::rngs::OsRng)
}

fn coordinator(dir: &Path) -> Result<Arc<BucketCoordinator>> {
    let opts: BTreeMap<String, String> =
        [("root".to_string(), dir.to_string_lossy().into_owned())].into();
    Ok(Arc::new(BucketCoordinator::new("fs", &opts)?))
}

fn engine(
    coord: Arc<BucketCoordinator>,
    root: &str,
    name: &str,
    k: &SigningKey,
    repo: &RepoSpec,
    folder: &Path,
) -> Result<Engine> {
    fs::create_dir_all(folder)?;
    Engine::new(
        Setup {
            device: name.into(),
            folder: folder.to_path_buf(),
            repo: repo.clone(),
            signing: k.clone(),
            root: root.into(),
            state_path: None,
            settings: None,
            folders: None,
        },
        coord,
    )
}

#[test]
fn an_account_with_no_service() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let t = tmp.path();
    let repo = RepoSpec {
        repository: t.join("repo").to_string_lossy().into_owned(),
        options: BTreeMap::new(),
    };
    let coord_dir = t.join("coordination");
    let (ka, kb, kc) = (key(), key(), key());

    let created = account::create(coordinator(&coord_dir)?.as_ref(), &repo, &ka, "a")?;
    let root = created.root.clone();
    let mut a = engine(
        coordinator(&coord_dir)?,
        &root,
        "a",
        &ka,
        &repo,
        &t.join("A"),
    )?;
    fs::write(t.join("A/hello.txt"), "hi\n")?;
    a.sync()?;

    // b joins with the recovery code, c by approval
    account::join_with_recovery(
        coordinator(&coord_dir)?.as_ref(),
        &created.recovery_code,
        &kb,
        "b",
    )?;
    let mut b = engine(
        coordinator(&coord_dir)?,
        &root,
        "b",
        &kb,
        &repo,
        &t.join("B"),
    )?;
    b.sync()?;
    assert_eq!(fs::read_to_string(t.join("B/hello.txt"))?, "hi\n");
    account::request(coordinator(&coord_dir)?.as_ref(), &kc, "c")?;
    let req = a.requests()?.pop().expect("c's request");
    a.approve(&req)?;
    let mut c = engine(
        coordinator(&coord_dir)?,
        &root,
        "c",
        &kc,
        &repo,
        &t.join("C"),
    )?;
    c.sync()?;
    fs::write(t.join("C/from-c.txt"), "c\n")?;
    c.notice(["from-c.txt".into()]);
    c.sync()?;
    a.sync()?;
    assert_eq!(fs::read_to_string(t.join("A/from-c.txt"))?, "c\n");

    // two devices racing for the same position: the bucket lets one in
    let (x, y) = (coordinator(&coord_dir)?, coordinator(&coord_dir)?);
    let latest = x.head()?.expect("a head");
    let hx = Head::next_in(&ka, Some(&latest), &latest.snapshot, 3, 0);
    let hy = Head::next_in(&kb, Some(&latest), &latest.snapshot, 3, 0);
    assert!(x.append(&hx)?);
    assert!(!y.append(&hy)?);

    // remove b, then change the bucket key: sealed to a and c, not b
    a.sync()?;
    a.revoke(&public_hex(&kb))?;
    let new_key: BTreeMap<String, String> = [
        ("access_key_id".to_string(), "new-id".to_string()),
        ("secret_access_key".to_string(), "new-secret".to_string()),
    ]
    .into();
    assert_eq!(a.change_bucket_key(&new_key)?, 1);
    let (seq, key) = c.latest_bucket_key()?.expect("a key change");
    assert_eq!((seq, &key), (1, &new_key));
    c.ack_bucket_key(seq)?;
    let (_, acked) = a.bucket_key_status()?.expect("a key change");
    assert!(acked.contains(&public_hex(&ka)) && acked.contains(&public_hex(&kc)));
    assert!(!acked.contains(&public_hex(&kb)));
    let err = b.latest_bucket_key().unwrap_err();
    assert!(format!("{err:#}").contains("removed"), "{err:#}");

    assert_eq!(a.rotate()?.epoch, 1);
    c.sync()?;
    assert_eq!(c.repository()?.2, 1);
    assert!(b.sync().is_err());
    Ok(())
}

#[test]
fn every_write_moves_the_change_marker() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let t = tmp.path();
    let repo = RepoSpec {
        repository: t.join("repo").to_string_lossy().into_owned(),
        options: BTreeMap::new(),
    };
    let coord_dir = t.join("coordination");
    let ka = key();
    let created = account::create(coordinator(&coord_dir)?.as_ref(), &repo, &ka, "a")?;
    let mut a = engine(
        coordinator(&coord_dir)?,
        &created.root,
        "a",
        &ka,
        &repo,
        &t.join("A"),
    )?;
    a.sync()?;
    let watcher = coordinator(&coord_dir)?;
    let before = watcher.marker()?;
    assert!(before.is_some());
    // nothing written: the same marker
    a.sync()?;
    assert_eq!(watcher.marker()?, before);
    // a push, and a join request: each moves it
    fs::write(t.join("A/new.txt"), "new\n")?;
    a.request_rescan();
    assert!(a.sync()?.pushed > 0);
    let after_push = watcher.marker()?;
    assert_ne!(after_push, before);
    account::request(coordinator(&coord_dir)?.as_ref(), &key(), "b")?;
    assert_ne!(watcher.marker()?, after_push);
    Ok(())
}
