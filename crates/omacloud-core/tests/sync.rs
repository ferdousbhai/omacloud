use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, SystemTime},
};

use anyhow::Result;
use ed25519_dalek::SigningKey;
use omacloud_core::{
    Anchor, Coordinator, DirCoordinator, Engine, Head, HeadError, JoinRequest, Membership,
    RepoSpec, Setup, Stats, account,
    devices::DeviceEntry,
    epoch::{EpochRecord, Grant},
    fingerprint,
};
use tempfile::TempDir;

struct World {
    tmp: TempDir,
    repo: RepoSpec,
    coord: Arc<DirCoordinator>,
    keys: BTreeMap<String, SigningKey>,
    root: String,
    recovery_code: String,
}

impl World {
    fn new(devices: &[&str]) -> Result<Self> {
        Self::with_repo(devices, |tmp| RepoSpec {
            repository: tmp.join("repo").to_string_lossy().into_owned(),
            options: BTreeMap::new(),
        })
    }

    fn with_repo(devices: &[&str], repo: impl Fn(&Path) -> RepoSpec) -> Result<Self> {
        let tmp = tempfile::tempdir()?;
        let repo = repo(tmp.path());
        let coord = Arc::new(DirCoordinator::new(tmp.path().join("coord"))?);
        let keys: BTreeMap<String, SigningKey> = devices
            .iter()
            .map(|d| (d.to_string(), SigningKey::generate(&mut rand::rngs::OsRng)))
            .collect();
        // the first device creates the account, the others join with the
        // recovery code; names starting with "outsider" stay out
        let first = devices[0];
        let created = account::create(coord.as_ref(), &repo, &keys[first], first)?;
        for d in &devices[1..] {
            if !d.starts_with("outsider") {
                account::join_with_recovery(coord.as_ref(), &created.recovery_code, &keys[*d], d)?;
            }
        }
        Ok(Self {
            tmp,
            repo,
            coord,
            keys,
            root: created.root,
            recovery_code: created.recovery_code,
        })
    }

    fn setup(&self, device: &str, folder: PathBuf, state: Option<PathBuf>) -> Setup {
        Setup {
            device: device.to_string(),
            folder,
            repo: self.repo.clone(),
            signing: self.keys[device].clone(),
            root: self.root.clone(),
            state_path: state,
            settings: None,
            folders: None,
        }
    }

    fn public(&self, device: &str) -> String {
        hex::encode(self.keys[device].verifying_key().as_bytes())
    }

    fn dir(&self, device: &str) -> PathBuf {
        self.tmp.path().join(device)
    }

    /// A device of the account, with persistent state.
    fn engine(&self, device: &str) -> Result<Engine> {
        let dir = self.dir(device);
        fs::create_dir_all(&dir)?;
        let state = self.tmp.path().join(format!("{device}.state.json"));
        Engine::new(self.setup(device, dir, Some(state)), self.coord.clone())
    }
}

/// Every file (content) and symlink (target) under `dir`.
fn digest(dir: &Path) -> BTreeMap<PathBuf, String> {
    fn go(root: &Path, rel: &Path, out: &mut BTreeMap<PathBuf, String>) {
        for e in fs::read_dir(root.join(rel)).unwrap() {
            let e = e.unwrap();
            let path = rel.join(e.file_name());
            let ft = e.file_type().unwrap();
            if ft.is_symlink() {
                let t = fs::read_link(e.path()).unwrap();
                _ = out.insert(path, format!("-> {}", t.display()));
            } else if ft.is_dir() {
                go(root, &path, out);
            } else {
                _ = out.insert(path, fs::read_to_string(e.path()).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    go(dir, Path::new(""), &mut out);
    out
}

fn write(dir: &Path, rel: &str, body: &str) -> PathBuf {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, body).unwrap();
    PathBuf::from(rel)
}

fn set_mtime(dir: &Path, rel: &str, secs_ago: u64) {
    let f = fs::File::options().write(true).open(dir.join(rel)).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(secs_ago))
        .unwrap();
}

fn conflict_copies(d: &BTreeMap<PathBuf, String>) -> Vec<(&PathBuf, &String)> {
    d.iter()
        .filter(|(p, _)| p.to_string_lossy().contains(".sync-conflict-"))
        .collect()
}

#[test]
fn round_trip_edits_deletes_and_symlinks() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (mut a, mut b) = (w.engine("a")?, w.engine("b")?);
    let (da, db) = (w.dir("a"), w.dir("b"));

    write(&da, "notes.md", "one\n");
    write(&da, "deep/er/file.txt", "deep\n");
    symlink("notes.md", da.join("link"))?;
    a.sync()?;
    b.sync()?;
    assert_eq!(digest(&da), digest(&db));
    assert_eq!(digest(&db)[Path::new("link")], "-> notes.md");

    // edit, delete, retarget a link, all found through watcher paths
    let edited = write(&da, "notes.md", "two\n");
    fs::remove_file(da.join("deep/er/file.txt"))?;
    fs::remove_file(da.join("link"))?;
    symlink("deep", da.join("link"))?;
    a.notice([edited, "deep/er/file.txt".into(), "link".into()]);
    let st = a.sync()?;
    // the edit, the delete, the new link, and the folder the delete emptied
    assert_eq!(st.pushed, 4);
    b.sync()?;
    assert_eq!(digest(&da), digest(&db));
    // the file is gone on b; its folder, still on a (now empty), stays
    assert!(!db.join("deep/er/file.txt").exists());
    assert!(db.join("deep/er").is_dir());

    // nothing changed: nothing pushed, and b's pulled files don't echo back
    b.request_rescan();
    assert_eq!(b.sync()?, Stats::default());
    Ok(())
}

#[test]
fn conflicting_edits_keep_both_versions() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (mut a, mut b) = (w.engine("a")?, w.engine("b")?);
    let (da, db) = (w.dir("a"), w.dir("b"));
    let f = write(&da, "doc.txt", "base\n");
    a.sync()?;
    b.sync()?;

    // both edit offline; b's edit is newer
    write(&da, "doc.txt", "from a\n");
    set_mtime(&da, "doc.txt", 60);
    write(&db, "doc.txt", "from b\n");
    a.notice([f.clone()]);
    b.notice([f]);
    a.sync()?;
    let st = b.sync()?;
    assert_eq!(st.conflicts, 1);
    a.sync()?;

    let (x, y) = (digest(&da), digest(&db));
    assert_eq!(x, y);
    assert_eq!(x[Path::new("doc.txt")], "from b\n");
    let copies = conflict_copies(&x);
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].1, "from a\n");
    assert!(copies[0].0.to_string_lossy().ends_with("-a.txt"));
    Ok(())
}

#[test]
fn a_change_beats_a_delete() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (mut a, mut b) = (w.engine("a")?, w.engine("b")?);
    let (da, db) = (w.dir("a"), w.dir("b"));
    let f = write(&da, "keep.txt", "v1\n");
    a.sync()?;
    b.sync()?;

    fs::remove_file(da.join("keep.txt"))?;
    write(&db, "keep.txt", "v2\n");
    a.notice([f.clone()]);
    b.notice([f]);
    a.sync()?;
    b.sync()?;
    a.sync()?;
    assert_eq!(digest(&da)[Path::new("keep.txt")], "v2\n");
    assert_eq!(digest(&da), digest(&db));
    Ok(())
}

#[test]
fn file_and_directory_swap_places() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (mut a, mut b) = (w.engine("a")?, w.engine("b")?);
    let (da, db) = (w.dir("a"), w.dir("b"));
    write(&da, "x", "a file\n");
    write(&da, "y/inner.txt", "in a dir\n");
    a.sync()?;
    b.sync()?;

    fs::remove_file(da.join("x"))?;
    write(&da, "x/now-a-dir.txt", "hello\n");
    fs::remove_dir_all(da.join("y"))?;
    write(&da, "y", "now a file\n");
    a.notice(["x".into(), "y".into()]);
    a.sync()?;
    b.sync()?;
    assert_eq!(digest(&da), digest(&db));
    assert!(db.join("x").is_dir());
    assert!(db.join("y").is_file());
    Ok(())
}

#[test]
fn rescan_after_restart_finds_offline_changes() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let da = w.dir("a");
    write(&da, "one.txt", "1\n");
    write(&da, "two.txt", "2\n");
    w.engine("a")?.sync()?;

    // edits while no daemon was running
    write(&da, "one.txt", "1 changed\n");
    fs::remove_file(da.join("two.txt"))?;
    write(&da, "three.txt", "3\n");
    let st = w.engine("a")?.sync()?; // a fresh engine rescans on start
    assert_eq!(st.pushed, 3);

    w.engine("b")?.sync()?;
    assert_eq!(digest(&da), digest(&w.dir("b")));
    Ok(())
}

#[test]
fn refuses_rollback_even_after_restart() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let da = w.dir("a");
    let mut a = w.engine("a")?;
    for i in 0..2 {
        let f = write(&da, "f.txt", &format!("{i}\n"));
        a.notice([f]);
        a.sync()?;
    }
    w.engine("b")?.sync()?;

    // the coordinator drops the latest head
    let latest = w.coord.head()?.unwrap().seq;
    fs::remove_file(w.tmp.path().join(format!("coord/heads/{latest:020}.json")))?;

    // b remembers seq 2 in its state file, so a restarted b refuses
    let err = w.engine("b")?.sync().unwrap_err();
    assert_eq!(
        err.downcast_ref::<HeadError>(),
        Some(&HeadError::Rollback {
            known: 2,
            served: Some(1)
        })
    );
    Ok(())
}

#[test]
fn outsiders_cannot_sync_or_forge_heads() -> Result<()> {
    let w = World::new(&["a", "b", "outsider"])?;
    write(&w.dir("a"), "real.txt", "x\n");
    w.engine("a")?.sync()?;

    // not a member: the engine refuses to sync at all
    let err = w.engine("outsider")?.sync().unwrap_err();
    assert_eq!(
        err.downcast_ref::<Membership>(),
        Some(&Membership::NotApproved)
    );

    // a head it signs and slips in through the coordinator is refused
    let known = w.coord.head()?.unwrap();
    let forged = Head::next(&w.keys["outsider"], Some(&known), &known.snapshot, 2);
    assert!(w.coord.append(&forged)?);
    let err = w.engine("b")?.sync().unwrap_err();
    assert!(matches!(
        err.downcast_ref::<HeadError>(),
        Some(HeadError::UntrustedDevice { seq: 2, .. })
    ));
    Ok(())
}

#[test]
fn join_by_request_and_approval() -> Result<()> {
    let w = World::new(&["a", "outsider-c"])?;
    let mut a = w.engine("a")?;
    write(&w.dir("a"), "hello.txt", "hi\n");
    a.sync()?;

    // c asks to join; until approved it can't sync
    account::request(w.coord.as_ref(), &w.keys["outsider-c"], "c")?;
    let mut c = w.engine("outsider-c")?;
    assert!(c.sync().is_err());

    // a sees the request with c's fingerprint and approves
    let reqs = a.requests()?;
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        fingerprint(&reqs[0].device),
        fingerprint(&w.public("outsider-c"))
    );
    a.approve(&reqs[0])?;
    assert!(a.requests()?.is_empty());

    c.sync()?;
    assert_eq!(
        fs::read_to_string(w.dir("outsider-c").join("hello.txt"))?,
        "hi\n"
    );
    Ok(())
}

#[test]
fn revoked_devices_are_locked_out() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (mut a, mut b) = (w.engine("a")?, w.engine("b")?);
    write(&w.dir("a"), "f.txt", "1\n");
    a.sync()?;
    b.sync()?;
    let before = w.coord.head()?.unwrap(); // names device entry 2

    a.revoke(&w.public("b"))?;
    let err = b.sync().unwrap_err();
    assert_eq!(err.downcast_ref::<Membership>(), Some(&Membership::Revoked));

    // once a head names the revocation, b can't sign its way back in by
    // naming the old device list, nor with the current one
    write(&w.dir("a"), "f.txt", "2\n");
    a.notice(["f.txt".into()]);
    a.sync()?;
    let latest = w.coord.head()?.unwrap();
    assert_eq!(latest.devices, 3);
    for devices in [before.devices, latest.devices] {
        let coord = DirCoordinator::new(w.tmp.path().join(format!("fork-{devices}")))?;
        for e in w.coord.devices_since(0)? {
            coord.append_device(&e)?;
        }
        for h in w.coord.since(0)? {
            coord.append(&h)?;
        }
        let forged = Head::next(&w.keys["b"], Some(&latest), &latest.snapshot, devices);
        assert!(coord.append(&forged)?);
        let mut chain = omacloud_core::DeviceChain::new(&w.root);
        chain.advance(&coord)?;
        let err = omacloud_core::HeadTracker::default()
            .advance(&coord, &chain)
            .unwrap_err();
        let err = err.downcast_ref::<HeadError>().unwrap();
        assert!(
            matches!(
                err,
                HeadError::StaleDevices { .. } | HeadError::UntrustedDevice { .. }
            ),
            "{err:?}"
        );
    }

    // history from while b was a member stays valid
    assert!(!a.versions(Path::new("f.txt"))?.is_empty());
    Ok(())
}

