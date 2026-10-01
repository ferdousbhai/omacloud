//! Phase 0 spike: can rustic_core carry live sync?
//!
//!   rustic-spike live <repo> <work> [files] [rounds]
//!   rustic-spike concurrent <repo> <work> [rounds]
//!   rustic-spike check <repo>
//!   rustic-spike scale <repo> <work> [files] [rounds]       (spike 2)
//!   rustic-spike conflicts <repo> <work> [rounds]           (spike 2)
//!
//! `repo` is a local path or an opendal URL (e.g. `opendal:s3`, configured via
//! RUSTIC_OPT_* env vars, see `backends`). Password is "spike".

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::Barrier,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use rustic_backend::BackendOptions;
use rustic_core::{
    BackupOptions, CheckOptions, ConfigOptions, Credentials, KeyOptions, LocalDestination,
    LsOptions, PathList, Repository, RepositoryOptions, RestoreOptions, SnapshotOptions,
    repofile::SnapshotFile,
};

mod sync;

const PASSWORD: &str = "spike";

fn backends(repo: &str) -> Result<rustic_core::RepositoryBackends> {
    // RUSTIC_OPT_<KEY>=<value> becomes a backend option, e.g. RUSTIC_OPT_ENDPOINT
    let options: BTreeMap<String, String> = std::env::vars()
        .filter_map(|(k, v)| {
            k.strip_prefix("RUSTIC_OPT_")
                .map(|k| (k.to_lowercase(), v))
        })
        .collect();
    Ok(BackendOptions::default()
        .repository(repo)
        .options(options)
        .to_backends()?)
}

fn repo_opts() -> RepositoryOptions {
    RepositoryOptions::default()
}

fn init(repo: &str) -> Result<()> {
    Repository::new(&repo_opts(), &backends(repo)?)?.init(
        &Credentials::password(PASSWORD),
        &KeyOptions::default(),
        &ConfigOptions::default(),
    )?;
    Ok(())
}

/// Stands in for one machine: opens the repo from scratch, like a daemon
/// waking up on a change feed notification would.
fn open(repo: &str) -> Result<Repository<rustic_core::OpenStatus>> {
    Ok(Repository::new(&repo_opts(), &backends(repo)?)?.open(&Credentials::password(PASSWORD))?)
}

/// Push: snapshot `dir` as "/sync", using `parent` to skip unchanged files.
fn push(repo: &str, dir: &Path, host: &str, parent: Option<&str>) -> Result<SnapshotFile> {
    let repo = open(repo)?.to_indexed_ids()?;
    let mut opts = BackupOptions::default().as_path(Some(PathBuf::from("/sync")));
    if let Some(p) = parent {
        opts.parent_opts = opts.parent_opts.parents(vec![p.to_string()]);
    }
    let snap = SnapshotOptions::default().host(Some(host.to_string())).to_snapshot()?;
    let source = PathList::from_string(dir.to_str().unwrap())?.sanitize()?;
    Ok(repo.backup(&opts, &source, snap)?)
}

/// Pull: make `dir` match the latest snapshot, deleting extras.
fn pull(repo: &str, dir: &Path) -> Result<(SnapshotFile, u64)> {
    let repo = open(repo)?.to_indexed()?;
    let snap = repo.get_snapshot_from_str("latest", |_| true)?;
    let node = repo.node_from_snapshot_and_path(&snap, "sync")?;
    let ls_opts = LsOptions::default().recursive(true);
    let dest = LocalDestination::new(dir.to_str().unwrap(), true, false)?;
    let opts = RestoreOptions::default().delete(true);
    let plan = repo.prepare_restore(&opts, repo.ls(&node, &ls_opts)?, &dest, false)?;
    let restored = plan.stats.files.restore + plan.stats.files.modify;
    repo.restore(plan, &opts, repo.ls(&node, &ls_opts)?, &dest)?;
    Ok((snap, restored))
}

fn make_tree(dir: &Path, files: usize) -> Result<()> {
    for i in 0..files {
        let sub = dir.join(format!("d{:03}", i % 100));
        fs::create_dir_all(&sub)?;
        let body = format!("file {i}\n").repeat(1 + i % 200);
        fs::write(sub.join(format!("f{i:06}.txt")), body)?;
    }
    Ok(())
}

