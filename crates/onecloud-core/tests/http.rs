//! Against the real coordinator service (`service/` under `wrangler dev`):
//!
//!   scripts/coordinator-e2e.sh
//!
//! Skipped unless ONECLOUD_COORDINATOR_URL is set.

use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use ed25519_dalek::SigningKey;
use onecloud_core::{
    Coordinator, Engine, Head, HttpCoordinator, Membership, RepoSpec, Setup, account,
    devices::{public_hex, root_key},
};

fn key() -> SigningKey {
    SigningKey::generate(&mut rand::rngs::OsRng)
}

struct Device {
    engine: Engine,
    coord: Arc<HttpCoordinator>,
}

fn device(
    url: &str,
    root: &str,
    name: &str,
    signing: &SigningKey,
    repo: &RepoSpec,
    dir: &Path,
) -> Result<Device> {
    fs::create_dir_all(dir)?;
    let coord = Arc::new(HttpCoordinator::new(
        url,
        Some(root.to_string()),
        signing.clone(),
    ));
    let engine = Engine::new(
        Setup {
            device: name.into(),
            folder: dir.to_path_buf(),
            repo: repo.clone(),
            signing: signing.clone(),
            root: root.into(),
            state_path: None,
            settings: None,
            folders: None,
        },
        coord.clone(),
    )?;
    Ok(Device { engine, coord })
}

#[test]
fn against_the_coordinator_service() -> Result<()> {
    let Ok(url) = std::env::var("ONECLOUD_COORDINATOR_URL") else {
        eprintln!("ONECLOUD_COORDINATOR_URL not set; skipping");
        return Ok(());
    };
    let tmp = tempfile::tempdir()?;
    let repo = RepoSpec {
        repository: tmp.path().join("repo").to_string_lossy().into_owned(),
        options: BTreeMap::new(),
        signer: None,
    };
    let (ka, kb, kc) = (key(), key(), key());

    // a creates the account through the service
    let creator = HttpCoordinator::new(&url, None, ka.clone());
    let created = account::create(&creator, &repo, &ka, "a")?;
    let root = created.root.clone();
    let mut a = device(&url, &root, "a", &ka, &repo, &tmp.path().join("a"))?;
    fs::write(tmp.path().join("a/hello.txt"), "hi\n")?;
    a.engine.sync()?;

    // b asks to join; a approves; b syncs
    let b_coord = HttpCoordinator::new(&url, Some(root.clone()), kb.clone());
    assert_eq!(account::request(&b_coord, &kb, "b")?, root);
    let mut b = device(&url, &root, "b", &kb, &repo, &tmp.path().join("b"))?;
    assert!(b.engine.sync().is_err()); // not approved yet
    let req = a.engine.requests()?.pop().unwrap();
    a.engine.approve(&req)?;
    b.engine.sync()?;
    assert_eq!(fs::read_to_string(tmp.path().join("b/hello.txt"))?, "hi\n");

    // c joins with the recovery code, the client signing as the root
    let root_coord =
        HttpCoordinator::new(&url, Some(root.clone()), root_key(&created.recovery_code)?);
    account::join_with_recovery(&root_coord, &created.recovery_code, &kc, "c")?;
    let mut c = device(&url, &root, "c", &kc, &repo, &tmp.path().join("c"))?;
    c.engine.sync()?;

    // a push wakes a waiting device
    let latest = c.coord.head()?.map_or(0, |h| h.seq);
    let waiter = {
        let coord = c.coord.clone();
        thread::spawn(move || {
            let t = Instant::now();
            (coord.wait(latest), t.elapsed())
        })
    };
    thread::sleep(Duration::from_millis(300));
    fs::write(tmp.path().join("b/from-b.txt"), "b\n")?;
    b.engine.notice(["from-b.txt".into()]);
    b.engine.sync()?;
    let (woke, took) = waiter.join().unwrap();
    assert!(woke? > latest);
    assert!(took < Duration::from_secs(10), "wait took {took:?}");

    // a removes b and rotates: the head and key record land together
    a.engine.sync()?;
    a.engine.revoke(&public_hex(&kb))?;
    let rotated = a.engine.rotate()?;
    assert_eq!(rotated.epoch, 1);
    c.engine.sync()?;
    assert_eq!(c.engine.repository()?.2, 1);
    assert_eq!(fs::read_to_string(tmp.path().join("c/from-b.txt"))?, "b\n");

    // b is out: the service refuses its reads and a head it signs
    let err = b.engine.sync().unwrap_err();
    assert_eq!(err.downcast_ref::<Membership>(), Some(&Membership::Revoked));
    let latest = a.coord.head()?.unwrap();
    let forged = Head::next_in(&kb, Some(&latest), &latest.snapshot, 2, 1);
    let err = b.coord.append(&forged).unwrap_err();
    assert_eq!(err.downcast_ref::<Membership>(), Some(&Membership::Revoked));
    Ok(())
}

