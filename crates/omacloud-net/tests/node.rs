//! Holders and an account's computer on local endpoints, no relays.

use anyhow::Result;
use blind_rsa_signatures::{DefaultRng, KeyPairSha384PSSRandomized};
use iroh::{Endpoint, endpoint::presets};
use omacloud_net::{
    client, credit,
    node::{self, Config, Node, now},
    proto::BlobId,
};

mod common;
use common::endpoint;

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

#[tokio::test(flavor = "multi_thread")]
async fn a_blob_stored_is_read_back_whole_and_in_verified_ranges() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let n = holder(tmp.path(), 10 << 20, None).await?;
    let me = endpoint().await?;
    // past a stream's flow-control window
    let data: Vec<u8> = (0..4_000_000u32).map(|i| (i % 251) as u8).collect();
    let blob = client::store(&me, &n.addr(), &data).await?;
    assert_eq!(blob.0, *blake3::hash(&data).as_bytes());
    // storing again is a no-op: the holder answers without taking the body
    client::store(&me, &n.addr(), &data).await?;
    assert_eq!(n.used()?, data.len() as u64);

    assert_eq!(client::read(&me, &n.addr(), blob, None).await?, data);
    let part = client::read(&me, &n.addr(), blob, Some(100_000..100_100)).await?;
    assert_eq!(part, &data[100_000..100_100]);

    let other = BlobId([7; 32]);
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
    // no longer kept (its bytes are served until the store's next collection)
    assert_eq!(
        client::renew(&me, &n.addr(), vec![blob], now() + node::MONTH, Vec::new()).await?,
        client::Renewal::Kept(vec![false])
    );
    // room again
    client::store(&me, &n.addr(), &[2u8; 600]).await?;
    n.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_holder_is_paid_for_time_served_and_a_paid_blob_gets_its_grace() -> Result<()> {
    use client::Renewal;
    let tmp = tempfile::tempdir()?;
    let kp = KeyPairSha384PSSRandomized::generate(&mut DefaultRng, 2048)?;
    let n = holder(tmp.path(), 10 << 20, Some(kp.pk.clone())).await?;
    let me = endpoint().await?;
    let blob = client::store(&me, &n.addr(), &[3u8; 1000]).await?;
    let until = now() + 2 * node::MONTH;

    // in its first week nothing is owed: kept on credit
    assert_eq!(
        client::renew(&me, &n.addr(), vec![blob], until, Vec::new()).await?,
        Renewal::Kept(vec![true])
    );
    // 33 days on, a whole month unpaid: under a GiB, one token
    n.backdate(40 * 24 * 3600)?;
    assert_eq!(
        client::renew(&me, &n.addr(), vec![blob], until, Vec::new()).await?,
        Renewal::Owed(1)
    );
    let (blind, pending) = credit::ask(&kp.pk, 1)?;
    let signed: Vec<Vec<u8>> = blind.iter().map(|b| kp.sk.blind_sign(b).unwrap().0).collect();
    let tokens = pending.finish(&kp.pk, &signed)?;
    assert_eq!(
        client::renew(&me, &n.addr(), vec![blob, BlobId([9; 32])], until, tokens.clone()).await?,
        Renewal::Kept(vec![true, false])
    );
    assert_eq!(n.tokens()?.len(), 1);
    // paid for that month (the days since carry over): nothing more owed
    // now, and spent tokens buy nothing later
    assert_eq!(
        client::renew(&me, &n.addr(), vec![blob], until, Vec::new()).await?,
        Renewal::Kept(vec![true])
    );
    n.backdate(31 * 24 * 3600)?;
    assert_eq!(
        client::renew(&me, &n.addr(), vec![blob], until, tokens).await?,
        Renewal::Owed(1)
    );

    let until = until - 31 * 24 * 3600;
    assert_eq!(n.sweep_as_of(until + 60).await?, 0, "kept through its grace");
    assert_eq!(n.sweep_as_of(until + node::GRACE + 60).await?, 1);
    n.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn storing_a_blob_again_keeps_what_it_owes() -> Result<()> {
    use client::Renewal;
    let tmp = tempfile::tempdir()?;
    let kp = KeyPairSha384PSSRandomized::generate(&mut DefaultRng, 2048)?;
    let n = holder(tmp.path(), 10 << 20, Some(kp.pk.clone())).await?;
    let me = endpoint().await?;
    let blob = client::store(&me, &n.addr(), &[8u8; 1000]).await?;
    client::renew(&me, &n.addr(), vec![blob], now() + 2 * node::MONTH, Vec::new()).await?;
    // weeks on, the same copy is stored again (a repair, or someone who
    // knows its id): what it owes isn't wiped
    n.backdate(40 * 24 * 3600)?;
    client::store(&me, &n.addr(), &[8u8; 1000]).await?;
    assert!(matches!(
        client::renew(&me, &n.addr(), vec![blob], now() + 2 * node::MONTH, Vec::new()).await?,
        Renewal::Owed(_)
    ));
    // and once its lease ran out (unpaid, no grace), it's refused: the
    // copy goes to a holder where it lasts
    n.backdate(60 * 24 * 3600)?;
    let e = client::store(&me, &n.addr(), &[8u8; 1000]).await.unwrap_err();
    assert!(format!("{e:#}").contains("about to be let go"), "{e:#}");
    n.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blob_never_paid_for_gets_no_grace() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let kp = KeyPairSha384PSSRandomized::generate(&mut DefaultRng, 2048)?;
    let n = holder(tmp.path(), 10 << 20, Some(kp.pk.clone())).await?;
    let me = endpoint().await?;
    let blob = client::store(&me, &n.addr(), &[4u8; 1000]).await?;
    // renewed in its free week, nothing owed: kept on credit, but no grace,
    // or a lease could be lapsed and renewed for nothing, again and again
    let until = now() + 2 * node::MONTH;
    client::renew(&me, &n.addr(), vec![blob], until, Vec::new()).await?;
    assert_eq!(n.sweep_as_of(until - 60).await?, 0);
    assert_eq!(n.sweep_as_of(until + 60).await?, 1);
    n.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_computer_reaches_a_holder_through_a_relay_alone() -> Result<()> {
    use iroh::{RelayMode, tls::CaTlsConfig};
    let (map, _url, _server) = iroh::test_utils::run_relay_server().await?;
    let tmp = tempfile::tempdir()?;
    let holder_ep = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Custom(map.clone()))
        .ca_tls_config(CaTlsConfig::insecure_skip_verify())
        .bind()
        .await?;
    holder_ep.online().await;
    let n = Node::spawn(
        holder_ep,
        Config {
            dir: tmp.path().to_path_buf(),
            capacity: 1 << 20,
            issuer: None,
        },
    )
    .await?;
    // no IP transport at all: only the relay carries it
    let me = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Custom(map))
        .ca_tls_config(CaTlsConfig::insecure_skip_verify())
        .clear_ip_transports()
        .bind()
        .await?;
    me.online().await;
    let addr = n.addr();
    assert!(addr.relay_urls().next().is_some(), "{addr:?}");
    let blob = client::store(&me, &addr, b"through the relay").await?;
    assert_eq!(client::read(&me, &addr, blob, None).await?, b"through the relay");
    n.shutdown().await;
    Ok(())
}