#[test]
fn approval_refuses_a_device_shown_another_account() -> Result<()> {
    let w = World::new(&["a", "outsider"])?;
    let mut a = w.engine("a")?;
    a.sync()?;
    // the coordinator showed the joining device a root of its own making
    let fake_root = hex::encode(
        SigningKey::generate(&mut rand::rngs::OsRng)
            .verifying_key()
            .as_bytes(),
    );
    let req = JoinRequest::new(&w.keys["outsider"], "outsider", &fake_root);
    w.coord.request(&req)?;
    assert!(a.approve(&a.requests()?[0]).is_err());
    // and a request whose name or root was changed after signing
    let mut tampered = JoinRequest::new(&w.keys["outsider"], "outsider", &w.root);
    tampered.name = "trusted laptop".into();
    assert!(a.approve(&tampered).is_err());
    // another account's recovery code gets nowhere
    let other = World::new(&["x"])?;
    assert!(
        account::join_with_recovery(
            w.coord.as_ref(),
            &other.recovery_code,
            &w.keys["outsider"],
            "o"
        )
        .is_err()
    );
    Ok(())
}

/// A bucket with delete protection keeps the old epoch's packs until their
/// retention ends. The rotation still completes, and deleting them is
/// retried later.
#[test]
fn rotation_leaves_protected_files_for_later() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let w = World::new(&["a"])?;
    let mut a = w.engine("a")?;
    write(&w.dir("a"), "note.md", "kept\n");
    a.sync()?;
    // read-only pack directories stand in for a bucket lock
    let packs: Vec<PathBuf> = fs::read_dir(Path::new(&w.repo.repository).join("data"))?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    let mode = |p: &Path, m| fs::set_permissions(p, fs::Permissions::from_mode(m));
    for p in &packs {
        mode(p, 0o555)?;
    }
    let rotated = a.rotate()?;
    assert_eq!(rotated.epoch, 1);
    assert!(rotated.left > 0);
    assert!(Path::new(&w.repo.repository).join("config").exists());
    write(&w.dir("a"), "after.md", "after\n");
    a.notice(["after.md".into()]);
    a.sync()?; // not retried yet, not even by a new process: tried a moment ago
    drop(a);
    let mut a = w.engine("a")?;
    a.sync()?;
    assert!(Path::new(&w.repo.repository).exists());

    // retention ends, and a day passes; the next sync deletes what's left
    for p in &packs {
        mode(p, 0o755)?;
    }
    drop(a);
    let state = w.tmp.path().join("a.state.json");
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&state)?)?;
    json["cleaned_at"] = 0.into();
    fs::write(&state, serde_json::to_vec(&json)?)?;
    let mut a = w.engine("a")?;
    a.sync()?;
    assert!(
        !Path::new(&w.repo.repository).exists(),
        "old epoch left behind"
    );
    assert_eq!(a.repository()?.2, 1);
    Ok(())
}

#[test]
fn rotation_locks_out_removed_devices_and_keeps_history() -> Result<()> {
    let w = World::new(&["a", "b", "c", "outsider-d"])?;
    let (mut a, mut b, mut c) = (w.engine("a")?, w.engine("b")?, w.engine("c")?);
    let (da, db) = (w.dir("a"), w.dir("b"));
    let f = write(&da, "diary.md", "day 1\n");
    a.sync()?;
    write(&da, "diary.md", "day 2\n");
    a.notice([f.clone()]);
    a.sync()?;
    b.sync()?;
    c.sync()?;

    // b edits while a removes c and rotates; b hasn't synced since
    write(&db, "from-b.txt", "written during the rotation\n");
    a.revoke(&w.public("c"))?;
    let rotated = a.rotate()?;
    assert_eq!(rotated.epoch, 1);
    assert!(
        !Path::new(&w.repo.repository).exists(),
        "old repository left behind"
    );
    let record = w.coord.epochs()?.pop().unwrap();
    assert!(!record.keys.contains_key(&w.public("c")));
    assert!(record.keys.contains_key(&w.public("b")));

    // b moves to the new epoch on its own and its edit goes through
    b.notice(["from-b.txt".into()]);
    b.sync()?;
    assert_eq!(b.repository()?.2, 1);
    a.sync()?;
    assert_eq!(
        fs::read_to_string(da.join("from-b.txt"))?,
        "written during the rotation\n"
    );
    assert_eq!(digest(&da), digest(&db));

    // c is out, and holds no key for the new repository
    let err = c.sync().unwrap_err();
    assert_eq!(err.downcast_ref::<Membership>(), Some(&Membership::Revoked));

    // history from before the rotation still reads
    let v = a.versions(&f)?;
    assert_eq!(v.len(), 2);
    let out = w.tmp.path().join("day1.md");
    a.restore_version(&f, v[0].seq, &out)?;
    assert_eq!(fs::read_to_string(&out)?, "day 1\n");

    // a device approved after the rotation gets the new key
    account::request(w.coord.as_ref(), &w.keys["outsider-d"], "d")?;
    let req = a.requests()?.pop().unwrap();
    a.approve(&req)?;
    let mut d = w.engine("outsider-d")?;
    d.sync()?;
    assert_eq!(digest(&w.dir("outsider-d")), digest(&da));

    // and so does a device joining with the recovery code
    let e_key = SigningKey::generate(&mut rand::rngs::OsRng);
    account::join_with_recovery(w.coord.as_ref(), &w.recovery_code, &e_key, "e")?;
    let e_dir = w.tmp.path().join("e");
    fs::create_dir_all(&e_dir)?;
    let mut e = Engine::new(
        Setup {
            device: "e".into(),
            folder: e_dir.clone(),
            repo: w.repo.clone(),
            signing: e_key,
            root: w.root.clone(),
            state_path: None,
            settings: None,
            folders: None,
        },
        w.coord.clone(),
    )?;
    e.sync()?;
    assert_eq!(digest(&e_dir), digest(&da));
    Ok(())
}

/// Two devices edit and delete the same few files at once, syncing every few
/// edits. They must converge, and whatever was on disk when a sync started
/// must be in some snapshot (as the file or a conflict copy).
#[test]
fn concurrent_devices_converge_without_losing_writes() -> Result<()> {
    let _ = env_logger::builder().is_test(true).try_init();
    let w = World::new(&["x", "y"])?;
    let mut engines = vec![w.engine("x")?, w.engine("y")?];
    let written: BTreeSet<String> = thread::scope(|s| {
        let handles: Vec<_> = engines
            .iter_mut()
            .enumerate()
            .map(|(i, e)| {
                let name = ["x", "y"][i];
                s.spawn(move || -> Result<Vec<String>> {
                    let mut rng: u64 = 0x9e37_79b9_7f4a_7c15 ^ (i as u64 + 1);
                    let mut next = |m: u64| {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        rng % m
                    };
                    let dir = e.folder().to_path_buf();
                    let (mut written, mut on_disk, mut changed) =
                        (Vec::new(), BTreeMap::new(), Vec::new());
                    for round in 0..120 {
                        let n = next(6);
                        let rel = format!("d{}/f{}.txt", n % 2, n / 2);
                        if next(100) < 80 {
                            let body = format!("{name} {round} {rel}\n");
                            write(&dir, &rel, &body);
                            _ = on_disk.insert(rel.clone(), body);
                        } else {
                            _ = fs::remove_file(dir.join(&rel));
                            _ = on_disk.remove(&rel);
                        }
                        changed.push(PathBuf::from(rel));
                        if round % 5 == 4 {
                            written.extend(std::mem::take(&mut on_disk).into_values());
                            e.notice(std::mem::take(&mut changed));
                            e.sync()?;
                        }
                    }
                    written.extend(on_disk.into_values());
                    e.notice(changed);
                    e.sync()?;
                    Ok(written)
                })
            })
            .collect();
        let mut all = BTreeSet::new();
        for h in handles {
            all.extend(h.join().unwrap()?);
        }
        Ok::<_, anyhow::Error>(all)
    })?;

    for _ in 0..2 {
        for e in &mut engines {
            e.sync()?;
        }
    }
    let (x, y) = (digest(&w.dir("x")), digest(&w.dir("y")));
    assert_eq!(x, y, "devices did not converge");

    // every write is in the final folder or in history
    let mut seen: BTreeSet<String> = x.values().cloned().collect();
    seen.extend(all_snapshot_contents(&w)?);
    let lost: Vec<_> = written.difference(&seen).collect();
    assert!(lost.is_empty(), "lost writes: {lost:?}");
    Ok(())
}

/// Contents of every file at every head: a fresh device syncs to each head
/// in turn through a coordinator that stops there.
fn all_snapshot_contents(w: &World) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    let heads = w.coord.since(0)?;
    for n in 1..=heads.len() {
        // a coordinator that stops at head n, to materialize that version
        let prefix = Arc::new(Prefix(heads[..n].to_vec(), w.coord.clone()));
        let dir = w.tmp.path().join(format!("replay-{n}"));
        fs::create_dir_all(&dir)?;
        // a member's key: the replay only pulls, it never pushes
        let member = w.keys.keys().next().unwrap().clone();
        let mut e = Engine::new(w.setup(&member, dir.clone(), None), prefix)?;
        e.sync()?;
        out.extend(digest(&dir).into_values());
        fs::remove_dir_all(&dir)?;
    }
    Ok(out)
}

/// The first heads of a real coordinator; everything else passes through.
struct Prefix(Vec<Head>, Arc<DirCoordinator>);

impl Coordinator for Prefix {
    fn head(&self) -> Result<Option<Head>> {
        Ok(self.0.last().cloned())
    }
    fn since(&self, after: u64) -> Result<Vec<Head>> {
        Ok(self.0.iter().filter(|h| h.seq > after).cloned().collect())
    }
    fn append(&self, _: &Head) -> Result<bool> {
        Ok(false)
    }
    fn device_head(&self) -> Result<Option<DeviceEntry>> {
        self.1.device_head()
    }
    fn devices_since(&self, after: u64) -> Result<Vec<DeviceEntry>> {
        self.1.devices_since(after)
    }
    fn append_device(&self, e: &DeviceEntry) -> Result<bool> {
        self.1.append_device(e)
    }
    fn anchor(&self) -> Result<Option<Anchor>> {
        self.1.anchor()
    }
    fn set_anchor(&self, a: &Anchor) -> Result<bool> {
        self.1.set_anchor(a)
    }
    fn epochs(&self) -> Result<Vec<EpochRecord>> {
        self.1.epochs()
    }
    fn append_epoch(&self, r: &EpochRecord) -> Result<bool> {
        self.1.append_epoch(r)
    }
    fn grants(&self, device: &str) -> Result<Vec<Grant>> {
        self.1.grants(device)
    }
    fn put_grant(&self, g: &Grant) -> Result<()> {
        self.1.put_grant(g)
    }
    fn requests(&self) -> Result<Vec<JoinRequest>> {
        self.1.requests()
    }
    fn request(&self, r: &JoinRequest) -> Result<()> {
        self.1.request(r)
    }
    fn remove_request(&self, d: &str) -> Result<()> {
        self.1.remove_request(d)
    }
}

#[test]
fn versions_and_point_in_time_restore() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (mut a, mut b) = (w.engine("a")?, w.engine("b")?);
    let (da, db) = (w.dir("a"), w.dir("b"));
    let f = write(&da, "essay.md", "draft 1\n");
    a.sync()?;
    write(&da, "other.txt", "unrelated\n"); // a head that doesn't touch essay.md
    a.notice(["other.txt".into()]);
    a.sync()?;
    b.sync()?;
    write(&db, "essay.md", "draft 2 from b\n");
    b.notice([f.clone()]);
    b.sync()?;
    a.sync()?;
    fs::remove_file(da.join("essay.md"))?;
    a.notice([f.clone()]);
    a.sync()?;

    let v = a.versions(&f)?;
    let summary: Vec<_> = v
        .iter()
        .map(|v| (v.seq, v.device.as_str(), v.size))
        .collect();
    assert_eq!(
        summary,
        [(1, "a", Some(8)), (3, "b", Some(15)), (4, "a", None)]
    );

    // the deleted file's first draft comes back from head 1
    let out = w.tmp.path().join("restored/essay.md");
    a.restore_version(&f, 1, &out)?;
    assert_eq!(fs::read_to_string(&out)?, "draft 1\n");
    assert!(a.restore_version(&f, 4, &out).is_err()); // deleted at head 4
    Ok(())
}

