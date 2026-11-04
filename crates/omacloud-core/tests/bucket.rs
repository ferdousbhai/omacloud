//! The coordinator in the user's own bucket, on OpenDAL's filesystem
//! service (the S3 path runs in scripts/s3-sync.sh): the whole account flow
//! with no service.

use std::{collections::BTreeMap, fs, path::Path, sync::Arc};

use anyhow::Result;
use ed25519_dalek::SigningKey;
use omacloud_core::{
    BucketCoordinator, Coordinator, Engine, Head, KeyChange, RepoSpec, Setup, account, bucket_key::Credentials,
    devices::public_hex,
};

fn key() -> SigningKey {
    SigningKey::generate(&mut rand::rngs::OsRng)
}

fn coordinator(dir: &Path) -> Result<Arc<BucketCoordinator>> {
    let opts: BTreeMap<String, String> = [("root".to_string(), dir.to_string_lossy().into_owned())].into();
    Ok(Arc::new(BucketCoordinator::new("fs", &opts)?))
}

fn repo_in(dir: &Path) -> RepoSpec {
    RepoSpec {
        repository: dir.join("repo").to_string_lossy().into_owned(),
        options: BTreeMap::new(),
    }
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
    let repo = repo_in(t);
    let coord_dir = t.join("coordination");
    let (ka, kb, kc) = (key(), key(), key());

    let created = account::create(coordinator(&coord_dir)?.as_ref(), &repo, &ka, "a")?;
    let root = created.root.clone();
    let mut a = engine(coordinator(&coord_dir)?, &root, "a", &ka, &repo, &t.join("A"))?;
    fs::write(t.join("A/hello.txt"), "hi\n")?;
    a.sync()?;

    // b joins with the recovery code, c by approval
    account::join_with_recovery(coordinator(&coord_dir)?.as_ref(), &created.recovery_code, &kb, "b")?;
    let mut b = engine(coordinator(&coord_dir)?, &root, "b", &kb, &repo, &t.join("B"))?;
    b.sync()?;
    assert_eq!(fs::read_to_string(t.join("B/hello.txt"))?, "hi\n");
    account::request(coordinator(&coord_dir)?.as_ref(), &kc, "c")?;
    let req = a.requests()?.pop().expect("c's request");
    a.approve(&req)?;
    let mut c = engine(coordinator(&coord_dir)?, &root, "c", &kc, &repo, &t.join("C"))?;
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
    let new_key = Credentials {
        access_key_id: "new-id".into(),
        secret_access_key: "new-secret".into(),
    };
    assert_eq!(a.change_bucket_key(&new_key)?, 1);
    assert_eq!(c.key_change_since(0)?, KeyChange::Switch(1, new_key));
    // seen already: nothing to verify or open
    assert_eq!(c.key_change_since(1)?, KeyChange::None);
    c.ack_bucket_key(1)?;
    let (_, acked) = a.bucket_key_status()?.expect("a key change");
    assert!(acked.contains(&public_hex(&ka)) && acked.contains(&public_hex(&kc)));
    assert!(!acked.contains(&public_hex(&kb)));
    let err = b.key_change_since(0).unwrap_err();
    assert!(format!("{err:#}").contains("removed"), "{err:#}");
    // a computer joining after the change came with a key of its own
    let kd = key();
    account::join_with_recovery(coordinator(&coord_dir)?.as_ref(), &created.recovery_code, &kd, "d")?;
    let mut d = engine(coordinator(&coord_dir)?, &root, "d", &kd, &repo, &t.join("D"))?;
    assert_eq!(d.key_change_since(0)?, KeyChange::CaughtUp(1));

    assert_eq!(a.rotate()?.epoch, 1);
    c.sync()?;
    assert_eq!(c.repository()?.2, 1);
    assert!(b.sync().is_err());
    Ok(())
}

#[test]
fn a_write_others_need_moves_the_change_marker() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let t = tmp.path();
    let repo = repo_in(t);
    let coord_dir = t.join("coordination");
    let ka = key();
    let created = account::create(coordinator(&coord_dir)?.as_ref(), &repo, &ka, "a")?;
    let mut a = engine(coordinator(&coord_dir)?, &created.root, "a", &ka, &repo, &t.join("A"))?;
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

/// The check every new account's bucket gets, on storage that does it right.
#[test]
fn a_bucket_that_refuses_a_second_create_passes_the_check() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let c = coordinator(&tmp.path().join("coordination"))?;
    c.check()?;
    // again: it cleans up after itself
    c.check()?;
    assert_eq!(c.anchor()?, None);
    Ok(())
}

/// A stray object far past the end of a log, as anyone with the bucket key
/// could write, is an error to report, not a crash or an endless read.
#[test]
fn a_stray_entry_far_ahead_is_refused() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let t = tmp.path();
    let repo = repo_in(t);
    let coord_dir = t.join("coordination");
    let ka = key();
    let created = account::create(coordinator(&coord_dir)?.as_ref(), &repo, &ka, "a")?;
    let mut a = engine(coordinator(&coord_dir)?, &created.root, "a", &ka, &repo, &t.join("A"))?;
    fs::write(t.join("A/hello.txt"), "hi\n")?;
    a.sync()?;
    let head = fs::read(coord_dir.join("heads/00000000000000000001.json"))?;
    for seq in [u64::MAX, 1 << 40] {
        let stray = coord_dir.join(format!("heads/{seq}.json"));
        fs::write(&stray, &head)?;
        // a computer starting fresh lists the log
        let c = coordinator(&coord_dir)?;
        assert!(c.head().is_err());
        assert!(c.since(0).is_err());
        fs::remove_file(stray)?;
    }
    assert!(coordinator(&coord_dir)?.head()?.is_some());
    Ok(())
}
