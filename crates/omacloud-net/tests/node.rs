//! Holders and an account's computer on local endpoints, no relays.

use anyhow::Result;
use blind_rsa_signatures::{DefaultRng, KeyPairSha384PSSRandomized};
use iroh::{Endpoint, RelayMode, endpoint::presets};
use omacloud_net::{
    client, credit,
    node::{self, Config, Node},
    proto::BlobId,
};

async fn endpoint() -> Result<Endpoint> {
    Ok(Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await?)
}

async fn holder(dir: &std::path::Path, capacity: u64, issuer: Option<credit::IssuerKey>) -> Result<Node> {
    Node::spawn(
        endpoint().await?,
        Config {
            dir: dir.to_path_buf(),
            capacity,
            issuer,
        },
    )
    .await
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blob_stored_is_read_back_whole_and_in_verified_ranges() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let n = holder(tmp.path(), 10 << 20, None).await?;
    let me = endpoint().await?;
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let blob = client::store(&me, &n.addr(), &data).await?;
    assert_eq!(blob.0, *blake3::hash(&data).as_bytes());
    // storing again is a no-op
    client::store(&me, &n.addr(), &data).await?;
    assert_eq!(n.used()?, data.len() as u64);

    assert_eq!(client::read(&me, &n.addr(), blob, None).await?, data);
    let part = client::read(&me, &n.addr(), blob, Some(100_000..100_100)).await?;
    assert_eq!(part, &data[100_000..100_100]);

    let other = BlobId([7; 32]);
    assert_eq!(client::has(&me, &n.addr(), vec![blob, other]).await?, vec![true, false]);
    assert!(client::read(&me, &n.addr(), other, Some(0..10)).await.is_err());
    n.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_holder_refuses_and_an_unpaid_blob_goes_when_its_lease_ends() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let n = holder(tmp.path(), 1000, None).await?;
    let me = endpoint().await?;
    let blob = client::store(&me, &n.addr(), &[1u8; 600]).await?;
    let e = client::store(&me, &n.addr(), &[2u8; 600]).await.unwrap_err();
    assert!(format!("{e:#}").contains("full"), "{e:#}");

    assert_eq!(n.sweep_as_of(now()).await?, 0);
    assert_eq!(n.sweep_as_of(now() + node::FIRST_LEASE + 60).await?, 1);
    assert_eq!(client::has(&me, &n.addr(), vec![blob]).await?, vec![false]);
    // room again
    client::store(&me, &n.addr(), &[2u8; 600]).await?;
    n.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_renewal_pays_in_tokens_and_a_paid_blob_gets_its_grace() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let kp = KeyPairSha384PSSRandomized::generate(&mut DefaultRng, 2048)?;
    let n = holder(tmp.path(), 10 << 20, Some(kp.pk.clone())).await?;
    let me = endpoint().await?;
    let blob = client::store(&me, &n.addr(), &[3u8; 1000]).await?;
    let until = now() + 2 * node::MONTH;

    // two months of under a GiB: two tokens
    let (blind, pending) = credit::ask(&kp.pk, 2)?;
    let signed: Vec<Vec<u8>> = blind.iter().map(|b| kp.sk.blind_sign(b).unwrap().0).collect();
    let tokens = pending.finish(&kp.pk, &signed)?;
    let e = client::renew(&me, &n.addr(), vec![blob], until, tokens[..1].to_vec())
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("2 tokens owed"), "{e:#}");
    let kept = client::renew(&me, &n.addr(), vec![blob, BlobId([9; 32])], until, tokens.clone()).await?;
    assert_eq!(kept, vec![true, false]);
    assert_eq!(n.tokens()?.len(), 2);
    // the same tokens again buy nothing
    let e = client::renew(&me, &n.addr(), vec![blob], until, tokens)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("0 good ones"), "{e:#}");

    assert_eq!(n.sweep_as_of(until + 60).await?, 0, "kept through its grace");
    assert_eq!(n.sweep_as_of(until + node::GRACE + 60).await?, 1);
    n.shutdown().await;
    Ok(())
}