#[test]
fn ignore_rules_hold_in_both_directions() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (mut a, mut b) = (w.engine("a")?, w.engine("b")?);
    let (da, db) = (w.dir("a"), w.dir("b"));
    write(&da, "notes.md", "synced\n");
    write(&da, "old.log", "synced before the rule\n");
    a.sync()?;
    b.sync()?;

    // the rules file itself syncs, so both devices follow it
    write(&da, ".omacloudignore", "*.log\nbuild/\n");
    write(&da, "build/out.o", "binary\n");
    write(&da, "run.log", "noise\n");
    write(&da, ".notes.md.swp", "vim swap\n"); // a built-in default
    a.request_rescan();
    a.sync()?;
    b.sync()?;
    assert!(db.join(".omacloudignore").exists());
    assert!(!db.join("build").exists());
    assert!(!db.join("run.log").exists());
    assert!(!db.join(".notes.md.swp").exists());

    // ignored on b too: its own log doesn't travel back
    write(&db, "b.log", "local only\n");
    b.request_rescan();
    b.sync()?;
    a.sync()?;
    assert!(!da.join("b.log").exists());

    // a file synced before the rule stays where it is on both devices
    assert!(da.join("old.log").exists() && db.join("old.log").exists());
    // with its user rule, a device can keep more to itself
    b.set_ignore_rules(vec!["private/".into()]);
    write(&db, "private/diary.txt", "mine\n");
    b.request_rescan();
    b.sync()?;
    a.sync()?;
    assert!(!da.join("private").exists());
    Ok(())
}

#[test]
fn the_same_edit_on_both_sides_is_no_conflict() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (mut a, mut b) = (w.engine("a")?, w.engine("b")?);
    let (da, db) = (w.dir("a"), w.dir("b"));
    let f = write(&da, "todo.txt", "old\n");
    a.sync()?;
    b.sync()?;
    write(&da, "todo.txt", "same fix\n");
    write(&db, "todo.txt", "same fix\n");
    a.notice([f.clone()]);
    b.notice([f]);
    a.sync()?;
    let st = b.sync()?;
    assert_eq!(st.conflicts, 0);
    assert_eq!(st.pushed, 0); // b adopted a's version, nothing to send back
    a.sync()?;
    assert!(conflict_copies(&digest(&db)).is_empty());
    assert_eq!(digest(&da), digest(&db));
    Ok(())
}

/// File timestamps are coarse: two devices can write the same file in the
/// same tick, with the same size. Neither version may be lost.
#[test]
fn same_size_same_mtime_edits_keep_both() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (mut a, mut b) = (w.engine("a")?, w.engine("b")?);
    let (da, db) = (w.dir("a"), w.dir("b"));
    let f = write(&da, "clock.txt", "base\n");
    a.sync()?;
    b.sync()?;

    let when = SystemTime::now();
    for (dir, body) in [(&da, "from a\n"), (&db, "from b\n")] {
        write(dir, "clock.txt", body);
        fs::File::options()
            .write(true)
            .open(dir.join("clock.txt"))?
            .set_modified(when)?;
    }
    a.notice([f.clone()]);
    b.notice([f]);
    a.sync()?;
    b.sync()?; // b's version wins the tie ("b" > "a") and must still be pushed
    a.sync()?;

    let (x, y) = (digest(&da), digest(&db));
    assert_eq!(x, y);
    let mut bodies: Vec<_> = x.values().cloned().collect();
    bodies.sort();
    assert_eq!(bodies, ["from a\n", "from b\n"]);
    Ok(())
}

#[test]
fn empty_directories_sync() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (mut a, mut b) = (w.engine("a")?, w.engine("b")?);
    let (da, db) = (w.dir("a"), w.dir("b"));
    fs::create_dir_all(da.join("projects/new/empty"))?;
    fs::create_dir_all(da.join("inbox"))?;
    a.sync()?;
    b.sync()?;
    assert!(db.join("projects/new/empty").is_dir());
    assert!(db.join("inbox").is_dir());

    // a file comes and goes; the folder it was in stays
    let f = write(&da, "inbox/x.txt", "x\n");
    a.notice([f.clone()]);
    a.sync()?;
    b.sync()?;
    assert!(db.join("inbox/x.txt").is_file());
    fs::remove_file(da.join(&f))?;
    a.notice([f]);
    a.sync()?;
    b.sync()?;
    assert!(db.join("inbox").is_dir(), "the emptied folder should stay");
    assert!(!db.join("inbox/x.txt").exists());

    // removing the folder removes it on the other side
    fs::remove_dir(da.join("inbox"))?;
    a.notice(["inbox".into()]);
    a.sync()?;
    b.sync()?;
    assert!(!db.join("inbox").exists());

    // both make the same empty folder: no conflict
    fs::create_dir_all(da.join("shared"))?;
    fs::create_dir_all(db.join("shared"))?;
    a.notice(["shared".into()]);
    b.notice(["shared".into()]);
    a.sync()?;
    let st = b.sync()?;
    assert_eq!(st.conflicts, 0);
    a.sync()?;
    assert!(fs::read_dir(&db)?.all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .contains("sync-conflict")
    }));

    // nothing echoes back after a restart
    b.request_rescan();
    assert_eq!(b.sync()?.pushed, 0);
    let mut a2 = w.engine("a")?;
    assert_eq!(a2.sync()?.pushed, 0);
    Ok(())
}

impl World {
    /// A device that also syncs settings from `<device>-home`.
    fn engine_with_settings(&self, device: &str) -> Result<Engine> {
        let dir = self.dir(device);
        fs::create_dir_all(&dir)?;
        let home = self.home(device);
        fs::create_dir_all(&home)?;
        let mut setup = self.setup(
            device,
            dir,
            Some(self.tmp.path().join(format!("{device}.state.json"))),
        );
        setup.settings = Some(omacloud_core::SettingsSetup {
            home,
            manifest: omacloud_core::settings::Manifest::bundled(),
            backups: self.tmp.path().join(format!("{device}-backups")),
            packages: self.tmp.path().join(format!("{device}-packages")),
            secrets: self.tmp.path().join(format!("{device}-secrets")),
            merged: self.tmp.path().join(format!("{device}-merged")),
            groups: Vec::new(),
        });
        Engine::new(setup, self.coord.clone())
    }

    fn home(&self, device: &str) -> PathBuf {
        self.tmp.path().join(format!("{device}-home"))
    }
}

#[test]
fn settings_follow_the_dots_manifest_and_merge() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (ha, hb) = (w.home("a"), w.home("b"));
    let bindings = ".config/hypr/bindings.lua";
    write(&ha, bindings, "bind 1\nbind 2\nbind 3\nbind 4\n");
    write(&ha, ".config/hypr/monitors.lua", "a's monitors\n"); // local tier
    write(&ha, ".ssh/id_ed25519", "secret\n"); // not in the manifest
    write(&hb, ".config/hypr/monitors.lua", "b's monitors\n");
    let (mut a, mut b) = (w.engine_with_settings("a")?, w.engine_with_settings("b")?);
    a.sync()?;
    b.sync()?;
    assert_eq!(
        fs::read_to_string(hb.join(bindings))?,
        "bind 1\nbind 2\nbind 3\nbind 4\n"
    );
    assert_eq!(
        fs::read_to_string(hb.join(".config/hypr/monitors.lua"))?,
        "b's monitors\n"
    );
    assert!(!hb.join(".ssh").exists());
    // settings don't show up in the synced folder
    assert!(!w.dir("b").join(".omacloud").exists());

    // separate edits merge on both devices
    write(&ha, bindings, "bind 1 (a)\nbind 2\nbind 3\nbind 4\n");
    write(&hb, bindings, "bind 1\nbind 2\nbind 3\nbind 4 (b)\n");
    a.notice([ha.join(bindings)]);
    b.notice([hb.join(bindings)]);
    a.sync()?;
    b.sync()?; // merges a's change into its own and pushes
    a.sync()?;
    let merged = "bind 1 (a)\nbind 2\nbind 3\nbind 4 (b)\n";
    assert_eq!(fs::read_to_string(ha.join(bindings))?, merged);
    assert_eq!(fs::read_to_string(hb.join(bindings))?, merged);
    // a's copy from before the merge landed in its history
    assert!(fs::read_dir(w.tmp.path().join("a-backups/history"))?.count() > 0);

    // overlapping edits: b keeps its version and holds a's until resolved
    write(
        &ha,
        bindings,
        "bind 1 (a again)\nbind 2\nbind 3\nbind 4 (b)\n",
    );
    write(
        &hb,
        bindings,
        "bind 1 (b too)\nbind 2\nbind 3\nbind 4 (b)\n",
    );
    a.notice([ha.join(bindings)]);
    b.notice([hb.join(bindings)]);
    a.sync()?;
    let st = b.sync()?;
    assert_eq!(st.conflicts, 1);
    assert_eq!(
        fs::read_to_string(hb.join(bindings))?,
        "bind 1 (b too)\nbind 2\nbind 3\nbind 4 (b)\n"
    );
    assert_eq!(b.settings_status().held, [PathBuf::from(bindings)]);
    a.sync()?;
    // held back: a keeps its own version meanwhile
    assert_eq!(
        fs::read_to_string(ha.join(bindings))?,
        "bind 1 (a again)\nbind 2\nbind 3\nbind 4 (b)\n"
    );
    // b takes a's version; both agree again
    b.resolve_setting(Path::new(bindings), true)?;
    b.sync()?;
    a.sync()?;
    assert!(b.settings_status().held.is_empty());
    assert_eq!(
        fs::read_to_string(hb.join(bindings))?,
        fs::read_to_string(ha.join(bindings))?
    );

    // a delete travels, after a backup
    fs::remove_file(ha.join(bindings))?;
    a.notice([ha.join(bindings)]);
    a.sync()?;
    b.sync()?;
    assert!(!hb.join(bindings).exists());

    // history covers settings too
    assert!(
        a.versions(&Path::new(".omacloud/settings").join(bindings))?
            .len()
            >= 3
    );
    Ok(())
}

#[test]
fn settings_stand_down_next_to_a_dotfile_manager() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (ha, hb) = (w.home("a"), w.home("b"));
    write(&ha, ".bashrc", "from a\n");
    // b manages its dotfiles with links: omacloud leaves them alone
    fs::create_dir_all(&hb)?;
    write(&hb, "dotfiles/bashrc", "b's own\n");
    std::os::unix::fs::symlink(hb.join("dotfiles/bashrc"), hb.join(".bashrc"))?;
    let (mut a, mut b) = (w.engine_with_settings("a")?, w.engine_with_settings("b")?);
    a.sync()?;
    b.sync()?;
    assert!(b.settings_status().dormant.is_some());
    assert_eq!(fs::read_to_string(hb.join(".bashrc"))?, "b's own\n");
    assert!(
        fs::symlink_metadata(hb.join(".bashrc"))?
            .file_type()
            .is_symlink()
    );
    Ok(())
}

#[test]
fn package_lists_travel_between_devices() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (mut a, mut b) = (w.engine_with_settings("a")?, w.engine_with_settings("b")?);
    assert!(a.update_package_list("[repo]\nhelix\nripgrep\n")?);
    assert!(!a.update_package_list("[repo]\nhelix\nripgrep\n")?); // unchanged
    a.sync()?;
    b.update_package_list("[repo]\nneovim\n")?;
    b.sync()?;
    a.sync()?;
    let lists = |d: &str| -> Result<Vec<String>> {
        let mut v: Vec<String> = fs::read_dir(w.tmp.path().join(format!("{d}-packages")))?
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        Ok(v)
    };
    assert_eq!(lists("a")?, ["a.txt", "b.txt"]);
    assert_eq!(lists("b")?, ["a.txt", "b.txt"]);
    assert_eq!(
        fs::read_to_string(w.tmp.path().join("b-packages/a.txt"))?,
        "[repo]\nhelix\nripgrep\n"
    );
    // nothing lands in the synced folders
    assert!(!w.dir("a").join(".omacloud").exists());
    // a restart finds nothing to push
    let mut a2 = w.engine_with_settings("a")?;
    assert_eq!(a2.sync()?.pushed, 0);
    Ok(())
}

