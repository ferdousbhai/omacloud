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
    BlindSignature, BlindingResult, DefaultRng, MessageRandomizer, PublicKeySha384PSSRandomized, Signature,
};
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
    let mut asked = Vec::with_capacity(n);
    for _ in 0..n {
        let msg: [u8; 32] = rand::random();
        let blinded = issuer.blind(&mut DefaultRng, msg).context("blinding a token")?;
        asked.push((msg, blinded));
    }
    let blind = asked.iter().map(|(_, b)| b.blind_message.0.clone()).collect();
    Ok((blind, Pending { asked }))
}

impl Pending {
    /// Unblind the Worker's signatures, in the order asked: as many as it
    /// signed, the first ones (an exchange pays nothing for a token spent
    /// before).
    ///
    /// # Errors
    ///
    /// If the count is off or a signature isn't the Worker's.
    pub fn finish(self, issuer: &IssuerKey, signed: &[Vec<u8>]) -> Result<Vec<Token>> {
        anyhow::ensure!(
            signed.len() <= self.asked.len(),
            "asked for {} tokens, got {}",
            self.asked.len(),
            signed.len()
        );
        self.asked
            .into_iter()
            .zip(signed)
            .map(|((msg, blinded), sig)| {
                let sig = issuer
                    .finalize(&BlindSignature(sig.clone()), &blinded, msg)
                    .context("a token the Worker didn't sign properly")?;
                Ok(Token {
                    msg,
                    randomizer: blinded.msg_randomizer.context("randomized tokens")?.0,
                    sig: sig.0,
                })
            })
            .collect()
    }
}

/// Tokens for keeping `bytes` for a month, rounded up.
#[must_use]
pub fn tokens_for(bytes: u64) -> u64 {
    bytes.div_ceil(TOKEN_BYTES)
}

#[cfg(test)]
mod tests {
    use blind_rsa_signatures::KeyPairSha384PSSRandomized;

    use super::*;

    #[test]
    fn tokens_verify_and_can_not_be_linked_to_what_was_signed() -> Result<()> {
        let kp = KeyPairSha384PSSRandomized::generate(&mut DefaultRng, 2048)?;
        let (blind, pending) = ask(&kp.pk, 2)?;
        let signed: Vec<Vec<u8>> = blind
            .iter()
            .map(|b| kp.sk.blind_sign(b).map(|s| s.0))
            .collect::<Result<_, _>>()?;
        let tokens = pending.finish(&kp.pk, &signed)?;
        assert_eq!(tokens.len(), 2);
        for t in &tokens {
            t.verify(&kp.pk)?;
            // what the Worker saw shares nothing with what comes back
            assert!(blind.iter().all(|b| !b.windows(32).any(|w| w == t.msg)));
            assert!(signed.iter().all(|s| *s != t.sig));
        }
        assert_ne!(tokens[0].id(), tokens[1].id());
        Ok(())
    }

    #[test]
    fn a_forged_or_altered_token_fails() -> Result<()> {
        let kp = KeyPairSha384PSSRandomized::generate(&mut DefaultRng, 2048)?;
        let other = KeyPairSha384PSSRandomized::generate(&mut DefaultRng, 2048)?;
        let (blind, pending) = ask(&kp.pk, 1)?;
        let mut t = pending.finish(&kp.pk, &[kp.sk.blind_sign(&blind[0])?.0])?.remove(0);
        assert!(t.verify(&other.pk).is_err());
        t.msg[0] ^= 1;
        assert!(t.verify(&kp.pk).is_err());
        // a signature over some other blind message doesn't unblind into one
        let (blind, pending) = ask(&kp.pk, 1)?;
        let (stray, _) = ask(&kp.pk, 1)?;
        assert_ne!(blind, stray);
        assert!(pending.finish(&kp.pk, &[kp.sk.blind_sign(&stray[0])?.0]).is_err());
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
