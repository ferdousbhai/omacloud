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
use onecloud_core::{
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
        let mut chain = onecloud_core::DeviceChain::new(&w.root);
        chain.advance(&coord)?;
        let err = onecloud_core::HeadTracker::default()
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
    write(&da, ".onecloudignore", "*.log\nbuild/\n");
    write(&da, "build/out.o", "binary\n");
    write(&da, "run.log", "noise\n");
    write(&da, ".notes.md.swp", "vim swap\n"); // a built-in default
    a.request_rescan();
    a.sync()?;
    b.sync()?;
    assert!(db.join(".onecloudignore").exists());
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
        setup.settings = Some(onecloud_core::SettingsSetup {
            home,
            manifest: onecloud_core::settings::Manifest::bundled(),
            backups: self.tmp.path().join(format!("{device}-backups")),
            packages: self.tmp.path().join(format!("{device}-packages")),
            secrets: self.tmp.path().join(format!("{device}-secrets")),
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
    assert!(!w.dir("b").join(".onecloud").exists());

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
        a.versions(&Path::new(".onecloud/settings").join(bindings))?
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
    // b manages its dotfiles with links: onecloud leaves them alone
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
    assert!(!w.dir("a").join(".onecloud").exists());
    // a restart finds nothing to push
    let mut a2 = w.engine_with_settings("a")?;
    assert_eq!(a2.sync()?.pushed, 0);
    Ok(())
}

#[test]
fn secrets_travel_sealed_and_open_only_with_the_recovery_code() -> Result<()> {
    use onecloud_core::secrets;
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
    let root = onecloud_core::devices::root_key(&w.recovery_code)?;
    let hb = w.home("b");
    let opened = secrets::open(&root, &sealed)?;
    secrets::restore(&hb, &opened, &w.tmp.path().join("b-secret-backups"))?;
    assert_eq!(fs::read_to_string(hb.join(".ssh/id_ed25519"))?, "a's key\n");

    // nothing lands in the synced folder, and a restart has nothing to push
    assert!(!w.dir("b").join(".onecloud").exists());
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
    assert!(b.repository()?.0.options["root"].ends_with("onecloud-e1"));
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
        onecloud_core::throttle::THROTTLED_READ_WRITE.load(std::sync::atomic::Ordering::Relaxed);
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
        setup.folders = Some(onecloud_core::Folders {
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
