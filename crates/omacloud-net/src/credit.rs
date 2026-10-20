//! Storage credits: anonymous tokens instead of a ledger.
//!
//! The Worker signs tokens blind (RFC 9474), so it can't tell which
//! computer it gave a token to when that token comes back. An owner pays a
//! holder in tokens for keeping its blobs; the holder checks them against
//! the Worker's public key on its own, then trades them at the Worker for
//! fresh ones of its own, which it spends on storage for its own account.
//! The Worker sees tokens issued and tokens spent, never whose. One token
//! is one GiB kept for a month.

use anyhow::{Context, Result};
use blind_rsa_signatures::{
    BlindSignature, BlindingResult, MessageRandomizer, PublicKeySha384PSSRandomized, Signature,
};
use chacha20::{ChaCha20Rng, rand_core::SeedableRng};
use serde::{Deserialize, Serialize};

/// The Worker's token key.
pub type IssuerKey = PublicKeySha384PSSRandomized;

/// What one token buys: a GiB for a month.
const TOKEN_BYTES: u64 = 1 << 30;

/// A spendable token.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Token {
    #[serde(with = "hex::serde")]
    msg: [u8; 32],
    #[serde(with = "hex::serde")]
    randomizer: [u8; 32],
    #[serde(with = "hex::serde")]
    sig: Vec<u8>,
}

impl Token {
    /// Whether the Worker signed it. Says nothing of whether it was spent:
    /// only the Worker knows that, when it's traded in.
    ///
    /// # Errors
    ///
    /// If the signature doesn't verify.
    pub fn verify(&self, issuer: &IssuerKey) -> Result<()> {
        issuer
            .verify(
                &Signature(self.sig.clone()),
                Some(MessageRandomizer(self.randomizer)),
                self.msg,
            )
            .context("not a token the Worker signed")
    }

    /// What the Worker records as spent.
    #[must_use]
    pub fn id(&self) -> [u8; 32] {
        *blake3::hash(&self.msg).as_bytes()
    }
}

/// Tokens asked for and not yet signed: their blinding stays here.
pub struct Pending {
    asked: Vec<([u8; 32], BlindingResult)>,
}

/// Ask for `n` tokens: the blind messages go to the Worker.
///
/// # Errors
///
/// If blinding fails (a key that isn't a usable RSA key).
pub fn ask(issuer: &IssuerKey, n: usize) -> Result<(Vec<Vec<u8>>, Pending)> {
    let seeds: Vec<[u8; 32]> = (0..n).map(|_| rand::random()).collect();
    ask_from(issuer, &seeds)
}

/// [`ask`], a token for each seed, all of it drawn from that seed: the
/// same seed asks the same, so an exchange whose answer was lost can be
/// asked again. Only whoever keeps a seed can turn its signature into a
/// token.
///
/// # Errors
///
/// As [`ask`].
pub fn ask_from(issuer: &IssuerKey, seeds: &[[u8; 32]]) -> Result<(Vec<Vec<u8>>, Pending)> {
    let mut asked = Vec::with_capacity(seeds.len());
    for seed in seeds {
        let mut rng = ChaCha20Rng::from_seed(blake3::keyed_hash(seed, b"blinding").into());
        let msg: [u8; 32] = blake3::keyed_hash(seed, b"message").into();
        let blinded = issuer.blind(&mut rng, msg).context("blinding a token")?;
        asked.push((msg, blinded));
    }
    let blind = asked.iter().map(|(_, b)| b.blind_message.0.clone()).collect();
    Ok((blind, Pending { asked }))
}

impl Pending {
    /// Unblind the Worker's signatures, each in the place of the blind
    /// message it signs: those it didn't sign are `None` (an exchange pays
    /// nothing for a token spent before), or missing at the end.
    ///
    /// # Errors
    ///
    /// If the count is off or a signature isn't the Worker's.
    pub fn finish(self, issuer: &IssuerKey, signed: &[Option<Vec<u8>>]) -> Result<Vec<Token>> {
        Ok(self.finish_each(issuer, signed)?.into_iter().flatten().collect())
    }

    /// [`Self::finish`], a place for each asked: `None` where nothing was
    /// signed.
    ///
    /// # Errors
    ///
    /// As [`Self::finish`].
    pub fn finish_each(self, issuer: &IssuerKey, signed: &[Option<Vec<u8>>]) -> Result<Vec<Option<Token>>> {
        anyhow::ensure!(
            signed.len() <= self.asked.len(),
            "asked for {} tokens, got {}",
            self.asked.len(),
            signed.len()
        );
        let n = self.asked.len();
        let mut out: Vec<Option<Token>> = self
            .asked
            .into_iter()
            .zip(signed)
            .map(|((msg, blinded), sig)| {
                let Some(sig) = sig else { return Ok(None) };
                let sig = issuer
                    .finalize(&BlindSignature(sig.clone()), &blinded, msg)
                    .context("a token the Worker didn't sign properly")?;
                Ok(Some(Token {
                    msg,
                    randomizer: blinded.msg_randomizer.context("randomized tokens")?.0,
                    sig: sig.0,
                }))
            })
            .collect::<Result<_>>()?;
        out.resize(n, None);
        Ok(out)
    }
}

