//! The web reader against a repository the engine wrote: an account's files,
//! found and read with nothing but the stored bytes and the password
//! `omacloud export` gives.

use std::{collections::BTreeMap, fs, path::Path, sync::Arc};

use ed25519_dalek::SigningKey;
use omacloud_core::{DirCoordinator, Engine, RepoSpec, Setup, account};
use omacloud_web::Reader;
use serde_json::Value;

/// Every file under `dir`, by its path there.
fn stored(dir: &Path, under: &str) -> Vec<Vec<u8>> {
    fn go(dir: &Path, out: &mut Vec<Vec<u8>>) {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            if e.path().is_dir() {
                go(&e.path(), out);
            } else {
                out.push(fs::read(e.path()).unwrap());
            }
        }
    }
    let mut out = Vec::new();
    go(&dir.join(under), &mut out);
    out
}

/// A blob, fetched as the page fetches it: that range of its pack.
fn blob(reader: &Reader, repo: &Path, id: &str) -> Vec<u8> {
    let at = reader.locate(id).expect("indexed");
    let pack = fs::read(repo.join("data").join(&at.pack[..2]).join(&at.pack)).unwrap();
    let (from, to) = (at.offset as usize, (at.offset + at.length) as usize);
    reader.blob(id, &pack[from..to]).unwrap()
}

fn nodes(reader: &Reader, repo: &Path, tree: &str) -> Vec<Value> {
    let tree: Value = serde_json::from_slice(&blob(reader, repo, tree)).unwrap();
    tree["nodes"].as_array().unwrap().clone()
}

#[test]
fn reads_what_the_engine_wrote() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let spec = RepoSpec {
        repository: repo.to_string_lossy().into_owned(),
        options: BTreeMap::new(),
    };
    let coord = Arc::new(DirCoordinator::new(tmp.path().join("coord")).unwrap());
    let signing = SigningKey::generate(&mut rand::rngs::OsRng);
    let created = account::create(coord.as_ref(), &spec, &signing, "a").unwrap();
    let folder = tmp.path().join("a");
    fs::create_dir_all(folder.join("Notes")).unwrap();
    fs::write(folder.join("Notes/song.txt"), "la la").unwrap();
    // large and repetitive: several blobs, compressed
    let big: Vec<u8> = (0..6_000_000u32).flat_map(|i| (i / 7).to_le_bytes()).collect();
    fs::write(folder.join("big.bin"), &big).unwrap();
    let mut engine = Engine::new(
        Setup {
            device: "a".into(),
            folder,
            repo: spec,
            signing,
            root: created.root,
            state_path: None,
            settings: None,
            folders: None,
        },
        coord,
    )
    .unwrap();
    _ = engine.sync().unwrap();
    let (_, secret, _) = engine.repository().unwrap();

    let key = &stored(&repo, "keys")[0];
    assert!(Reader::open("not the password", key).is_err());
    let mut reader = Reader::open(&secret.password, key).unwrap();
    let config: Value = serde_json::from_slice(&reader.file(&fs::read(repo.join("config")).unwrap()).unwrap()).unwrap();
    assert_eq!(config["version"], 2);
    for index in stored(&repo, "index") {
        assert!(reader.add_index(&index).unwrap() > 0);
    }
    // the newest snapshot is the account as it stands
    let mut snapshots: Vec<Value> = stored(&repo, "snapshots")
        .iter()
        .map(|s| serde_json::from_slice(&reader.file(s).unwrap()).unwrap())
        .collect();
    snapshots.sort_by_key(|s| s["time"].as_str().unwrap().to_string());
    let root = snapshots.last().unwrap()["tree"].as_str().unwrap().to_string();

    let top = nodes(&reader, &repo, &root);
    let find = |nodes: &[Value], name: &str| nodes.iter().find(|n| n["name"] == name).cloned().expect(name);
    let notes = find(&top, "Notes");
    assert_eq!(notes["type"], "dir");
    let song = find(&nodes(&reader, &repo, notes["subtree"].as_str().unwrap()), "song.txt");
    let content: Vec<u8> = song["content"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|id| blob(&reader, &repo, id.as_str().unwrap()))
        .collect();
    assert_eq!(content, b"la la");

    let file = find(&top, "big.bin");
    let ids = file["content"].as_array().unwrap();
    assert!(ids.len() > 1, "one blob only");
    let content: Vec<u8> = ids
        .iter()
        .flat_map(|id| blob(&reader, &repo, id.as_str().unwrap()))
        .collect();
    assert!(content == big, "big.bin differs");

    // a blob's bytes under another's id are refused
    let (a, b) = (ids[0].as_str().unwrap(), ids[1].as_str().unwrap());
    let at = reader.locate(a).unwrap().clone();
    let pack = fs::read(repo.join("data").join(&at.pack[..2]).join(&at.pack)).unwrap();
    let bytes = &pack[at.offset as usize..(at.offset + at.length) as usize];
    assert!(reader.blob(b, bytes).is_err());

    // what a browser uploads, a computer takes: the engine vouches for a
    // session's key, the reader signs and seals with it, and the inbox's
    // checks pass with what both sides made
    let session = SigningKey::generate(&mut rand::rngs::OsRng);
    let session_public = hex::encode(session.verifying_key().to_bytes());
    let endorsement = serde_json::to_string(&engine.endorse_upload_key(&session_public, 2000)).unwrap();
    let sealed = omacloud_web::upload::make(
        &reader,
        &session.to_bytes(),
        &endorsement,
        "Notes/from the browser.txt",
        1_800_000_000_000,
        [9; 16],
        b"hello",
    )
    .unwrap();
    assert!(!sealed.windows(5).any(|w| w == b"hello"));
    let upload = omacloud_core::inbox::open(&reader.unseal(&sealed).unwrap(), 1999, |d| engine.is_member(d)).unwrap();
    assert_eq!(upload.path, Path::new("Notes/from the browser.txt"));
    assert_eq!(upload.content, b"hello");
    assert!(omacloud_core::inbox::open(&reader.unseal(&sealed).unwrap(), 2001, |d| engine.is_member(d)).is_err());
}