#[test]
fn secrets_travel_sealed_and_open_only_with_the_recovery_code() -> Result<()> {
    use omacloud_core::secrets;
    let w = World::new(&["a", "b"])?;
    let (mut a, mut b) = (w.engine_with_settings("a")?, w.engine_with_settings("b")?);
    let ha = w.home("a");
    fs::create_dir_all(ha.join(".ssh"))?;
    fs::write(ha.join(".ssh/id_ed25519"), "a's key\n")?;
    let entries = secrets::collect(&ha, &secrets::paths(None)?)?;
    a.put_secrets(&secrets::seal(&w.root, &entries)?)?;
    a.sync()?;
    b.sync()?;
    assert_eq!(b.secrets_devices(), ["a"]);

    // the file syncs, sealed: it holds no key in the clear, and neither
    // device's own key opens it
    let sealed = b.secrets_of("a")?;
    assert!(!sealed.windows(8).any(|w| w == b"a's key\n"));
    assert!(secrets::open(&w.keys["b"], &sealed).is_err());
    let root = omacloud_core::devices::root_key(&w.recovery_code)?;
    let hb = w.home("b");
    let opened = secrets::open(&root, &sealed)?;
    secrets::restore(&hb, &opened, &w.tmp.path().join("b-secret-backups"))?;
    assert_eq!(fs::read_to_string(hb.join(".ssh/id_ed25519"))?, "a's key\n");

    // nothing lands in the synced folder, and a restart has nothing to push
    assert!(!w.dir("b").join(".omacloud").exists());
    assert!(!w.dir("a").join(".ssh").exists());
    assert_eq!(w.engine_with_settings("a")?.sync()?.pushed, 0);
    Ok(())
}

/// An own bucket: its location and credentials travel sealed in the epoch
/// secret, so a joining device needs neither typed in, and whoever relays
/// the records never sees them.
#[test]
fn own_bucket_location_and_credentials_travel_with_the_key() -> Result<()> {
    let bucket = |tmp: &Path| RepoSpec {
        repository: "opendal:fs".into(),
        options: [
            (
                "root".to_string(),
                tmp.join("bucket").to_string_lossy().into_owned(),
            ),
            ("access_key_id".to_string(), "not-needed-by-fs".to_string()),
        ]
        .into(),
    };
    let w = World::with_repo(&["a"], bucket)?;
    let mut a = w.engine("a")?;
    write(&w.dir("a"), "note.md", "in my bucket\n");
    a.sync()?;

    // b joins knowing nothing about the bucket
    let kb = SigningKey::generate(&mut rand::rngs::OsRng);
    account::join_with_recovery(w.coord.as_ref(), &w.recovery_code, &kb, "b")?;
    let db = w.tmp.path().join("b");
    fs::create_dir_all(&db)?;
    let mut b = Engine::new(
        Setup {
            device: "b".into(),
            folder: db.clone(),
            repo: RepoSpec {
                repository: w.tmp.path().join("nowhere").to_string_lossy().into_owned(),
                options: [("connections".to_string(), "2".to_string())].into(),
            },
            signing: kb,
            root: w.root.clone(),
            state_path: None,
            settings: None,
            folders: None,
        },
        w.coord.clone(),
    )?;
    b.sync()?;
    assert_eq!(fs::read_to_string(db.join("note.md"))?, "in my bucket\n");
    let (spec, secret, _) = b.repository()?;
    assert_eq!(
        spec.options.get("access_key_id").map(String::as_str),
        Some("not-needed-by-fs")
    );
    assert_eq!(
        spec.options.get("connections").map(String::as_str),
        Some("2")
    );
    assert!(!w.tmp.path().join("nowhere").exists());
    // the location isn't this device's limit to share
    assert!(!secret.storage.unwrap().options.contains_key("connections"));

    // a rotation keeps the bucket
    a.sync()?;
    assert_eq!(a.rotate()?.epoch, 1);
    b.sync()?;
    assert!(b.repository()?.0.options["root"].ends_with("omacloud-e1"));
    Ok(())
}

/// A bandwidth cap holds for many small files, not only for packs.
#[test]
fn bandwidth_cap_holds() -> Result<()> {
    let w = World::new(&["a"])?;
    let dir = w.dir("a");
    fs::create_dir_all(&dir)?;
    let mut setup = w.setup("a", dir.clone(), None);
    _ = setup
        .repo
        .options
        .insert("bandwidth".into(), "200KB".into());
    let mut a = Engine::new(setup, w.coord.clone())?;
    // incompressible, so the upload is really about this size
    let mut seed = 1u64;
    for i in 0..60 {
        let data: Vec<u8> = (0..10_000)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (seed >> 56) as u8
            })
            .collect();
        fs::write(dir.join(format!("f{i}.bin")), data)?;
    }
    let t = std::time::Instant::now();
    a.sync()?;
    let took = t.elapsed();
    let moved =
        omacloud_core::throttle::THROTTLED_READ_WRITE.load(std::sync::atomic::Ordering::Relaxed);
    assert!(moved >= 600_000, "only {moved} bytes went through the cap");
    // 600 KB at 200 KB/s, less a second of credit
    assert!(
        took >= std::time::Duration::from_millis(1800),
        "took {took:?}"
    );
    Ok(())
}

impl World {
    /// A device syncing chosen folders of its home (`<device>-home`).
    fn engine_home(&self, device: &str, add: &[&str], skip: &[&str]) -> Result<Engine> {
        self.engine_home_named(device, add, skip, &[])
    }

    fn engine_home_named(
        &self,
        device: &str,
        add: &[&str],
        skip: &[&str],
        names: &[(&str, &str)],
    ) -> Result<Engine> {
        let home = self.home(device);
        fs::create_dir_all(&home)?;
        let mut setup = self.setup(
            device,
            home,
            Some(self.tmp.path().join(format!("{device}.state.json"))),
        );
        setup.folders = Some(omacloud_core::Folders {
            add: add.iter().map(|s| (*s).to_string()).collect(),
            skip: skip.iter().map(|s| (*s).to_string()).collect(),
            local_names: names
                .iter()
                .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
                .collect(),
        });
        Engine::new(setup, self.coord.clone())
    }
}

/// As iCloud syncs Desktop and Documents: chosen folders of home sync in
/// place, the rest of home doesn't, a device can skip a folder, and a folder
/// one device adds reaches the others.
#[test]
fn chosen_home_folders_sync_in_place() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (ha, hb) = (w.home("a"), w.home("b"));
    for d in ["Documents", "Pictures", "Music", ".cache"] {
        fs::create_dir_all(ha.join(d))?;
    }
    write(&ha, "Documents/cv.md", "cv\n");
    write(&ha, "Pictures/cat.jpg", "meow");
    write(&ha, "Music/song.flac", "la");
    write(&ha, ".cache/junk", "x");
    write(&ha, "loose.txt", "not in a folder");
    let mut a = w.engine_home("a", &["Documents", "Pictures"], &[])?;
    a.sync()?;

    // b has no such folders yet and skips Pictures
    let mut b = w.engine_home("b", &["Documents"], &["Pictures"])?;
    b.sync()?;
    assert_eq!(fs::read_to_string(hb.join("Documents/cv.md"))?, "cv\n");
    assert!(!hb.join("Pictures").exists());
    assert!(!hb.join("Music").exists() && !hb.join(".cache").exists());
    assert!(!hb.join("loose.txt").exists());

    // a adds Music; b follows without being told
    drop(a);
    let mut a = w.engine_home("a", &["Documents", "Pictures", "Music"], &[])?;
    a.sync()?;
    b.sync()?;
    assert_eq!(fs::read_to_string(hb.join("Music/song.flac"))?, "la");

    // b edits in place; a gets it
    write(&hb, "Documents/cv.md", "cv v2\n");
    b.notice([hb.join("Documents/cv.md")]);
    b.sync()?;
    a.sync()?;
    assert_eq!(fs::read_to_string(ha.join("Documents/cv.md"))?, "cv v2\n");

    // emptying a folder deletes its files, not the folder, anywhere
    fs::remove_file(hb.join("Music/song.flac"))?;
    b.notice([hb.join("Music/song.flac")]);
    b.sync()?;
    a.sync()?;
    assert!(!ha.join("Music/song.flac").exists());
    assert!(ha.join("Music").is_dir() && hb.join("Music").is_dir());
    Ok(())
}

/// A folder is matched by what it is, not what it's called: a German
/// machine's `Dokumente` is the English machine's `Documents`.
#[test]
fn folders_match_across_languages() -> Result<()> {
    let w = World::new(&["de", "en"])?;
    let (hde, hen) = (w.home("de"), w.home("en"));
    fs::create_dir_all(hde.join("Dokumente"))?;
    write(&hde, "Dokumente/brief.md", "hallo\n");
    let mut de = w.engine_home_named("de", &["Documents"], &[], &[("Documents", "Dokumente")])?;
    de.sync()?;
    let mut en = w.engine_home("en", &["Documents"], &[])?;
    en.sync()?;
    assert_eq!(
        fs::read_to_string(hen.join("Documents/brief.md"))?,
        "hallo\n"
    );
    assert!(!hen.join("Dokumente").exists());

    // an edit on the English side lands in Dokumente, and a watcher's
    // absolute path there maps back
    write(&hen, "Documents/brief.md", "hello\n");
    en.notice([hen.join("Documents/brief.md")]);
    en.sync()?;
    de.sync()?;
    assert_eq!(
        fs::read_to_string(hde.join("Dokumente/brief.md"))?,
        "hello\n"
    );
    write(&hde, "Dokumente/neu.md", "neu\n");
    de.notice([hde.join("Dokumente/neu.md")]);
    de.sync()?;
    en.sync()?;
    assert_eq!(fs::read_to_string(hen.join("Documents/neu.md"))?, "neu\n");
    assert!(!hde.join("Documents").exists());
    Ok(())
}

/// A first pull cut off before it recorded anything starts over, and finds
/// the files it already wrote: no conflict copies, nothing changed.
#[test]
fn interrupted_first_pull_resumes() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (da, db) = (w.dir("a"), w.dir("b"));
    let mut a = w.engine("a")?;
    for i in 0..20 {
        write(&da, &format!("doc{i}.md"), &format!("doc {i}\n"));
    }
    fs::write(da.join("big.bin"), vec![7u8; 3 << 20])?;
    a.sync()?;
    let mut b = w.engine("b")?;
    b.sync()?;
    drop(b);
    // as if the pull was cut off before its state was saved
    fs::remove_file(w.tmp.path().join("b.state.json"))?;
    let mut b = w.engine("b")?;
    let st = b.sync()?;
    assert_eq!(st.conflicts, 0);
    assert_eq!(digest(&da), digest(&db));
    assert!(!fs::read_dir(&db)?.any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .contains("sync-conflict")
    }));
    Ok(())
}

/// A save that lands during a pull, after the cycle looked for local
/// changes, is neither overwritten nor deleted: it's kept and pushed.
#[test]
fn a_save_during_a_pull_is_kept() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (mut a, mut b) = (w.engine("a")?, w.engine("b")?);
    let (da, db) = (w.dir("a"), w.dir("b"));
    write(&da, "doc.txt", "base\n");
    write(&da, "gone.txt", "base\n");
    a.sync()?;
    b.sync()?;

    let f = write(&da, "doc.txt", "from a\n");
    fs::remove_file(da.join("gone.txt"))?;
    a.notice([f, "gone.txt".into()]);
    a.sync()?;
    // b saves both, and its watcher hasn't told it yet
    write(&db, "doc.txt", "saved on b\n");
    write(&db, "gone.txt", "saved on b\n");
    let st = b.sync()?;
    assert_eq!(st.conflicts, 1);
    a.sync()?;

    let (x, y) = (digest(&da), digest(&db));
    assert_eq!(x, y);
    assert_eq!(x[Path::new("doc.txt")], "from a\n");
    assert_eq!(x[Path::new("gone.txt")], "saved on b\n");
    let copies = conflict_copies(&x);
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].1, "saved on b\n");
    Ok(())
}

/// A file put here some other way (copied over before syncing), the same as
/// the one another computer pushes, is no conflict: no copy, and it takes
/// the other computer's time.
#[test]
fn the_same_file_already_here_is_no_conflict() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let w = World::new(&["a", "b"])?;
    let (mut a, mut b) = (w.engine("a")?, w.engine("b")?);
    let (da, db) = (w.dir("a"), w.dir("b"));
    write(&da, "seed.txt", "seed\n");
    a.sync()?;
    b.sync()?;

    // b gets the photo by hand first, with its own time, unseen by its watcher
    write(&db, "photo.jpg", "same bytes\n");
    set_mtime(&db, "photo.jpg", 86400);
    // and made read-only, which mustn't stop the pull
    fs::set_permissions(db.join("photo.jpg"), fs::Permissions::from_mode(0o444))?;
    let f = write(&da, "photo.jpg", "same bytes\n");
    a.notice([f]);
    a.sync()?;
    let st = b.sync()?;
    assert_eq!(st.conflicts, 0);
    a.sync()?;

    let (x, y) = (digest(&da), digest(&db));
    assert_eq!(x, y);
    assert!(conflict_copies(&y).is_empty());
    assert_eq!(
        fs::metadata(da.join("photo.jpg"))?.modified()?,
        fs::metadata(db.join("photo.jpg"))?.modified()?
    );
    Ok(())
}