fn tree_digest(dir: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d)? {
            let e = e?;
            if e.file_type()?.is_dir() {
                stack.push(e.path());
            } else {
                out.insert(e.path().strip_prefix(dir)?.to_path_buf(), fs::read(e.path())?);
            }
        }
    }
    Ok(out)
}

fn count_files(repo: &str) -> Result<String> {
    let infos = open(repo)?.infos_files()?;
    Ok(infos
        .repo
        .iter()
        .map(|i| format!("{:?}={}", i.tpe, i.count))
        .collect::<Vec<_>>()
        .join(" "))
}

fn ms(d: Duration) -> String {
    format!("{:>6.0}ms", d.as_secs_f64() * 1000.0)
}

fn live(repo: &str, work: &Path, files: usize, rounds: usize) -> Result<()> {
    let (a, b) = (work.join("a"), work.join("b"));
    fs::create_dir_all(&a)?;
    fs::create_dir_all(&b)?;
    init(repo)?;
    make_tree(&a, files)?;

    let t = Instant::now();
    let mut last = push(repo, &a, "machine-a", None)?;
    let t_push = t.elapsed();
    let t = Instant::now();
    pull(repo, &b)?;
    println!(
        "initial: {files} files, push {} pull {}",
        ms(t_push),
        ms(t.elapsed())
    );
    if tree_digest(&a)? != tree_digest(&b)? {
        bail!("initial restore differs");
    }

    let mut push_total = Duration::ZERO;
    let mut pull_total = Duration::ZERO;
    for round in 1..=rounds {
        // one small edit per round, the common live sync case
        let f = a.join(format!("d{:03}/f{:06}.txt", round % 100, round % files));
        fs::write(&f, format!("edited in round {round}\n"))?;
        if round % 10 == 0 {
            fs::write(a.join(format!("new-{round}.txt")), "new\n")?;
        }
        if round % 20 == 0 {
            fs::remove_file(a.join(format!("new-{}.txt", round - 10)))?;
        }

        let t = Instant::now();
        last = push(repo, &a, "machine-a", Some(&last.id.to_string()))?;
        let dp = t.elapsed();
        let t = Instant::now();
        let (_, restored) = pull(repo, &b)?;
        let dl = t.elapsed();
        push_total += dp;
        pull_total += dl;

        if round == 1 || round % (rounds / 10).max(1) == 0 {
            if tree_digest(&a)? != tree_digest(&b)? {
                bail!("round {round}: b differs from a");
            }
            // where the time goes: key derivation on open, then index load
            let t = Instant::now();
            let r = open(repo)?;
            let d_open = t.elapsed();
            let t = Instant::now();
            let _ = r.to_indexed()?;
            let d_index = t.elapsed();
            println!(
                "round {round:>5}: push {} pull {} ({restored} files) [open {} index {}] | {}",
                ms(dp),
                ms(dl),
                ms(d_open),
                ms(d_index),
                count_files(repo)?
            );
        }
    }
    println!(
        "avg over {rounds} rounds: push {} pull {}",
        ms(push_total / rounds as u32),
        ms(pull_total / rounds as u32)
    );
    check(repo)
}

