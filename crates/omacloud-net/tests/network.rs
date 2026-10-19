//! A small network on this machine: holders on local endpoints, the
//! in-memory registry, and a gateway an S3 client talks to.

use std::{collections::BTreeSet, sync::Arc};

use anyhow::Result;
use iroh::{Endpoint, RelayMode, endpoint::presets};
use omacloud_net::{
    bucket::Bucket,
    gateway::{BUCKET, Gateway},
    node::{Config, Node},
    place,
    registry::{Listed, MemRegistry},
    seal::Keys,
};
use opendal::{Operator, services::S3};

async fn endpoint() -> Result<Endpoint> {
    Ok(Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await?)
}

struct Net {
    _tmp: tempfile::TempDir,
    nodes: Vec<Node>,
    registry: Arc<MemRegistry>,
}

async fn network(n: u8) -> Result<Net> {
    let tmp = tempfile::tempdir()?;
    let registry = Arc::new(MemRegistry::default());
    let mut nodes = Vec::new();
    for i in 0..n {
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
    Ok(Net {
        _tmp: tmp,
        nodes,
        registry,
    })
}

async fn bucket(net: &Net, password: &str) -> Result<Arc<Bucket>> {
    Ok(Arc::new(Bucket::new(
        endpoint().await?,
        net.registry.clone(),
        Keys::from_secret(password),
        Arc::default(),
        2,
    )))
}

/// An epoch's network key, as the sync engine makes it.
const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn s3(g: &Gateway, key: &str, root: &str) -> Result<Operator> {
    opendal::install_default();
    let builder = S3::default()
        .endpoint(&g.endpoint())
        .bucket(BUCKET)
        .region("auto")
        .access_key_id(key)
        .secret_access_key("network")
        .root(root);
    Ok(Operator::new(builder)?)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_s3_client_sees_a_bucket() -> Result<()> {
    let net = network(4).await?;
    let g = Gateway::start(endpoint().await?, net.registry.clone(), BTreeSet::new(), 2).await?;
    let b = g.bucket(KEY);
    let op = s3(&g, KEY, "/acct/e0")?;

    let big: Vec<u8> = (0..1_500_000u32).map(|i| (i % 253) as u8).collect();
    op.write("data/ab/abcdef", big.clone()).await?;
    op.write("config", b"cfg".to_vec()).await?;
    op.write("keys/k1", b"key".to_vec()).await?;
    op.create_dir("data/00/").await?;

    assert_eq!(op.read("data/ab/abcdef").await?.to_vec(), big);
    assert_eq!(
        op.read_with("data/ab/abcdef").range(1000..1010).await?.to_vec(),
        &big[1000..1010]
    );
    assert_eq!(op.stat("config").await?.content_length(), 3);
    assert!(op.stat("nothing").await.is_err());

    let mut listed: Vec<String> = op
        .list_with("")
        .recursive(true)
        .await?
        .into_iter()
        .map(|e| e.path().to_string())
        .collect();
    listed.sort();
    assert!(listed.contains(&"data/ab/abcdef".to_string()), "{listed:?}");
    assert!(listed.contains(&"keys/k1".to_string()), "{listed:?}");

    // each object has two copies, on different holders, scrambled apart
    let entry = b.entry("acct/e0/data/ab/abcdef").await?.expect("indexed");
    assert_eq!(entry.holders.len(), 2);
    assert_ne!(entry.holders[0].node, entry.holders[1].node);
    assert_ne!(entry.holders[0].blob, entry.holders[1].blob);

    op.delete("keys/k1").await?;
    assert!(op.stat("keys/k1").await.is_err());
    op.delete_iter(["config".to_string()]).await?;
    assert!(op.stat("config").await.is_err());

    // another key reaches a bucket of its own, empty
    let other = s3(&g, &KEY.replace('0', "f"), "/acct/e0")?;
    assert!(other.read("data/ab/abcdef").await.is_err());
    assert!(other.list_with("").recursive(true).await?.is_empty());
    // and no key, nothing
    assert!(s3(&g, "short", "/acct/e0")?.read("data/ab/abcdef").await.is_err());
    for n in net.nodes {
        n.shutdown().await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_holder_leaves_the_other_copy_and_another_account_sees_nothing() -> Result<()> {
    let net = network(3).await?;
    let b = bucket(&net, "pw").await?;
    b.put("data/aa/1", b"one").await?;
    let entry = b.entry("data/aa/1").await?.expect("indexed");
    // one holder goes
    net.registry.remove_node(&entry.holders[0].node);
    assert_eq!(b.get("data/aa/1", None).await?.as_deref(), Some(&b"one"[..]));

    // another account (or epoch) shares the Worker and sees none of it
    let other = bucket(&net, "another").await?;
    assert!(other.get("data/aa/1", None).await?.is_none());
    assert!(other.list("").await?.is_empty());
    for n in net.nodes {
        n.shutdown().await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn upkeep_replaces_a_holder_that_left_and_one_that_dropped_its_copy() -> Result<()> {
    use omacloud_net::upkeep;
    let net = network(5).await?;
    let b = bucket(&net, "pw").await?;
    for i in 0..4 {
        b.put(&format!("data/aa/{i}"), format!("object {i}").as_bytes()).await?;
    }
    let mut free = |_| Ok(Vec::new());

    // nothing wrong: nothing done
    let r = upkeep::round(&b, 100, None, &mut free).await?;
    assert_eq!((r.failed, r.repaired), (0, 0), "{r:?}");
    assert_eq!(r.audited, 8);
    // 32 bytes, and a token a month for each holder, however little it keeps
    assert_eq!(r.stored, 32);
    let mut holders = std::collections::BTreeSet::new();
    for i in 0..4 {
        let e = b.entry(&format!("data/aa/{i}")).await?.expect("indexed");
        holders.extend(e.holders.iter().map(|h| h.node));
    }
    assert_eq!(r.monthly, holders.len() as u64);

    // one holder leaves the network
    let e0 = b.entry("data/aa/0").await?.expect("indexed");
    let left = e0.holders[0].node;
    net.registry.remove_node(&left);
    // another silently drops everything it holds
    let e1 = b.entry("data/aa/1").await?.expect("indexed");
    let dropper = e1
        .holders
        .iter()
        .map(|h| h.node)
        .find(|n| *n != left)
        .expect("another holder");
    let n = net.nodes.iter().find(|n| n.id() == dropper).expect("node");
    n.sweep_as_of(u64::MAX / 2).await?;

    let r = upkeep::round(&b, 100, Some(omacloud_net::node::now() + 3600), &mut free).await?;
    assert!(r.repaired >= 2, "{r:?}");
    assert!(r.lost.is_empty(), "{r:?}");
    for i in 0..4 {
        let key = format!("data/aa/{i}");
        let e = b.entry(&key).await?.expect("indexed");
        assert_eq!(e.holders.len(), 2, "{key}");
        assert!(e.holders.iter().all(|h| h.node != left && h.node != dropper), "{key}");
        assert_eq!(b.get(&key, None).await?, Some(format!("object {i}").into_bytes()));
    }
    for n in net.nodes {
        n.shutdown().await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shared_file_opens_with_its_secret_only() -> Result<()> {
    use omacloud_net::share;
    let net = network(3).await?;
    let secret = share::secret();
    let sharer = bucket(&net, &share::bucket_key(&secret)).await?;
    share::put(&sharer, &secret, "photo.jpg", b"jpeg bytes").await?;

    // anyone with the secret, from another computer
    let other = bucket(&net, &share::bucket_key(&secret)).await?;
    assert_eq!(
        share::get(&other, &secret).await?,
        ("photo.jpg".to_string(), b"jpeg bytes".to_vec())
    );
    // another secret finds nothing
    let wrong = share::secret();
    let stranger = bucket(&net, &share::bucket_key(&wrong)).await?;
    assert!(share::get(&stranger, &wrong).await.is_err());
    for n in net.nodes {
        n.shutdown().await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_large_object_is_coded_and_survives_two_holders_going() -> Result<()> {
    use omacloud_net::upkeep;
    let mut net = network(9).await?;
    let b = bucket(&net, "pw").await?;
    let mut nodes = std::mem::take(&mut net.nodes);
    let object: Vec<u8> = (0..(3u32 << 20)).map(|i| (i % 253) as u8).collect();
    b.put("data/bb/big", &object).await?;
    let e = b.entry("data/bb/big").await?.expect("indexed");
    let coded = e.coded.expect("coded");
    assert_eq!(e.holders.len(), 6);
    assert_eq!(e.blob_size(), coded.shard);
    let shards: Vec<_> = e.holders.iter().map(|h| h.shard).collect();
    assert_eq!(shards, (0..6).map(Some).collect::<Vec<_>>());

    // a range across two data shards, read from them alone
    let r = coded.shard - 10..coded.shard + 10;
    let at = |r: &std::ops::Range<u64>| object[r.start as usize..r.end as usize].to_vec();
    assert_eq!(b.get("data/bb/big", Some(r.clone())).await?, Some(at(&r)));

    // two holders stop, one with a data shard the range needs
    let mut stop = async |id: [u8; 32]| {
        let i = nodes.iter().position(|n| n.id() == id).expect("node");
        nodes.swap_remove(i).shutdown().await;
    };
    stop(e.holders[0].node).await;
    stop(e.holders[5].node).await;
    assert_eq!(b.get("data/bb/big", None).await?, Some(object.clone()));
    assert_eq!(b.get("data/bb/big", Some(r.clone())).await?, Some(at(&r)));

    // upkeep finds them gone and gives their shards new holders
    let mut free = |_| Ok(Vec::new());
    let report = upkeep::round(&b, 6, None, &mut free).await?;
    assert_eq!(report.repaired, 1, "{report:?}");
    let after = b.entry("data/bb/big").await?.expect("indexed");
    assert_eq!(after.holders.len(), 6);
    assert!(
        after
            .holders
            .iter()
            .all(|h| h.node != e.holders[0].node && h.node != e.holders[5].node)
    );
    assert_eq!(b.get("data/bb/big", None).await?, Some(object.clone()));

    // with three more gone, the shards left can't rebuild it
    for h in &after.holders[..3] {
        stop(h.node).await;
    }
    assert!(b.get("data/bb/big", None).await.is_err());
    for n in nodes {
        n.shutdown().await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn on_a_small_network_a_large_object_is_copied_whole() -> Result<()> {
    let net = network(3).await?;
    let b = bucket(&net, "pw").await?;
    let object = vec![7u8; 2 << 20];
    b.put("data/cc/big", &object).await?;
    let e = b.entry("data/cc/big").await?.expect("indexed");
    assert!(e.coded.is_none());
    assert_eq!(e.holders.len(), 2);
    assert_eq!(b.get("data/cc/big", Some(5..9)).await?, Some(vec![7u8; 4]));
    for n in net.nodes {
        n.shutdown().await;
    }
    Ok(())
}