/// A named pipe where another computer has a file doesn't stall the pull:
/// opening it to compare would wait for a writer forever.
#[test]
fn a_named_pipe_in_the_way_doesnt_stall_a_pull() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (mut a, mut b) = (w.engine("a")?, w.engine("b")?);
    let (da, db) = (w.dir("a"), w.dir("b"));
    write(&da, "seed.txt", "seed\n");
    a.sync()?;
    b.sync()?;

    let pipe = std::ffi::CString::new(db.join("pipe").into_os_string().into_encoded_bytes())?;
    assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o644) }, 0);
    let f = write(&da, "pipe", "a file on a\n");
    a.notice([f]);
    a.sync()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || _ = tx.send(b.sync().map(|st| st.conflicts)));
    let done = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .map_err(|_| anyhow::anyhow!("the pull stalled on the pipe"))?;
    // neither stalled nor failed: the pipe is kept, as a change beats a pull
    assert_eq!(done?, 1);
    Ok(())
}

/// The same for a setting: no conflict copy in home, the other version is
/// held for `resolve` instead.
#[test]
fn a_setting_saved_during_a_pull_is_kept() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (ha, hb) = (w.home("a"), w.home("b"));
    let bindings = ".config/hypr/bindings.lua";
    write(&ha, bindings, "bind 1\n");
    let (mut a, mut b) = (w.engine_with_settings("a")?, w.engine_with_settings("b")?);
    a.sync()?;
    b.sync()?;

    write(&ha, bindings, "bind 1 (a)\n");
    a.notice([ha.join(bindings)]);
    a.sync()?;
    write(&hb, bindings, "bind 1 (b, unseen)\n");
    let st = b.sync()?;
    assert_eq!(st.conflicts, 1);
    assert_eq!(
        fs::read_to_string(hb.join(bindings))?,
        "bind 1 (b, unseen)\n"
    );
    assert_eq!(b.settings_status().held, [PathBuf::from(bindings)]);
    // held versions and backups are this user's alone
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let backups = w.tmp.path().join("b-backups");
    assert_eq!(mode(backups.clone()), 0o700);
    assert_eq!(mode(backups.join("held").join(bindings)), 0o600);
    Ok(())
}

const CHROMIUM_FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/chromium");

/// A Chromium user data dir from the fixtures, with one profile.
fn chromium_profile(data: &Path) {
    let f = Path::new(CHROMIUM_FIXTURES);
    let default = data.join("Default");
    fs::create_dir_all(&default).unwrap();
    fs::copy(f.join("Local State"), data.join("Local State")).unwrap();
    fs::copy(f.join("Bookmarks"), default.join("Bookmarks")).unwrap();
    fs::copy(f.join("Preferences"), default.join("Preferences")).unwrap();
    let db = rusqlite::Connection::open(default.join("Web Data")).unwrap();
    db.execute_batch(&fs::read_to_string(f.join("keywords.sql")).unwrap())
        .unwrap();
}

/// A stand-in NetworkManager: saved networks in memory.
#[derive(Default, Clone)]
struct FakeNm {
    saved: Arc<std::sync::Mutex<Vec<omacloud_core::wifi::Saved>>>,
    /// How many times NetworkManager was asked for the saved networks.
    asked: Arc<std::sync::atomic::AtomicUsize>,
    /// Connections in use.
    in_use: Arc<std::sync::Mutex<Vec<String>>>,
    /// Fail to export a connection (a transient error, not a refusal).
    export_fails: Arc<std::sync::atomic::AtomicBool>,
    /// Turn every change down, and how many were tried.
    refuse: Arc<std::sync::atomic::AtomicBool>,
    changes: Arc<std::sync::atomic::AtomicUsize>,
}

impl FakeNm {
    fn turn_down(&self) -> Result<()> {
        _ = self
            .changes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.refuse.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(omacloud_core::merged::Refused("refused".into()).into());
        }
        Ok(())
    }
    fn uuid(&self, ssid: &str) -> String {
        self.saved
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.name == ssid)
            .unwrap()
            .uuid
            .clone()
    }
}

impl omacloud_core::wifi::Manager for FakeNm {
    fn ready(&self) -> Result<(), String> {
        Ok(())
    }
    fn saved(&self) -> Result<Vec<omacloud_core::wifi::Saved>> {
        _ = self
            .asked
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(self.saved.lock().unwrap().clone())
    }
    fn active(&self) -> Vec<String> {
        self.in_use.lock().unwrap().clone()
    }
    fn add(&self, n: &omacloud_core::wifi::Network) -> Result<()> {
        self.turn_down()?;
        let mut s = self.saved.lock().unwrap();
        let uuid = format!("uuid-{}", s.len());
        s.push(omacloud_core::wifi::Saved {
            uuid,
            name: n.ssid.clone(),
            key: Some(n.key()),
            network: Ok(n.clone()),
        });
        Ok(())
    }
    fn change(&self, uuid: &str, n: &omacloud_core::wifi::Network) -> Result<()> {
        self.turn_down()?;
        for s in self.saved.lock().unwrap().iter_mut() {
            if s.uuid == uuid {
                s.network = Ok(n.clone());
            }
        }
        Ok(())
    }
    fn forget(&self, uuid: &str) -> Result<()> {
        self.turn_down()?;
        self.saved.lock().unwrap().retain(|s| s.uuid != uuid);
        Ok(())
    }
    fn export(&self, uuid: &str) -> Result<String> {
        if self.export_fails.load(std::sync::atomic::Ordering::Relaxed) {
            anyhow::bail!("nmcli connection failed");
        }
        Ok(format!("connection.uuid:{uuid}\n"))
    }
}

impl FakeNm {
    fn ssids(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .saved
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        v.sort();
        v
    }
    fn psk(&self, ssid: &str) -> Option<String> {
        self.saved
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.name == ssid)
            .and_then(|s| s.network.as_ref().ok()?.psk.clone())
    }
    /// The network's key turns unreadable here (a keyring takes it).
    fn hide_key(&self, ssid: &str) {
        for s in self.saved.lock().unwrap().iter_mut() {
            if s.name == ssid {
                s.network = Err("its key isn't readable here".into());
            }
        }
    }
}

fn network(ssid: &str, psk: &str) -> omacloud_core::wifi::Network {
    omacloud_core::wifi::Network {
        ssid: ssid.into(),
        security: "wpa-psk".into(),
        psk: Some(psk.into()),
        hidden: false,
        autoconnect: true,
        priority: 0,
    }
}

impl World {
    /// A device with settings, its Chromium in `<device>-home/.config/chromium`
    /// and Wi-Fi through `nm`.
    fn engine_with_apps(&self, device: &str, nm: &FakeNm) -> Result<Engine> {
        let dir = self.dir(device);
        fs::create_dir_all(&dir)?;
        let home = self.home(device);
        fs::create_dir_all(&home)?;
        let merged = self.tmp.path().join(format!("{device}-merged"));
        let mut groups: Vec<Arc<dyn omacloud_core::merged::Group>> =
            omacloud_core::chromium::Chromium::for_home(&home, &merged.join("scratch"))
                .into_iter()
                .map(|c| Arc::new(c) as Arc<dyn omacloud_core::merged::Group>)
                .collect();
        groups.push(Arc::new(omacloud_core::wifi::Wifi::new(Box::new(
            nm.clone(),
        ))));
        groups.push(Arc::new(omacloud_core::agents::Agents::new(&home)));
        let mut setup = self.setup(
            device,
            dir,
            Some(self.tmp.path().join(format!("{device}.state.json"))),
        );
        setup.settings = Some(omacloud_core::SettingsSetup {
            home,
            manifest: omacloud_core::settings::Manifest::bundled(),
            backups: self.tmp.path().join(format!("{device}-backups")),
            packages: self.tmp.path().join(format!("{device}-packages")),
            secrets: self.tmp.path().join(format!("{device}-secrets")),
            merged,
            groups,
        });
        Engine::new(setup, self.coord.clone())
    }
}

fn read_json(p: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(p).unwrap()).unwrap()
}

fn bookmark_names(bookmarks: &Path) -> Vec<String> {
    fn walk(v: &serde_json::Value, out: &mut Vec<String>) {
        if v["type"] == "url" {
            out.push(v["name"].as_str().unwrap().to_string());
        }
        for c in v["children"].as_array().into_iter().flatten() {
            walk(c, out);
        }
    }
    let f = read_json(bookmarks);
    let mut out = Vec::new();
    for r in ["bookmark_bar", "other", "synced"] {
        walk(&f["roots"][r], &mut out);
    }
    out
}

/// Add a bookmark to a `Bookmarks` file's other folder, as Chromium would.
fn add_bookmark(bookmarks: &Path, id: &str, guid: &str, name: &str) {
    let mut f = read_json(bookmarks);
    f["roots"]["other"]["children"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "date_added": "13400000990000000", "date_last_used": "0", "guid": guid,
            "id": id, "name": name, "type": "url", "url": format!("https://{id}.example.com/"),
        }));
    fs::write(bookmarks, serde_json::to_vec_pretty(&f).unwrap()).unwrap();
}

#[test]
fn chromium_merges_across_computers_and_waits_while_open() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (ca, cb) = (
        w.home("a").join(".config/chromium"),
        w.home("b").join(".config/chromium"),
    );
    chromium_profile(&ca);
    chromium_profile(&cb);
    let nm = FakeNm::default();
    let (mut a, mut b) = (w.engine_with_apps("a", &nm)?, w.engine_with_apps("b", &nm)?);
    a.sync()?;
    b.sync()?;
    a.sync()?;
    // the same profile on both: nothing doubles
    assert_eq!(bookmark_names(&cb.join("Default/Bookmarks")).len(), 4);

    // a adds a bookmark and a font size; b has an extension a lacks
    add_bookmark(
        &ca.join("Default/Bookmarks"),
        "20",
        "6d2f1b0e-2222-4a5b-9c3d-000000000001",
        "From a",
    );
    let mut prefs = read_json(&ca.join("Default/Preferences"));
    prefs["webkit"]["webprefs"]["default_font_size"] = serde_json::json!(20);
    fs::write(ca.join("Default/Preferences"), serde_json::to_vec(&prefs)?)?;
    let mut prefs = read_json(&cb.join("Default/Preferences"));
    prefs["extensions"]["settings"]["gighmmpiobklfepjocnamgkkbiglidom"] = serde_json::json!(
        {"from_webstore": true, "location": 1, "manifest": {"name": "Example Tool"}}
    );
    fs::write(cb.join("Default/Preferences"), serde_json::to_vec(&prefs)?)?;
    a.notice([ca.join("Default/Bookmarks"), ca.join("Default/Preferences")]);
    b.notice([cb.join("Default/Preferences")]);
    a.sync()?;

    // b's Chromium is open: nothing written into its profile
    let me = format!("{}-{}", hostname(), std::process::id());
    symlink(&me, cb.join("SingletonLock"))?;
    let before = fs::read(cb.join("Default/Bookmarks"))?;
    b.sync()?;
    assert_eq!(fs::read(cb.join("Default/Bookmarks"))?, before);
    let chromium = |e: &Engine| {
        e.settings_status()
            .groups
            .into_iter()
            .find(|g| g.title == "Chromium: Default")
            .unwrap()
    };
    let st = chromium(&b);
    assert_eq!(st.state, "waiting");
    assert!(st.detail.contains("bookmarks"));
    // an edit on b meanwhile merges with what waits
    add_bookmark(
        &cb.join("Default/Bookmarks"),
        "21",
        "6d2f1b0e-2222-4a5b-9c3d-000000000002",
        "From b",
    );
    b.notice([cb.join("Default/Bookmarks")]);
    b.sync()?;
    assert!(!bookmark_names(&cb.join("Default/Bookmarks")).contains(&"From a".to_string()));

    // closed: the next sync writes it all in
    fs::remove_file(cb.join("SingletonLock"))?;
    b.notice([cb.join("SingletonLock")]);
    b.look_at_apps();
    assert!(b.has_pending());
    b.sync()?;
    a.sync()?;
    let names = bookmark_names(&cb.join("Default/Bookmarks"));
    assert!(names.contains(&"From a".to_string()) && names.contains(&"From b".to_string()));
    assert_eq!(
        bookmark_names(&ca.join("Default/Bookmarks")),
        bookmark_names(&cb.join("Default/Bookmarks"))
    );
    let f = read_json(&cb.join("Default/Bookmarks"));
    let sum = omacloud_core::chromium::bookmarks::checksums(&f).0;
    assert_eq!(f["checksum"], serde_json::json!(sum));
    let pb = read_json(&cb.join("Default/Preferences"));
    assert_eq!(pb["webkit"]["webprefs"]["default_font_size"], 20);
    // protected preferences stay as b had them
    assert_eq!(pb["homepage"], "https://home.example.com/");
    assert_eq!(
        pb["protection"],
        read_json(&Path::new(CHROMIUM_FIXTURES).join("Preferences"))["protection"]
    );
    // b's extension is offered to a's Chromium, for its next start
    assert!(
        ca.join("External Extensions/gighmmpiobklfepjocnamgkkbiglidom.json")
            .is_file()
    );
    assert_eq!(chromium(&b).state, "synced");
    // what was replaced is backed up
    assert!(w.tmp.path().join("b-merged/backups/history").is_dir());

    // settled: nothing more moves, nothing is rewritten
    let (fa, fb) = (
        fs::read(ca.join("Default/Bookmarks"))?,
        fs::read(cb.join("Default/Bookmarks"))?,
    );
    a.request_rescan();
    b.request_rescan();
    assert_eq!(a.sync()?.pushed + b.sync()?.pushed, 0);
    assert_eq!(fs::read(ca.join("Default/Bookmarks"))?, fa);
    assert_eq!(fs::read(cb.join("Default/Bookmarks"))?, fb);
    Ok(())
}

