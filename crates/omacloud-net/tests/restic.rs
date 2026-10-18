//! Two computers of one account syncing a folder through the network: the
//! sync engine and rustic, unchanged, on the gateway's S3.

use std::{collections::BTreeSet, fs, sync::Arc};

use anyhow::Result;
use ed25519_dalek::SigningKey;
use iroh::{Endpoint, RelayMode, endpoint::presets};
use omacloud_core::{Coordinator, Engine, RepoSpec, Setup, account};
use omacloud_net::{
    coord::{Account, NetworkCoordinator},
    gateway::{BUCKET, Gateway},
    node::{Config, Node},
    place,
    records::MemRecords,
    registry::{Listed, MemRegistry},
};

async fn endpoint() -> Result<Endpoint> {
    Ok(Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await?)
}

#[test]
fn two_computers_sync_through_the_network() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let rt = tokio::runtime::Runtime::new()?;
    let (nodes, gateway) = rt.block_on(async {
        let registry = Arc::new(MemRegistry::default());
        let mut nodes = Vec::new();
        for i in 0..3u8 {
            let node = Node::spawn(
                endpoint().await?,
                Config {
                    dir: tmp.path().join(format!("node{i}")),
                    capacity: 1 << 30,
                    issuer: None,
                },
            )
            .await?;
            registry.add_node(Listed {
                node: place::Node {
                    id: node.id(),
                    asn: 64500 + u32::from(i),
                    prefix: format!("192.0.{i}.0/24"),
                    uptime: 0.99,
                    probation: false,
                },
                addr: node.addr(),
            });
            nodes.push(node);
        }
        let gateway = Gateway::start(endpoint().await?, registry, BTreeSet::new(), 2).await?;
        anyhow::Ok((nodes, gateway))
    })?;

    let repo = RepoSpec {
        repository: "opendal:s3".into(),
        options: [
            ("endpoint", gateway.endpoint()),
            ("bucket", BUCKET.to_string()),
            ("region", "auto".to_string()),
            ("root", "/account".to_string()),
            // the key comes from each epoch's password
            ("network", "1".to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
    };
    // coordination at the "Worker", sealed: a made the account; b joins
    // with the recovery code, which gives the same account keys
    let records = Arc::new(MemRecords::default());
    let (a, b) = (
        SigningKey::generate(&mut rand::rngs::OsRng),
        SigningKey::generate(&mut rand::rngs::OsRng),
    );
    let seed: [u8; 32] = rand::random();
    let provisional = hex::encode(rand::random::<[u8; 32]>());
    let coord_a = Arc::new(NetworkCoordinator::new(
        records.clone(),
        Account::from_seed(seed),
        Some(provisional.clone()),
    ));
    assert!(coord_a.create(&provisional)?);
    // creating it moves writes to the epoch's capability
    let created = account::create(coord_a.as_ref(), &repo, &a, "a")?;
    // b has nothing but the recovery code: the root opens the epoch's key,
    // which gives the capability to write
    let coord_b = Arc::new(NetworkCoordinator::new(records.clone(), Account::from_seed(seed), None));
    account::join_with_recovery(coord_b.as_ref(), &created.recovery_code, &b, "b")?;
    // the old capability writes nothing now
    let stale = NetworkCoordinator::new(records.clone(), Account::from_seed(seed), Some(provisional));
    assert!(stale.remove_request("x").is_err());
    // the account's computers learn each other's nodes, which then hold
    // none of its copies
    coord_a.publish_node(&hex::encode(a.verifying_key().as_bytes()), &[7; 32])?;
    assert!(coord_b.account_nodes()?.contains(&[7; 32]));
    // and another account's keys open nothing
    let outsider = NetworkCoordinator::new(records.clone(), Account::from_seed([9; 32]), None);
    assert!(outsider.anchor()?.is_none());
    let engine = |name: &str, key: &SigningKey, coord: Arc<NetworkCoordinator>| {
        let folder = tmp.path().join(name);
        fs::create_dir_all(&folder)?;
        Engine::new(
            Setup {
                device: name.to_string(),
                folder,
                repo: repo.clone(),
                signing: key.clone(),
                root: created.root.clone(),
                state_path: Some(tmp.path().join(format!("{name}.state.json"))),
                settings: None,
                folders: None,
            },
            coord,
        )
    };
    let (mut ea, mut eb) = (engine("a", &a, coord_a)?, engine("b", &b, coord_b)?);

    let big: Vec<u8> = (0..3_000_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    fs::write(tmp.path().join("a/note.txt"), "hello from a")?;
    fs::create_dir_all(tmp.path().join("a/photos"))?;
    fs::write(tmp.path().join("a/photos/big.bin"), &big)?;
    assert!(ea.sync()?.pushed > 0);
    eb.sync()?;
    assert_eq!(fs::read_to_string(tmp.path().join("b/note.txt"))?, "hello from a");
    assert_eq!(fs::read(tmp.path().join("b/photos/big.bin"))?, big);

    // and back, with a delete
    fs::write(tmp.path().join("b/note.txt"), "edited on b")?;
    fs::remove_file(tmp.path().join("b/photos/big.bin"))?;
    // as the daemon's watcher would tell it
    eb.request_rescan();
    assert!(eb.sync()?.pushed > 0);
    assert!(ea.sync()?.pulled > 0);
    assert_eq!(fs::read_to_string(tmp.path().join("a/note.txt"))?, "edited on b");
    assert!(!tmp.path().join("a/photos/big.bin").exists());

    // a new epoch: new repository, new network key and scope, and writes
    // move to its capability; b follows it
    let rotated = ea.rotate()?;
    assert_eq!(rotated.epoch, 1);
    fs::write(tmp.path().join("a/after.txt"), "epoch one")?;
    ea.request_rescan();
    ea.sync()?;
    eb.sync()?;
    assert_eq!(fs::read_to_string(tmp.path().join("b/after.txt"))?, "epoch one");
    fs::write(tmp.path().join("b/back.txt"), "from b in epoch one")?;
    eb.request_rescan();
    assert!(eb.sync()?.pushed > 0);
    ea.sync()?;
    assert_eq!(
        fs::read_to_string(tmp.path().join("a/back.txt"))?,
        "from b in epoch one"
    );

    drop(gateway);
    rt.block_on(async {
        for n in nodes {
            n.shutdown().await;
        }
    });
    Ok(())
}