/// The seed a holder blinds the token it trades `token` for from: its
/// node's secret key's and the token's, so the same whatever batch it's
/// asked in, and nobody else (not the owner who paid with it) can ask it.
#[must_use]
pub fn deposit_seed(node_secret: &[u8; 32], token: &Token) -> [u8; 32] {
    let key = blake3::derive_key("omacloud deposit blinding", node_secret);
    blake3::keyed_hash(&key, &token.id()).into()
}

/// Tokens for keeping `bytes` for a month, rounded up.
#[must_use]
pub fn tokens_for(bytes: u64) -> u64 {
    bytes.div_ceil(TOKEN_BYTES)
}

#[cfg(test)]
mod tests {
    use blind_rsa_signatures::{DefaultRng, KeyPairSha384PSSRandomized};

    use super::*;

    #[test]
    fn tokens_verify_and_can_not_be_linked_to_what_was_signed() -> Result<()> {
        let kp = KeyPairSha384PSSRandomized::generate(&mut DefaultRng, 2048)?;
        let (blind, pending) = ask(&kp.pk, 2)?;
        let signed: Vec<Option<Vec<u8>>> = blind
            .iter()
            .map(|b| kp.sk.blind_sign(b).map(|s| Some(s.0)))
            .collect::<Result<_, _>>()?;
        let tokens = pending.finish(&kp.pk, &signed)?;
        assert_eq!(tokens.len(), 2);
        for t in &tokens {
            t.verify(&kp.pk)?;
            // what the Worker saw shares nothing with what comes back
            assert!(blind.iter().all(|b| !b.windows(32).any(|w| w == t.msg)));
            assert!(signed.iter().all(|s| s.as_ref() != Some(&t.sig)));
        }
        assert_ne!(tokens[0].id(), tokens[1].id());
        Ok(())
    }

    #[test]
    fn a_forged_or_altered_token_fails() -> Result<()> {
        let kp = KeyPairSha384PSSRandomized::generate(&mut DefaultRng, 2048)?;
        let other = KeyPairSha384PSSRandomized::generate(&mut DefaultRng, 2048)?;
        let (blind, pending) = ask(&kp.pk, 1)?;
        let mut t = pending
            .finish(&kp.pk, &[Some(kp.sk.blind_sign(&blind[0])?.0)])?
            .remove(0);
        assert!(t.verify(&other.pk).is_err());
        t.msg[0] ^= 1;
        assert!(t.verify(&kp.pk).is_err());
        // a signature over some other blind message doesn't unblind into one
        let (blind, pending) = ask(&kp.pk, 1)?;
        let (stray, _) = ask(&kp.pk, 1)?;
        assert_ne!(blind, stray);
        assert!(pending.finish(&kp.pk, &[Some(kp.sk.blind_sign(&stray[0])?.0)]).is_err());
        Ok(())
    }

    #[test]
    fn the_same_seed_asks_the_same_and_skips_what_wasnt_signed() -> Result<()> {
        let kp = KeyPairSha384PSSRandomized::generate(&mut DefaultRng, 2048)?;
        let (blind, _) = ask_from(&kp.pk, &[[1; 32], [2; 32]])?;
        let (again, pending) = ask_from(&kp.pk, &[[1; 32], [2; 32]])?;
        assert_eq!(blind, again);
        // a seed asks the same wherever it is
        assert_eq!(ask_from(&kp.pk, &[[2; 32]])?.0[0], blind[1]);
        assert_ne!(blind[0], blind[1]);
        let tokens = pending.finish(&kp.pk, &[None, Some(kp.sk.blind_sign(&blind[1])?.0)])?;
        assert_eq!(tokens.len(), 1);
        tokens[0].verify(&kp.pk)?;
        Ok(())
    }

    #[test]
    fn a_partial_gib_costs_a_whole_token() {
        assert_eq!(tokens_for(0), 0);
        assert_eq!(tokens_for(1), 1);
        assert_eq!(tokens_for(TOKEN_BYTES), 1);
        assert_eq!(tokens_for(TOKEN_BYTES + 1), 2);
    }
}