fn hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap()
        .trim()
        .to_string()
}

#[test]
fn wifi_networks_merge_and_deletes_travel_from_who_had_them() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (na, nb) = (FakeNm::default(), FakeNm::default());
    omacloud_core::wifi::Manager::add(&na, &network("Home", "home-key"))?;
    omacloud_core::wifi::Manager::add(&nb, &network("Office", "office-key"))?;
    let (mut a, mut b) = (w.engine_with_apps("a", &na)?, w.engine_with_apps("b", &nb)?);
    a.sync()?;
    b.sync()?;
    a.sync()?;
    let ssids = |nm: &FakeNm| -> Vec<String> {
        let mut v: Vec<String> = nm
            .saved
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.network.as_ref().unwrap().ssid.clone())
            .collect();
        v.sort();
        v
    };
    assert_eq!(ssids(&na), ["Home", "Office"]);
    assert_eq!(ssids(&nb), ["Home", "Office"]);
    // the document syncs in the settings tree, keys and all (encrypted)
    let doc = read_json(&w.tmp.path().join("a-merged/synced/wifi/networks.json"));
    assert_eq!(doc["wpa-psk Office"]["psk"], "office-key");

    // a forgets Home: b, which had it, forgets it too
    let home = na
        .saved
        .lock()
        .unwrap()
        .iter()
        .find(|s| s.name == "Home")
        .unwrap()
        .uuid
        .clone();
    omacloud_core::wifi::Manager::forget(&na, &home)?;
    // (a new look, as after the five minutes a look stands)
    let mut a = w.engine_with_apps("a", &na)?;
    a.sync()?;
    let mut b = w.engine_with_apps("b", &nb)?;
    b.sync()?;
    assert_eq!(ssids(&nb), ["Office"]);
    Ok(())
}

#[test]
fn new_dotfiles_sync_and_a_missing_one_is_no_delete() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (ha, hb) = (w.home("a"), w.home("b"));
    write(&ha, ".gitconfig", "[user]\n\tname = A\n");
    write(&ha, ".config/nvim/lua/plugins/theme.lua", "return {}\n");
    write(&ha, ".config/fish/fish_variables", "machine state\n");
    write(&ha, ".npmrc", "//registry.example.com/:_authToken=x\n");
    write(&hb, ".inputrc", "set bell-style none\n");
    let (mut a, mut b) = (w.engine_with_settings("a")?, w.engine_with_settings("b")?);
    a.sync()?;
    b.sync()?;
    a.sync()?;
    assert_eq!(
        fs::read_to_string(hb.join(".gitconfig"))?,
        "[user]\n\tname = A\n"
    );
    assert!(hb.join(".config/nvim/lua/plugins/theme.lua").is_file());
    assert!(!hb.join(".config/fish/fish_variables").exists());
    assert!(!hb.join(".npmrc").exists());
    // b never had .gitconfig's history before; a lacked .inputrc: both arrive
    assert_eq!(
        fs::read_to_string(ha.join(".inputrc"))?,
        "set bell-style none\n"
    );
    let shell = b
        .settings_status()
        .groups
        .into_iter()
        .find(|g| g.group == "shell")
        .unwrap();
    assert_eq!(shell.state, "synced");
    Ok(())
}

#[test]
fn a_network_saved_since_the_last_look_is_never_forgotten() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (na, nb) = (FakeNm::default(), FakeNm::default());
    omacloud_core::wifi::Manager::add(&na, &network("Home", "home-key"))?;
    let (mut a, mut b) = (w.engine_with_apps("a", &na)?, w.engine_with_apps("b", &nb)?);
    a.sync()?;
    b.sync()?;
    // b saves a network; a saves another right after its last look
    omacloud_core::wifi::Manager::add(&nb, &network("Office", "office-key"))?;
    let mut b = w.engine_with_apps("b", &nb)?; // its next look
    b.sync()?;
    omacloud_core::wifi::Manager::add(&na, &network("Cafe", "cafe-key"))?;
    a.sync()?; // a's look isn't due: the write looks first
    a.sync()?;
    assert_eq!(na.ssids(), ["Cafe", "Home", "Office"]);
    let mut b = w.engine_with_apps("b", &nb)?;
    b.sync()?;
    assert_eq!(nb.ssids(), ["Cafe", "Home", "Office"]);
    Ok(())
}

#[test]
fn one_network_two_keys_each_computer_keeps_its_own() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (na, nb) = (FakeNm::default(), FakeNm::default());
    omacloud_core::wifi::Manager::add(&na, &network("Home", "key-on-a"))?;
    omacloud_core::wifi::Manager::add(&nb, &network("Home", "key-on-b"))?;
    let (mut a, mut b) = (w.engine_with_apps("a", &na)?, w.engine_with_apps("b", &nb)?);
    a.sync()?;
    b.sync()?;
    a.sync()?;
    assert_eq!(na.psk("Home").as_deref(), Some("key-on-a"));
    assert_eq!(nb.psk("Home").as_deref(), Some("key-on-b"));
    // the computer whose key the merge didn't take hears of it too
    let wifi = a
        .settings_status()
        .groups
        .into_iter()
        .find(|g| g.group == "wifi")
        .unwrap();
    assert_eq!(wifi.state, "settled");
    assert!(wifi.detail.contains("each keeps its own"));
    assert!(!wifi.detail.contains("key-on"));
    // the router's password changes and a's key with it: now b takes it
    let home = na
        .saved
        .lock()
        .unwrap()
        .iter()
        .find(|s| s.name == "Home")
        .unwrap()
        .uuid
        .clone();
    omacloud_core::wifi::Manager::change(&na, &home, &network("Home", "new-key"))?;
    let mut a = w.engine_with_apps("a", &na)?;
    a.sync()?;
    let mut b = w.engine_with_apps("b", &nb)?;
    b.sync()?;
    assert_eq!(nb.psk("Home").as_deref(), Some("new-key"));
    // a computer that doesn't have it takes the synced key
    let w2 = w.engine_with_apps("b", &nb)?;
    drop(w2);
    // and a key changed on one later travels, after a backup of the old
    let nb2 = nb.clone();
    let mut b = w.engine_with_apps("b", &nb2)?;
    omacloud_core::wifi::Manager::add(&na, &network("Office", "old"))?;
    let mut a = w.engine_with_apps("a", &na)?;
    a.sync()?;
    b.sync()?;
    let office = na
        .saved
        .lock()
        .unwrap()
        .iter()
        .find(|s| s.name == "Office")
        .unwrap()
        .uuid
        .clone();
    omacloud_core::wifi::Manager::change(&na, &office, &network("Office", "new"))?;
    let mut a = w.engine_with_apps("a", &na)?;
    a.sync()?;
    let mut b = w.engine_with_apps("b", &nb)?;
    b.sync()?;
    assert_eq!(nb.psk("Office").as_deref(), Some("new"));
    let history = w.tmp.path().join("b-merged/backups/history");
    let backed_up = fs::read_dir(&history)?
        .flatten()
        .any(|d| fs::read_dir(d.path().join("wifi")).is_ok_and(|mut f| f.next().is_some()));
    assert!(backed_up);
    Ok(())
}

#[test]
fn a_network_that_cant_be_read_is_neither_forgotten_nor_doubled() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (na, nb) = (FakeNm::default(), FakeNm::default());
    omacloud_core::wifi::Manager::add(&na, &network("Home", "k"))?;
    omacloud_core::wifi::Manager::add(&na, &network("Office", "o"))?;
    omacloud_core::wifi::Manager::add(&nb, &network("Office", "o"))?;
    nb.hide_key("Office"); // b has it, its key out of reach
    let (mut a, mut b) = (w.engine_with_apps("a", &na)?, w.engine_with_apps("b", &nb)?);
    a.sync()?;
    b.sync()?;
    assert_eq!(nb.ssids(), ["Home", "Office"]); // not a second Office
    // Home turns unreadable on a: b keeps it
    na.hide_key("Home");
    let mut a = w.engine_with_apps("a", &na)?;
    a.sync()?;
    let mut b = w.engine_with_apps("b", &nb)?;
    b.sync()?;
    assert_eq!(nb.ssids(), ["Home", "Office"]);
    Ok(())
}

#[test]
fn an_engine_added_while_chromium_writes_its_database_is_kept() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (ca, cb) = (
        w.home("a").join(".config/chromium"),
        w.home("b").join(".config/chromium"),
    );
    chromium_profile(&ca);
    chromium_profile(&cb);
    let nm = FakeNm::default();
    let (mut a, mut b) = (w.engine_with_apps("a", &nm)?, w.engine_with_apps("b", &nm)?);
    a.sync()?;
    b.sync()?;
    // a adds an engine; b adds one, mid-write when b syncs
    let add = |dir: &Path, k: &str, guid: &str| {
        let db = rusqlite::Connection::open(dir.join("Default/Web Data")).unwrap();
        db.execute(
            "INSERT INTO keywords (short_name, keyword, favicon_url, url, safe_for_autoreplace, sync_guid) VALUES (?1, ?1, '', ?2, 0, ?3)",
            [k, &format!("https://{k}.example.com/?q={{searchTerms}}"), guid],
        )
        .unwrap();
    };
    add(&ca, "fromA", "bbbbbbbb-0000-4000-8000-00000000000a");
    a.notice([ca.join("Default/Web Data")]);
    a.sync()?;
    add(&cb, "fromB", "bbbbbbbb-0000-4000-8000-00000000000b");
    fs::write(cb.join("Default/Web Data-journal"), "mid-write")?;
    b.sync()?;
    let keywords = |dir: &Path| -> Vec<String> {
        let db = rusqlite::Connection::open(dir.join("Default/Web Data")).unwrap();
        let mut st = db
            .prepare("SELECT keyword FROM keywords ORDER BY keyword")
            .unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert!(keywords(&cb).contains(&"fromB".to_string()));
    // written once readable, with both (reading it above may have
    // cleared the made-up journal already)
    _ = fs::remove_file(cb.join("Default/Web Data-journal"));
    let mut b = w.engine_with_apps("b", &nm)?;
    b.sync()?;
    a.sync()?;
    for dir in [&ca, &cb] {
        let k = keywords(dir);
        assert!(
            k.contains(&"fromA".to_string()) && k.contains(&"fromB".to_string()),
            "{k:?}"
        );
    }
    Ok(())
}

