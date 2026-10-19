//! One file shared by link. The file is sealed under a random secret and
//! kept in a bucket of its own that the secret alone reaches: its holders
//! and the Worker see ciphertext under ids derived from it, and the secret
//! travels after the link's `#`, which browsers never send. It lasts its
//! first lease (7 days) unless its sharer's daemon renews it to the end
//! `share --days` set (up to 365).

use anyhow::{Context, Result};

use crate::{bucket::Bucket, seal::Keys};

/// The one object in a share's bucket.
const OBJECT: &str = "share";
/// What the sealed file is bound to.
const SEALED_FOR: &str = "omacloud share v1";
/// Larger files go another way: a share is held in memory whole, and on a
/// network too small for erasure coding it's kept as whole copies, which
/// holders take up to `MAX_OBJECT`: room is left for the name and the seal.
pub const MAX: u64 = crate::proto::MAX_OBJECT - (1 << 20);

/// A new share's secret: what the link carries.
#[must_use]
pub fn secret() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}

/// The access key of a share's bucket, apart from any account's.
#[must_use]
pub fn bucket_key(secret: &str) -> String {
    format!("share {secret}")
}

/// Keep `name` with `data` in `bucket` (a share's).
///
/// # Errors
///
/// If no holder takes it.
pub async fn put(bucket: &Bucket, secret: &str, name: &str, data: &[u8]) -> Result<()> {
    anyhow::ensure!(data.len() as u64 <= MAX, "a share holds up to {} MiB", MAX >> 20);
    anyhow::ensure!(name.len() <= 255, "a file name that long");
    let mut plain = Vec::with_capacity(2 + name.len() + data.len());
    plain.extend_from_slice(
        &u16::try_from(name.len())
            .context("a file name that long")?
            .to_be_bytes(),
    );
    plain.extend_from_slice(name.as_bytes());
    plain.extend_from_slice(data);
    let sealed = Keys::from_secret(&bucket_key(secret)).seal(SEALED_FOR, &plain);
    bucket.put(OBJECT, &sealed).await
}

/// The file shared under `secret`: its name and contents.
///
/// # Errors
///
/// If it's gone (expired), can't be read, or doesn't verify.
pub async fn get(bucket: &Bucket, secret: &str) -> Result<(String, Vec<u8>)> {
    let sealed = bucket
        .get(OBJECT, None)
        .await?
        .context("nothing is shared under this link: it may have expired or been taken back")?;
    let plain = Keys::from_secret(&bucket_key(secret))
        .open(SEALED_FOR, &sealed)
        .context("the shared file doesn't verify")?;
    let (len, rest) = plain.split_at_checked(2).context("a shared file without a name")?;
    let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
    let (name, data) = rest.split_at_checked(len).context("a shared file without a name")?;
    let name = std::str::from_utf8(name)?;
    // a bare file name, never a path
    anyhow::ensure!(
        !name.is_empty() && !name.contains('/') && name != "." && name != "..",
        "a shared file named {name:?}"
    );
    Ok((name.to_string(), data.to_vec()))
}

/// Take a share back: its index entry goes, so nobody finds its holders,
/// who drop it when its lease ends.
///
/// # Errors
///
/// If it's gone already, or the Worker can't be reached.
pub async fn remove(bucket: &Bucket) -> Result<()> {
    anyhow::ensure!(
        bucket.entry(OBJECT).await?.is_some(),
        "nothing is shared under this link: it may have expired or been taken back"
    );
    bucket.delete(OBJECT).await
}

/// A share link: the network's Worker (or `dir:` path), then the secret
/// after `/s#`.
#[must_use]
pub fn link(network: &str, secret: &str) -> String {
    format!("{}/s#{secret}", network.trim_end_matches('/'))
}

/// A link's network and secret.
///
/// # Errors
///
/// If it isn't a share link.
pub fn parse(link: &str) -> Result<(String, String)> {
    let (network, secret) = link.trim().rsplit_once("/s#").context("not an Omacloud share link")?;
    anyhow::ensure!(
        secret.len() == 64 && secret.bytes().all(|b| b.is_ascii_hexdigit()),
        "not an Omacloud share link"
    );
    Ok((network.to_string(), secret.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_carry_the_network_and_the_secret() -> Result<()> {
        let s = secret();
        let l = link("https://omacloud.computer/", &s);
        assert_eq!(l, format!("https://omacloud.computer/s#{s}"));
        assert_eq!(parse(&l)?, ("https://omacloud.computer".to_string(), s.clone()));
        assert_eq!(parse(&link("dir:/tmp/net", &s))?.0, "dir:/tmp/net");
        assert!(parse("https://omacloud.computer/s#short").is_err());
        assert!(parse("https://example.com/").is_err());
        Ok(())
    }
}