/// Two writers on one repo at the same moment, each syncing its own folder.
fn concurrent(repo: &str, work: &Path, rounds: usize) -> Result<()> {
    init(repo)?;
    let barrier = Barrier::new(2);
    thread::scope(|s| -> Result<()> {
        let handles: Vec<_> = ["x", "y"]
            .into_iter()
            .map(|who| {
                let dir = work.join(who);
                let barrier = &barrier;
                s.spawn(move || -> Result<()> {
                    fs::create_dir_all(&dir)?;
                    make_tree(&dir, 500)?;
                    let mut last: Option<String> = None;
                    for round in 0..rounds {
                        fs::write(dir.join(format!("{who}-{round}.bin")), vec![round as u8; 300_000])?;
                        barrier.wait(); // both backups start together
                        let snap = push(repo, &dir, who, last.as_deref())
                            .with_context(|| format!("{who} round {round}"))?;
                        last = Some(snap.id.to_string());
                    }
                    Ok(())
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap()?;
        }
        Ok(())
    })?;
    let snaps = open(repo)?.get_all_snapshots()?;
    println!("snapshots: {} (expected {})", snaps.len(), rounds * 2);
    if snaps.len() != rounds * 2 {
        bail!("lost snapshots");
    }
    check(repo)
}

fn check(repo: &str) -> Result<()> {
    let t = Instant::now();
    let res = open(repo)?.check(CheckOptions::default().read_data(true))?;
    for (level, err) in &res.0 {
        println!("check {level:?}: {err}");
    }
    res.is_ok()?;
    println!("check --read-data ok ({})", ms(t.elapsed()));
    Ok(())
}

/// Prune with deletes applied at once, then re-check.
fn prune(repo: &str) -> Result<()> {
    let r = open(repo)?;
    let opts = rustic_core::PruneOptions::default().instant_delete(true);
    let plan = r.prune_plan(&opts)?;
    let st = &plan.stats;
    println!(
        "prune plan: packs used {} partly {} unused {} -> keep {} repack {}",
        st.packs.used, st.packs.partly_used, st.packs.unused, st.packs.keep, st.packs.repack
    );
    r.prune(&opts, plan)?;
    check(repo)
}

fn rel_files(dir: &Path) -> Result<Vec<PathBuf>> {
    Ok(tree_digest(dir)?.into_keys().collect())
}

/// Spike 2 at scale: one small edit per round on a big folder.
fn scale(repo: &str, work: &Path, files: usize, rounds: usize) -> Result<()> {
    let (a, b) = (work.join("a"), work.join("b"));
    fs::create_dir_all(&a)?;
    fs::create_dir_all(&b)?;
    init(repo)?;
    let key = open(repo)?.key();
    let coord = sync::Coordinator::default();
    let mut ma = sync::Device::new("machine-a", &a, repo, key.clone(), &coord);
    let mut mb = sync::Device::new("machine-b", &b, repo, key, &coord);
    make_tree(&a, files)?;

    let t = Instant::now();
    ma.sync(rel_files(&a)?)?;
    let t_push = t.elapsed();
    let t = Instant::now();
    mb.sync([])?;
    println!("initial: {files} files, push {} pull {}", ms(t_push), ms(t.elapsed()));
    if tree_digest(&a)? != tree_digest(&b)? {
        bail!("initial pull differs");
    }

    let (mut push_total, mut pull_total) = (Duration::ZERO, Duration::ZERO);
    for round in 1..=rounds {
        let mut changed = vec![PathBuf::from(format!("d{:03}/f{:06}.txt", round % 100, round % files))];
        fs::write(a.join(&changed[0]), format!("edited in round {round}\n"))?;
        if round % 10 == 0 {
            changed.push(PathBuf::from(format!("new/n{round}.txt")));
            fs::create_dir_all(a.join("new"))?;
            fs::write(a.join(changed.last().unwrap()), "new\n")?;
        }
        if round % 20 == 0 {
            changed.push(PathBuf::from(format!("new/n{}.txt", round - 10)));
            fs::remove_file(a.join(changed.last().unwrap()))?;
        }
        let t = Instant::now();
        let sp = ma.sync(changed)?;
        let dp = t.elapsed();
        let t = Instant::now();
        let sl = mb.sync([])?;
        let dl = t.elapsed();
        push_total += dp;
        pull_total += dl;
        if round == 1 || round % (rounds / 10).max(1) == 0 {
            if tree_digest(&a)? != tree_digest(&b)? {
                bail!("round {round}: b differs from a");
            }
            println!(
                "round {round:>5}: push {} ({} files) pull {} ({} files) | {}",
                ms(dp),
                sp.pushed,
                ms(dl),
                sl.pulled,
                count_files(repo)?
            );
        }
    }
    println!(
        "avg over {rounds} rounds: push {} pull {}",
        ms(push_total / rounds as u32),
        ms(pull_total / rounds as u32)
    );
    check(repo)
}

/// Spike 2 conflicts: two devices edit and delete the same few files at once,
/// each syncing every `every` edits. Pass: both converge, and whatever was on
/// disk when a sync started reached some snapshot (as the file or a conflict
/// copy). A device overwriting its own unsynced edit is not a loss.
fn conflicts(repo: &str, work: &Path, rounds: usize, paths: u64, every: usize) -> Result<()> {
    init(repo)?;
    let key = open(repo)?.key();
    let coord = sync::Coordinator::default();
    let mut devices: Vec<_> = ["x", "y"]
        .iter()
        .map(|n| {
            let dir = work.join(n);
            fs::create_dir_all(&dir).unwrap();
            sync::Device::new(n, &dir, repo, key.clone(), &coord)
        })
        .collect();

    let (written, stats) = thread::scope(|s| {
        let handles: Vec<_> = devices
            .iter_mut()
            .enumerate()
            .map(|(i, dev)| {
                s.spawn(move || -> Result<(Vec<String>, sync::Stats)> {
                    let mut rng: u64 = 0x9e37_79b9_7f4a_7c15 ^ (i as u64 + 1);
                    let mut next = |m: u64| {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        rng % m
                    };
                    let mut written = Vec::new();
                    let mut total = sync::Stats::default();
                    let mut on_disk: BTreeMap<PathBuf, String> = BTreeMap::new();
                    let mut changed = Vec::new();
                    for round in 0..rounds {
                        let n = next(paths);
                        let path = PathBuf::from(format!("d{}/f{}.txt", n % 3, n / 3));
                        let full = dev.dir.join(&path);
                        if next(100) < 80 {
                            let body = format!("{} round {round} {}\n", dev.name, path.display());
                            fs::create_dir_all(full.parent().unwrap())?;
                            fs::write(&full, &body)?;
                            _ = on_disk.insert(path.clone(), body);
                        } else {
                            _ = fs::remove_file(&full);
                            _ = on_disk.remove(&path);
                        }
                        changed.push(path);
                        if round % every == every - 1 || round == rounds - 1 {
                            written.extend(std::mem::take(&mut on_disk).into_values());
                            let st = dev.sync(std::mem::take(&mut changed))?;
                            total.pushed += st.pushed;
                            total.pulled += st.pulled;
                            total.conflicts += st.conflicts;
                            total.retries += st.retries;
                        }
                    }
                    Ok((written, total))
                })
            })
            .collect();
        let mut written = BTreeSet::new();
        let mut stats = Vec::new();
        for h in handles {
            let (w, st) = h.join().unwrap()?;
            written.extend(w);
            stats.push(st);
        }
        Ok::<_, anyhow::Error>((written, stats))
    })?;
    for (d, st) in devices.iter().zip(&stats) {
        println!("{}: {st:?}", d.name);
    }

    // quiesce: everyone catches up to the head
    for _ in 0..2 {
        for d in &mut devices {
            d.sync([])?;
        }
    }
    let (x, y) = (tree_digest(&devices[0].dir)?, tree_digest(&devices[1].dir)?);
    if x != y {
        bail!("devices did not converge");
    }
    let copies = x.keys().filter(|p| p.to_string_lossy().contains(".sync-conflict-")).count();
    println!("converged: {} files, {copies} conflict copies", x.len());

    // every write must be in some snapshot
    let r = open(repo)?.to_indexed()?;
    let mut seen = BTreeSet::new();
    let mut stack: Vec<_> = r.get_all_snapshots()?.into_iter().map(|s| s.tree).collect();
    let mut visited = BTreeSet::new();
    while let Some(id) = stack.pop() {
        if !visited.insert(id) {
            continue;
        }
        for node in r.get_tree(&id)?.nodes {
            if let Some(sub) = node.subtree {
                stack.push(sub);
            } else if node.is_file() {
                let mut buf = Vec::new();
                r.dump(&node, &mut buf)?;
                _ = seen.insert(String::from_utf8(buf)?);
            }
        }
    }
    let lost: Vec<_> = written.difference(&seen).collect();
    println!("writes: {}, found in snapshots: {}, lost: {}", written.len(), written.len() - lost.len(), lost.len());
    for l in lost.iter().take(5) {
        print!("  lost: {l}");
    }
    if !lost.is_empty() {
        bail!("lost writes");
    }
    check(repo)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize, d: usize| args.get(i).map_or(Ok(d), |s| s.parse());
    match args.get(1).map(String::as_str) {
        Some("live") => live(&args[2], Path::new(&args[3]), arg(4, 2000)?, arg(5, 100)?),
        Some("concurrent") => concurrent(&args[2], Path::new(&args[3]), arg(4, 20)?),
        Some("check") => check(&args[2]),
        Some("prune") => prune(&args[2]),
        Some("scale") => scale(&args[2], Path::new(&args[3]), arg(4, 50_000)?, arg(5, 50)?),
        Some("conflicts") => conflicts(&args[2], Path::new(&args[3]), arg(4, 100)?, arg(5, 21)? as u64, arg(6, 1)?),
        _ => bail!("usage: rustic-spike live|concurrent|check <repo> [work] ..."),
    }
}