#[test]
fn chromium_rewriting_preferences_costs_a_local_look_at_most_once_a_minute() -> Result<()> {
    let w = World::new(&["a"])?;
    let ca = w.home("a").join(".config/chromium");
    chromium_profile(&ca);
    let mut a = w.engine_with_apps("a", &FakeNm::default())?;
    a.sync()?;
    assert!(!a.has_pending());
    let prefs = ca.join("Default/Preferences");
    let edit = |f: &dyn Fn(&mut serde_json::Value)| {
        let mut p = read_json(&prefs);
        f(&mut p);
        fs::write(&prefs, serde_json::to_vec(&p).unwrap()).unwrap();
    };
    // window placement: nothing that syncs, nothing to push
    edit(&|p| p["browser"]["window_placement"]["top"] = serde_json::json!(99));
    a.notice([prefs.clone()]);
    a.look_at_apps();
    assert!(!a.has_pending());
    // a font size within the minute: looked at later, not now
    edit(&|p| p["webkit"]["webprefs"]["default_font_size"] = serde_json::json!(24));
    a.notice([prefs.clone()]);
    a.look_at_apps();
    assert!(!a.has_pending());
    // a fresh engine (a minute on): it's found and pushed
    let mut a = w.engine_with_apps("a", &FakeNm::default())?;
    a.sync()?;
    edit(&|p| p["webkit"]["webprefs"]["default_font_size"] = serde_json::json!(26));
    a.notice([prefs]);
    a.look_at_apps();
    assert!(a.has_pending());
    Ok(())
}

#[test]
fn a_bookmark_renamed_on_both_says_so_on_both() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (ca, cb) = (
        w.home("a").join(".config/chromium"),
        w.home("b").join(".config/chromium"),
    );
    chromium_profile(&ca);
    chromium_profile(&cb);
    let nm = FakeNm::default();
    let (mut a, mut b) = (w.engine_with_apps("a", &nm)?, w.engine_with_apps("b", &nm)?);
    a.sync()?;
    b.sync()?;
    let rename = |dir: &Path, name: &str| {
        let f = dir.join("Default/Bookmarks");
        let mut v = read_json(&f);
        v["roots"]["other"]["children"][0]["name"] = serde_json::json!(name);
        fs::write(&f, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
    };
    rename(&ca, "Soup (a)");
    rename(&cb, "Soup (b)");
    a.notice([ca.join("Default/Bookmarks")]);
    b.notice([cb.join("Default/Bookmarks")]);
    a.sync()?;
    b.sync()?; // b merges: its name stays
    a.sync()?;
    for e in [&a, &b] {
        let item = e
            .settings_status()
            .groups
            .into_iter()
            .find(|g| g.title == "Chromium: Default")
            .unwrap();
        assert_eq!(item.state, "settled");
        assert!(
            item.detail.contains("Soup (a)") && item.detail.contains("kept b's"),
            "{}",
            item.detail
        );
        assert!(!item.detail.contains("https://"));
    }
    Ok(())
}

#[test]
fn agent_settings_and_skills_travel_but_never_their_sign_ins() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (ha, hb) = (w.home("a"), w.home("b"));
    write(&ha, ".claude/settings.json", "{\"theme\": \"dark\"}\n");
    write(&ha, ".claude/skills/review/SKILL.md", "# Review\n");
    write(&ha, ".claude/.credentials.json", "{\"token\": \"x\"}\n");
    write(&ha, ".codex/auth.json", "{}\n");
    write(&ha, ".config/herdr/config.toml", "theme = \"x\"\n");
    // b keeps its pi skills in a repo of its own, linked in
    write(&hb, "repo/skills/mine/SKILL.md", "b's own\n");
    write(&ha, ".pi/agent/skills/mine/SKILL.md", "a's\n");
    fs::create_dir_all(hb.join(".pi/agent"))?;
    symlink(hb.join("repo/skills"), hb.join(".pi/agent/skills"))?;
    let (mut a, mut b) = (w.engine_with_settings("a")?, w.engine_with_settings("b")?);
    a.sync()?;
    b.sync()?;
    assert_eq!(
        fs::read_to_string(hb.join(".claude/skills/review/SKILL.md"))?,
        "# Review\n"
    );
    assert!(hb.join(".config/herdr/config.toml").is_file());
    assert!(!hb.join(".claude/.credentials.json").exists());
    assert!(!hb.join(".codex/auth.json").exists());
    // nothing written through b's link
    assert_eq!(
        fs::read_to_string(hb.join("repo/skills/mine/SKILL.md"))?,
        "b's own\n"
    );
    assert!(b.settings_status().dormant.is_none());
    // a binary or a sign-in noticed by the watcher still isn't pushed
    fs::write(ha.join(".claude/skills/review/icon.png"), b"\x89PNG\0\0")?;
    a.notice([
        ha.join(".claude/skills/review/icon.png"),
        ha.join(".claude/.credentials.json"),
    ]);
    assert_eq!(a.sync()?.pushed, 0);
    let agents = a
        .settings_status()
        .groups
        .into_iter()
        .find(|g| g.group == "agents")
        .unwrap();
    assert!(agents.detail.starts_with("3 files"), "{}", agents.detail);
    Ok(())
}

#[test]
fn a_browsers_busy_files_cost_nothing_and_its_look_leaves_wifi_alone() -> Result<()> {
    let w = World::new(&["a"])?;
    let ca = w.home("a").join(".config/chromium");
    chromium_profile(&ca);
    let nm = FakeNm::default();
    let mut a = w.engine_with_apps("a", &nm)?;
    a.sync()?;
    // the journals a browser writes every second: nothing to do
    assert!(!a.notice([
        ca.join("Default/Cookies-journal"),
        ca.join("Default/History")
    ]));
    assert!(!a.has_pending());
    // its bookmarks: a look at Chromium, not at Wi-Fi
    let asked = nm.asked.load(std::sync::atomic::Ordering::Relaxed);
    assert!(a.notice([ca.join("Default/Bookmarks")]));
    a.look_at_apps();
    assert_eq!(nm.asked.load(std::sync::atomic::Ordering::Relaxed), asked);
    Ok(())
}

#[test]
fn codex_trust_stays_put_while_its_settings_travel() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (ha, hb) = (w.home("a"), w.home("b"));
    let config = |model: &str, projects: &[&str]| {
        let mut c = format!("model = \"{model}\"\n");
        for p in projects {
            c.push_str(&format!(
                "\n[projects.\"{p}\"]\ntrust_level = \"trusted\"\n"
            ));
        }
        c
    };
    write(
        &ha,
        ".codex/config.toml",
        &config("gpt-5", &["/home/a/work"]),
    );
    write(
        &hb,
        ".codex/config.toml",
        &config("gpt-5", &["/home/b/play"]),
    );
    let nm = FakeNm::default();
    let (mut a, mut b) = (w.engine_with_apps("a", &nm)?, w.engine_with_apps("b", &nm)?);
    a.sync()?;
    b.sync()?;
    a.sync()?;
    // each trusts one more project, and a changes the model
    write(
        &ha,
        ".codex/config.toml",
        &config("gpt-6", &["/home/a/work", "/home/a/two"]),
    );
    write(
        &hb,
        ".codex/config.toml",
        &config("gpt-5", &["/home/b/play", "/home/b/two"]),
    );
    let (mut a, mut b) = (w.engine_with_apps("a", &nm)?, w.engine_with_apps("b", &nm)?);
    a.sync()?;
    b.sync()?;
    a.sync()?;
    let (ca, cb) = (
        fs::read_to_string(ha.join(".codex/config.toml"))?,
        fs::read_to_string(hb.join(".codex/config.toml"))?,
    );
    assert!(cb.contains("gpt-6"), "{cb}");
    assert!(cb.contains("/home/b/play") && cb.contains("/home/b/two") && !cb.contains("/home/a/"));
    assert!(ca.contains("/home/a/two") && !ca.contains("/home/b/"));
    assert!(b.settings_status().held.is_empty());
    Ok(())
}

#[test]
fn with_settings_off_or_standing_down_the_daemon_can_idle() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let mut a = w.engine("a")?;
    a.sync()?;
    assert!(!a.has_pending());
    // b stands down next to a dotfile manager
    fs::create_dir_all(w.home("b").join(".git"))?;
    let mut b = w.engine_with_apps("b", &FakeNm::default())?;
    b.sync()?;
    assert!(b.settings_status().dormant.is_some());
    assert!(!b.has_pending());
    Ok(())
}

#[test]
fn the_default_engine_kept_here_isnt_written_again_and_again() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (ca, cb) = (
        w.home("a").join(".config/chromium"),
        w.home("b").join(".config/chromium"),
    );
    chromium_profile(&ca);
    chromium_profile(&cb);
    // b searches with its own engine, m, by default
    let mut prefs = read_json(&cb.join("Default/Preferences"));
    prefs["default_search_provider_data"]["template_url_data"]["synced_guid"] =
        serde_json::json!("aaaaaaaa-0000-4000-8000-000000000005");
    fs::write(cb.join("Default/Preferences"), serde_json::to_vec(&prefs)?)?;
    let nm = FakeNm::default();
    let (mut a, mut b) = (w.engine_with_apps("a", &nm)?, w.engine_with_apps("b", &nm)?);
    a.sync()?;
    b.sync()?;
    // a deletes m
    let db = rusqlite::Connection::open(ca.join("Default/Web Data"))?;
    _ = db.execute("DELETE FROM keywords WHERE keyword = 'm'", [])?;
    drop(db);
    a.notice([ca.join("Default/Web Data")]);
    a.sync()?;
    b.sync()?;
    let history = w.tmp.path().join("b-merged/backups/history");
    let count = || fs::read_dir(&history).map_or(0, |d| d.count());
    let before = count();
    for _ in 0..3 {
        b.sync()?;
    }
    assert_eq!(count(), before);
    let item = b
        .settings_status()
        .groups
        .into_iter()
        .find(|g| g.title == "Chromium: Default")
        .unwrap();
    assert_ne!(item.state, "waiting");
    // b's default is still there
    let db = rusqlite::Connection::open(cb.join("Default/Web Data"))?;
    let n: i64 = db.query_row(
        "SELECT COUNT(*) FROM keywords WHERE keyword = 'm'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(n, 1);
    Ok(())
}

#[test]
fn a_network_this_computer_cant_take_isnt_tried_every_sync() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (na, nb) = (FakeNm::default(), FakeNm::default());
    omacloud_core::wifi::Manager::add(&na, &network("Cafe", "k"))?;
    omacloud_core::wifi::Manager::add(&nb, &network("Cafe", "k"))?;
    nb.hide_key("Cafe");
    let (mut a, mut b) = (w.engine_with_apps("a", &na)?, w.engine_with_apps("b", &nb)?);
    a.sync()?;
    b.sync()?;
    b.sync()?;
    let asked = nb.asked.load(std::sync::atomic::Ordering::Relaxed);
    b.sync()?;
    b.sync()?;
    assert_eq!(nb.asked.load(std::sync::atomic::Ordering::Relaxed), asked);
    let wifi = b
        .settings_status()
        .groups
        .into_iter()
        .find(|g| g.group == "wifi")
        .unwrap();
    assert_ne!(wifi.state, "waiting");
    assert!(wifi.not_synced.iter().any(|n| n.starts_with("Cafe")));
    Ok(())
}

#[test]
fn a_network_in_use_goes_once_disconnected() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (na, nb) = (FakeNm::default(), FakeNm::default());
    for nm in [&na, &nb] {
        omacloud_core::wifi::Manager::add(nm, &network("Home", "k"))?;
        omacloud_core::wifi::Manager::add(nm, &network("Cafe", "c"))?;
    }
    let (mut a, mut b) = (w.engine_with_apps("a", &na)?, w.engine_with_apps("b", &nb)?);
    a.sync()?;
    b.sync()?;
    // a forgets Cafe; b is connected to it
    omacloud_core::wifi::Manager::forget(&na, &na.uuid("Cafe"))?;
    nb.in_use.lock().unwrap().push(nb.uuid("Cafe"));
    let mut a = w.engine_with_apps("a", &na)?;
    a.sync()?;
    let mut b = w.engine_with_apps("b", &nb)?;
    b.sync()?;
    assert_eq!(nb.ssids(), ["Cafe", "Home"]);
    let wifi = b
        .settings_status()
        .groups
        .into_iter()
        .find(|g| g.group == "wifi")
        .unwrap();
    assert_eq!(wifi.state, "waiting");
    assert!(wifi.detail.contains("in use"), "{}", wifi.detail);
    // disconnected: the next sync forgets it
    nb.in_use.lock().unwrap().clear();
    b.sync()?;
    assert_eq!(nb.ssids(), ["Home"]);
    Ok(())
}