/// Storage through the service as well: the device never holds bucket
/// credentials, every object moves through presigned URLs.
#[test]
fn managed_storage_through_the_service() -> Result<()> {
    let (Ok(url), Ok(_)) = (
        std::env::var("ONECLOUD_COORDINATOR_URL"),
        std::env::var("ONECLOUD_SERVICE_STORAGE"),
    ) else {
        eprintln!("ONECLOUD_COORDINATOR_URL or ONECLOUD_SERVICE_STORAGE not set; skipping");
        return Ok(());
    };
    let tmp = tempfile::tempdir()?;
    let repo = RepoSpec::service(&url);
    let (ka, kb) = (key(), key());
    let creator = HttpCoordinator::new(&url, None, ka.clone());
    let created = account::create(&creator, &repo, &ka, "a")?;
    let root = created.root.clone();
    let mut a = device(&url, &root, "a", &ka, &repo, &tmp.path().join("a"))?;
    fs::create_dir_all(tmp.path().join("a/docs"))?;
    fs::write(tmp.path().join("a/docs/note.md"), "managed\n")?;
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
    fs::write(tmp.path().join("a/big.bin"), &big)?;
    a.engine.sync()?;

    // b joins by recovery code and pulls everything through presigned URLs
    let root_coord =
        HttpCoordinator::new(&url, Some(root.clone()), root_key(&created.recovery_code)?);
    account::join_with_recovery(&root_coord, &created.recovery_code, &kb, "b")?;
    let mut b = device(&url, &root, "b", &kb, &repo, &tmp.path().join("b"))?;
    b.engine.sync()?;
    assert_eq!(fs::read(tmp.path().join("b/big.bin"))?, big);

    // rotation writes the new epoch under its own prefix and deletes the old
    let rotated = a.engine.rotate()?;
    assert_eq!(rotated.epoch, 1);
    fs::write(tmp.path().join("a/after.txt"), "after rotation\n")?;
    a.engine.notice(["after.txt".into()]);
    a.engine.sync()?;
    b.engine.sync()?;
    assert_eq!(
        fs::read_to_string(tmp.path().join("b/after.txt"))?,
        "after rotation\n"
    );
    let v = b.engine.versions(Path::new("docs/note.md"))?;
    assert_eq!(v.len(), 1);
    Ok(())
}

/// How many requests a managed pull of many small files costs. Prints; run
/// with ONECLOUD_BENCH=1 through scripts/coordinator-e2e.sh.
#[test]
fn managed_pull_request_count() -> Result<()> {
    use std::sync::atomic::Ordering;
    let (Ok(url), Ok(_), Ok(_)) = (
        std::env::var("ONECLOUD_COORDINATOR_URL"),
        std::env::var("ONECLOUD_SERVICE_STORAGE"),
        std::env::var("ONECLOUD_BENCH")
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or(()),
    ) else {
        return Ok(());
    };
    let tmp = tempfile::tempdir()?;
    let repo = RepoSpec::service(&url);
    let (ka, kb) = (key(), key());
    let created = account::create(
        &HttpCoordinator::new(&url, None, ka.clone()),
        &repo,
        &ka,
        "a",
    )?;
    let root = created.root.clone();
    let mut a = device(&url, &root, "a", &ka, &repo, &tmp.path().join("a"))?;
    for i in 0..2000 {
        let dir = tmp.path().join(format!("a/d{:02}", i % 20));
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join(format!("f{i}.txt")),
            format!("file {i}\n").repeat(1 + i % 50),
        )?;
    }
    let count = || {
        (
            onecloud_core::storage::SIGN_REQUESTS.load(Ordering::Relaxed),
            onecloud_core::storage::OBJECT_REQUESTS.load(Ordering::Relaxed),
        )
    };
    let (s0, o0) = count();
    let t = Instant::now();
    a.engine.sync()?;
    let (s1, o1) = count();
    eprintln!(
        "push 2000 files: {:?}, {} sign requests, {} object requests",
        t.elapsed(),
        s1 - s0,
        o1 - o0
    );

    let root_coord =
        HttpCoordinator::new(&url, Some(root.clone()), root_key(&created.recovery_code)?);
    account::join_with_recovery(&root_coord, &created.recovery_code, &kb, "b")?;
    let mut b = device(&url, &root, "b", &kb, &repo, &tmp.path().join("b"))?;
    let t = Instant::now();
    b.engine.sync()?;
    let (s2, o2) = count();
    eprintln!(
        "pull 2000 files: {:?}, {} sign requests, {} object requests",
        t.elapsed(),
        s2 - s1,
        o2 - o1
    );
    Ok(())
}

/// The bucket takes exactly the size the service signed for: the quota
/// counts what is really stored.
#[test]
fn uploads_must_match_the_signed_size() -> Result<()> {
    let (Ok(url), Ok(_)) = (
        std::env::var("ONECLOUD_COORDINATOR_URL"),
        std::env::var("ONECLOUD_SERVICE_STORAGE"),
    ) else {
        return Ok(());
    };
    let tmp = tempfile::tempdir()?;
    let ka = key();
    let creator = HttpCoordinator::new(&url, None, ka.clone());
    let created = account::create(&creator, &RepoSpec::service(&url), &ka, "a")?;
    let client = HttpCoordinator::new(&url, Some(created.root), ka);
    let id = format!("ab{}", "c".repeat(62));
    let path = format!("data/ab/{id}");
    let put = |signed: u64, body: &[u8]| -> Result<u16> {
        let url = client.presign("PUT", &path, Some(signed))?;
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .new_agent();
        Ok(agent.put(&url).send(body)?.status().as_u16())
    };
    assert!(
        put(5, b"12345678").map_or(true, |s| s >= 400),
        "a larger upload got in"
    );
    assert_eq!(put(5, b"12345")?, 200);
    let (used, _) = client.usage()?;
    assert!(used >= 5);
    drop(tmp);
    Ok(())
}
