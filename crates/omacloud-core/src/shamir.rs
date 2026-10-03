//! Social recovery: the recovery code split among people you trust, so any
//! `k` of `n` shares rebuild it and fewer reveal nothing about it (Shamir's
//! secret sharing over GF(256), byte by byte).
//!
//! A share reads `omacloud-share-<tag>-<k>of<n>-<x>-<hex>-<check>`: `tag`
//! names the account (the start of its root key) so shares of different
//! accounts don't mix, and `check` catches a mistyped share on its own.
//! Combining checks that the result derives the tagged root.

use anyhow::{Context, Result, bail, ensure};
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::devices::{public_hex, root_key};

const PREFIX: &str = "omacloud-share-";
const TAG_LEN: usize = 8;

/// The recovery code as the root key reads it.
fn normalize(code: &str) -> String {
    code.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn mul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        let carry = a & 0x80;
        a <<= 1;
        if carry != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    p
}

fn inv(a: u8) -> u8 {
    // a^254 = a^-1 in GF(256)
    let mut r = 1;
    for _ in 0..254 {
        r = mul(r, a);
    }
    r
}

fn check(body: &str) -> String {
    hex::encode(&Sha256::digest(body.as_bytes())[..2])
}

/// Split `code` into `n` shares, any `k` of which rebuild it.
///
/// # Errors
///
/// If the code isn't a recovery code, or `k` and `n` don't make sense.
pub fn split(code: &str, k: u8, n: u8) -> Result<Vec<String>> {
    ensure!(
        (2..=n).contains(&k),
        "need at least 2 shares to rebuild, and no more than there are"
    );
    let root = public_hex(&root_key(code)?);
    let secret = normalize(code).into_bytes();
    let mut rng = rand::rngs::OsRng;
    // one random polynomial per byte, the byte as its constant term
    let polys: Vec<Vec<u8>> = secret
        .iter()
        .map(|&s| {
            let mut c = vec![0u8; usize::from(k)];
            rng.fill_bytes(&mut c[1..]);
            c[0] = s;
            c
        })
        .collect();
    Ok((1..=n)
        .map(|x| {
            let ys: Vec<u8> = polys
                .iter()
                .map(|c| c.iter().rev().fold(0, |acc, &ci| mul(acc, x) ^ ci))
                .collect();
            let body = format!(
                "{PREFIX}{}-{k}of{n}-{x}-{}",
                &root[..TAG_LEN],
                hex::encode(ys)
            );
            let sum = check(&body);
            format!("{body}-{sum}")
        })
        .collect())
}

struct Share {
    tag: String,
    k: u8,
    x: u8,
    ys: Vec<u8>,
}

fn parse(share: &str) -> Result<Share> {
    let share = share.trim();
    let (body, sum) = share.rsplit_once('-').context("not an omacloud share")?;
    let rest = body.strip_prefix(PREFIX).context("not an omacloud share")?;
    ensure!(
        check(body) == sum,
        "share {share} is mistyped (its check doesn't match)"
    );
    let parts: Vec<&str> = rest.split('-').collect();
    let [tag, kn, x, ys] = parts[..] else {
        bail!("not an omacloud share");
    };
    let (k, _) = kn.split_once("of").context("not an omacloud share")?;
    Ok(Share {
        tag: tag.to_string(),
        k: k.parse()?,
        x: x.parse()?,
        ys: hex::decode(ys)?,
    })
}

/// How many shares rebuild the code, as the share says.
///
/// # Errors
///
/// If it isn't a well formed share.
pub fn needed(share: &str) -> Result<u8> {
    Ok(parse(share)?.k)
}

/// Rebuild the recovery code from shares.
///
/// # Errors
///
/// If a share is malformed, they belong to different accounts, there are too
/// few, or together they don't rebuild the account's code.
pub fn combine(shares: &[String]) -> Result<String> {
    let mut parsed: Vec<Share> = shares.iter().map(|s| parse(s)).collect::<Result<_>>()?;
    let first = parsed.first().context("no shares")?;
    let (tag, k, len) = (first.tag.clone(), first.k, first.ys.len());
    ensure!(
        parsed
            .iter()
            .all(|s| s.tag == tag && s.k == k && s.ys.len() == len),
        "these shares belong to different accounts or splits"
    );
    parsed.sort_by_key(|s| s.x);
    parsed.dedup_by_key(|s| s.x);
    ensure!(
        parsed.len() >= usize::from(k),
        "{} different shares; this split needs {k}",
        parsed.len()
    );
    parsed.truncate(usize::from(k));
    // Lagrange interpolation at 0
    let mut secret = vec![0u8; len];
    for (i, si) in parsed.iter().enumerate() {
        let mut basis = 1;
        for (j, sj) in parsed.iter().enumerate() {
            if i != j {
                basis = mul(basis, mul(sj.x, inv(sj.x ^ si.x)));
            }
        }
        for (b, &y) in secret.iter_mut().zip(&si.ys) {
            *b ^= mul(y, basis);
        }
    }
    let code = String::from_utf8(secret).ok().filter(|c| c.len() == 28);
    let code = code
        .filter(|c| root_key(c).is_ok_and(|k| public_hex(&k).starts_with(&tag)))
        .context("these shares don't rebuild the account's recovery code")?;
    Ok(code
        .as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("-"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::new_recovery_code;

    #[test]
    fn any_k_of_n_rebuild_the_code() -> Result<()> {
        let code = new_recovery_code();
        let shares = split(&code, 3, 5)?;
        assert_eq!(shares.len(), 5);
        assert_eq!(needed(&shares[0])?, 3);
        for pick in [[0, 1, 2], [4, 2, 0], [1, 3, 4]] {
            let some: Vec<String> = pick.iter().map(|&i| shares[i].clone()).collect();
            assert_eq!(normalize(&combine(&some)?), normalize(&code));
        }
        // too few, or a repeated share, don't
        assert!(combine(&shares[..2]).is_err());
        let twice = vec![shares[0].clone(), shares[0].clone(), shares[1].clone()];
        assert!(combine(&twice).is_err());
        Ok(())
    }

    #[test]
    fn typos_and_foreign_shares_are_caught() -> Result<()> {
        let shares = split(&new_recovery_code(), 2, 3)?;
        let mut typo = shares[0].clone().into_bytes();
        let at = typo.len() - 10;
        typo[at] = if typo[at] == b'0' { b'1' } else { b'0' };
        let typo = String::from_utf8(typo)?;
        assert!(needed(&typo).is_err());
        let other = split(&new_recovery_code(), 2, 3)?;
        assert!(combine(&[shares[0].clone(), other[1].clone()]).is_err());
        assert!(split("too-short", 2, 3).is_err());
        assert!(split(&new_recovery_code(), 1, 3).is_err());
        assert!(split(&new_recovery_code(), 4, 3).is_err());
        Ok(())
    }

    #[test]
    fn gf256_inverse() {
        for a in 1..=255u8 {
            assert_eq!(mul(a, inv(a)), 1);
        }
    }
}