#[test]
fn a_change_networkmanager_turns_down_is_tried_once_and_said() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (na, nb) = (FakeNm::default(), FakeNm::default());
    omacloud_core::wifi::Manager::add(&na, &network("Home", "old"))?;
    omacloud_core::wifi::Manager::add(&nb, &network("Home", "old"))?;
    let (mut a, mut b) = (w.engine_with_apps("a", &na)?, w.engine_with_apps("b", &nb)?);
    a.sync()?;
    b.sync()?;
    omacloud_core::wifi::Manager::change(&na, &na.uuid("Home"), &network("Home", "new"))?;
    let mut a = w.engine_with_apps("a", &na)?;
    a.sync()?;
    nb.refuse.store(true, std::sync::atomic::Ordering::Relaxed);
    let tried = nb.changes.load(std::sync::atomic::Ordering::Relaxed);
    let mut b = w.engine_with_apps("b", &nb)?;
    for _ in 0..4 {
        b.sync()?;
    }
    assert_eq!(
        nb.changes.load(std::sync::atomic::Ordering::Relaxed),
        tried + 1
    );
    let copies = fs::read_dir(w.tmp.path().join("b-merged/backups/history"))?
        .flatten()
        .filter(|d| d.path().join("wifi").is_dir())
        .count();
    assert_eq!(copies, 1);
    let wifi = b
        .settings_status()
        .groups
        .into_iter()
        .find(|g| g.group == "wifi")
        .unwrap();
    assert_eq!(wifi.state, "refused");
    assert!(wifi.detail.contains("not applied"), "{}", wifi.detail);
    Ok(())
}

/// An app kept in memory whose writes fail a given number of times, as
/// when Chromium opens between the check and the write.
#[derive(Default, Clone)]
struct FlakyApp {
    state: Arc<std::sync::Mutex<serde_json::Value>>,
    fail: Arc<std::sync::atomic::AtomicUsize>,
    /// Writes tried.
    tries: Arc<std::sync::atomic::AtomicUsize>,
}

impl omacloud_core::merged::Group for FlakyApp {
    fn name(&self) -> &str {
        "flaky"
    }
    fn docs(&self) -> Vec<String> {
        vec!["doc.json".into()]
    }
    fn capture(
        &self,
        _doc: &str,
        _applied: Option<&serde_json::Value>,
    ) -> Result<Option<serde_json::Value>> {
        Ok(Some(self.state.lock().unwrap().clone()))
    }
    fn busy(&self, _doc: &str) -> Option<String> {
        None
    }
    fn apply(
        &self,
        _doc: &str,
        value: &serde_json::Value,
        _applied: Option<&serde_json::Value>,
        backups: &Path,
    ) -> Result<()> {
        use std::sync::atomic::Ordering::Relaxed;
        _ = self.tries.fetch_add(1, Relaxed);
        let before = self.state.lock().unwrap().to_string();
        omacloud_core::merged::back_up_bytes(backups, Path::new("flaky/doc"), before.as_bytes())?;
        if self.fail.load(Relaxed) > 0 {
            _ = self.fail.fetch_sub(1, Relaxed);
            anyhow::bail!("Chromium is open");
        }
        *self.state.lock().unwrap() = value.clone();
        Ok(())
    }
    fn merge(
        &self,
        _doc: &str,
        base: Option<&serde_json::Value>,
        ours: &serde_json::Value,
        theirs: &serde_json::Value,
    ) -> omacloud_core::merged::Merged {
        // a change on one side wins
        let changed_here = base != Some(ours);
        omacloud_core::merged::Merged {
            value: if changed_here { ours } else { theirs }.clone(),
            conflicts: Vec::new(),
        }
    }
    fn status(
        &self,
        _mirror: &Path,
        _waiting: &[(String, String)],
    ) -> Vec<omacloud_core::merged::Item> {
        vec![omacloud_core::merged::Item {
            group: "flaky".into(),
            title: "Flaky".into(),
            state: "synced".into(),
            detail: String::new(),
            install: Vec::new(),
            not_synced: Vec::new(),
            scope: "flaky/".into(),
        }]
    }
    fn watch_dirs(&self) -> Vec<PathBuf> {
        Vec::new()
    }
    fn concerns(&self, _path: &Path) -> omacloud_core::merged::Concern {
        omacloud_core::merged::Concern::No
    }
}

impl World {
    fn engine_with_group(
        &self,
        device: &str,
        group: Arc<dyn omacloud_core::merged::Group>,
    ) -> Result<Engine> {
        let dir = self.dir(device);
        fs::create_dir_all(&dir)?;
        let home = self.home(device);
        fs::create_dir_all(&home)?;
        let mut setup = self.setup(
            device,
            dir,
            Some(self.tmp.path().join(format!("{device}.state.json"))),
        );
        setup.settings = Some(omacloud_core::SettingsSetup {
            home,
            manifest: omacloud_core::settings::Manifest::bundled(),
            backups: self.tmp.path().join(format!("{device}-backups")),
            packages: self.tmp.path().join(format!("{device}-packages")),
            secrets: self.tmp.path().join(format!("{device}-secrets")),
            merged: self.tmp.path().join(format!("{device}-merged")),
            groups: vec![group],
        });
        Engine::new(setup, self.coord.clone())
    }
}

#[test]
fn a_write_that_fails_for_now_is_tried_again() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (pa, pb) = (FlakyApp::default(), FlakyApp::default());
    *pa.state.lock().unwrap() = serde_json::json!({"v": 1});
    *pb.state.lock().unwrap() = serde_json::json!({"v": 1});
    let mut a = w.engine_with_group("a", Arc::new(pa.clone()))?;
    let mut b = w.engine_with_group("b", Arc::new(pb.clone()))?;
    a.sync()?;
    b.sync()?;
    *pa.state.lock().unwrap() = serde_json::json!({"v": 2});
    a.sync()?;
    // the app opens just as b writes: not a refusal
    pb.fail.store(1, std::sync::atomic::Ordering::Relaxed);
    b.sync()?;
    assert_eq!(*pb.state.lock().unwrap(), serde_json::json!({"v": 1}));
    b.sync()?;
    assert_eq!(*pb.state.lock().unwrap(), serde_json::json!({"v": 2}));
    Ok(())
}

#[test]
fn a_held_forget_costs_no_more_than_a_look_a_minute() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (na, nb) = (FakeNm::default(), FakeNm::default());
    for nm in [&na, &nb] {
        omacloud_core::wifi::Manager::add(nm, &network("Home", "k"))?;
        omacloud_core::wifi::Manager::add(nm, &network("Cafe", "c"))?;
    }
    let (mut a, mut b) = (w.engine_with_apps("a", &na)?, w.engine_with_apps("b", &nb)?);
    a.sync()?;
    b.sync()?;
    omacloud_core::wifi::Manager::forget(&na, &na.uuid("Cafe"))?;
    nb.in_use.lock().unwrap().push(nb.uuid("Cafe"));
    let mut a = w.engine_with_apps("a", &na)?;
    a.sync()?;
    let mut b = w.engine_with_apps("b", &nb)?;
    b.sync()?;
    let asked = nb.asked.load(std::sync::atomic::Ordering::Relaxed);
    for _ in 0..10 {
        b.sync()?;
    }
    assert_eq!(nb.asked.load(std::sync::atomic::Ordering::Relaxed), asked);
    assert_eq!(nb.ssids(), ["Cafe", "Home"]);
    Ok(())
}

#[test]
fn a_held_forget_outlives_a_refusal_beside_it() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (na, nb) = (FakeNm::default(), FakeNm::default());
    for nm in [&na, &nb] {
        omacloud_core::wifi::Manager::add(nm, &network("Home", "old"))?;
        omacloud_core::wifi::Manager::add(nm, &network("Cafe", "c"))?;
    }
    let (mut a, mut b) = (w.engine_with_apps("a", &na)?, w.engine_with_apps("b", &nb)?);
    a.sync()?;
    b.sync()?;
    // a forgets Cafe (b is on it) and changes Home's key (b refuses)
    omacloud_core::wifi::Manager::forget(&na, &na.uuid("Cafe"))?;
    omacloud_core::wifi::Manager::change(&na, &na.uuid("Home"), &network("Home", "new"))?;
    let mut a = w.engine_with_apps("a", &na)?;
    a.sync()?;
    nb.in_use.lock().unwrap().push(nb.uuid("Cafe"));
    nb.refuse.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut b = w.engine_with_apps("b", &nb)?;
    b.sync()?;
    assert_eq!(nb.ssids(), ["Cafe", "Home"]);
    // disconnected, and NetworkManager willing: both go through
    nb.in_use.lock().unwrap().clear();
    nb.refuse.store(false, std::sync::atomic::Ordering::Relaxed);
    b.sync()?;
    assert_eq!(nb.ssids(), ["Home"]);
    assert_eq!(nb.psk("Home").as_deref(), Some("new"));
    Ok(())
}

#[test]
fn while_a_forget_is_held_newer_changes_still_land() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (na, nb) = (FakeNm::default(), FakeNm::default());
    for nm in [&na, &nb] {
        omacloud_core::wifi::Manager::add(nm, &network("Home", "k"))?;
        omacloud_core::wifi::Manager::add(nm, &network("Cafe", "c"))?;
    }
    let (mut a, mut b) = (w.engine_with_apps("a", &na)?, w.engine_with_apps("b", &nb)?);
    a.sync()?;
    b.sync()?;
    omacloud_core::wifi::Manager::forget(&na, &na.uuid("Cafe"))?;
    nb.in_use.lock().unwrap().push(nb.uuid("Cafe"));
    let mut a = w.engine_with_apps("a", &na)?;
    a.sync()?;
    let mut b = w.engine_with_apps("b", &nb)?;
    b.sync()?; // held: b is on Cafe
    assert_eq!(nb.ssids(), ["Cafe", "Home"]);
    // a saves a new network: it lands on b while b stays connected
    omacloud_core::wifi::Manager::add(&na, &network("Office", "o"))?;
    let mut a = w.engine_with_apps("a", &na)?;
    a.sync()?;
    b.sync()?;
    assert_eq!(nb.ssids(), ["Cafe", "Home", "Office"]);
    // a minute on (a new look), still connected: still held
    let mut b = w.engine_with_apps("b", &nb)?;
    b.sync()?;
    assert_eq!(nb.ssids(), ["Cafe", "Home", "Office"]);
    // disconnected: the same engine's next sync forgets it
    nb.in_use.lock().unwrap().clear();
    b.sync()?;
    assert_eq!(nb.ssids(), ["Home", "Office"]);
    Ok(())
}

#[test]
fn a_write_that_keeps_failing_backs_off_with_one_backup() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (pa, pb) = (FlakyApp::default(), FlakyApp::default());
    *pa.state.lock().unwrap() = serde_json::json!({"v": 1});
    *pb.state.lock().unwrap() = serde_json::json!({"v": 1});
    let mut a = w.engine_with_group("a", Arc::new(pa.clone()))?;
    let mut b = w.engine_with_group("b", Arc::new(pb.clone()))?;
    a.sync()?;
    b.sync()?;
    *pa.state.lock().unwrap() = serde_json::json!({"v": 2});
    a.sync()?;
    pb.fail.store(1000, std::sync::atomic::Ordering::Relaxed);
    for _ in 0..8 {
        b.sync()?;
    }
    // once, once more at the next sync, then not for a while
    assert_eq!(pb.tries.load(std::sync::atomic::Ordering::Relaxed), 2);
    let copies = fs::read_dir(w.tmp.path().join("b-merged/backups/history"))?.count();
    assert_eq!(copies, 1);
    let item = b
        .settings_status()
        .groups
        .into_iter()
        .find(|g| g.group == "flaky")
        .unwrap();
    assert_eq!(item.state, "failed");
    assert!(item.detail.contains("Chromium is open"), "{}", item.detail);
    Ok(())
}

#[test]
fn nmcli_not_answering_isnt_a_refusal() -> Result<()> {
    let w = World::new(&["a", "b"])?;
    let (na, nb) = (FakeNm::default(), FakeNm::default());
    omacloud_core::wifi::Manager::add(&na, &network("Home", "old"))?;
    omacloud_core::wifi::Manager::add(&nb, &network("Home", "old"))?;
    let (mut a, mut b) = (w.engine_with_apps("a", &na)?, w.engine_with_apps("b", &nb)?);
    a.sync()?;
    b.sync()?;
    omacloud_core::wifi::Manager::change(&na, &na.uuid("Home"), &network("Home", "new"))?;
    let mut a = w.engine_with_apps("a", &na)?;
    a.sync()?;
    nb.export_fails
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let mut b = w.engine_with_apps("b", &nb)?;
    b.sync()?;
    assert_eq!(nb.psk("Home").as_deref(), Some("old"));
    let wifi = b
        .settings_status()
        .groups
        .into_iter()
        .find(|g| g.group == "wifi")
        .unwrap();
    assert_ne!(wifi.state, "refused");
    // it answers again: the next sync takes the change
    nb.export_fails
        .store(false, std::sync::atomic::Ordering::Relaxed);
    b.sync()?;
    assert_eq!(nb.psk("Home").as_deref(), Some("new"));
    Ok(())
}
