//! A trusted contact: someone who keeps a card that, together with your
//! Google sign-in, gives back the recovery code.
//!
//! The code is split in two. Omacloud storage keeps a *pad*, 28 random
//! characters, and hands it only to a Google sign-in of the account. The
//! contact keeps the *card*: the code with the pad added to it, character
//! by character (a one-time pad over the code's alphabet). Either half alone
//! is uniformly random and says nothing about the code; together they give
//! it back.
//!
//! A card reads like a recovery code with four more characters, a check
//! that catches a mistyped card. Making a new card replaces the pad, so an
//! earlier card stops working; removing the contact deletes the pad.

use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};

use crate::devices::{ALPHABET, random_secret};

const CODE_LEN: usize = 28;
const CHECK_LEN: usize = 4;

/// Characters a code, pad or card is written with, as positions in the
/// alphabet. Case, spaces and dashes don't matter.
fn digits(s: &str, what: &str, len: usize) -> Result<Vec<usize>> {
    let d: Vec<usize> = s
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| {
            let c = c.to_ascii_lowercase() as u8;
            ALPHABET.iter().position(|&a| a == c).ok_or_else(|| {
                anyhow::anyhow!("{what} has a character it can't have: {}", c as char)
            })
        })
        .collect::<Result<_>>()?;
    ensure!(d.len() == len, "{what} has {len} characters");
    Ok(d)
}

fn text(d: &[usize]) -> String {
    d.iter().map(|&i| ALPHABET[i] as char).collect()
}

/// In groups of four, as the recovery code is shown.
fn grouped(s: &str) -> String {
    s.as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

fn check(body: &str) -> String {
    let n = ALPHABET.len();
    Sha256::new()
        .chain_update(b"omacloud-card\n")
        .chain_update(body.as_bytes())
        .finalize()[..CHECK_LEN]
        .iter()
        .map(|&b| ALPHABET[usize::from(b) % n] as char)
        .collect()
}

/// A new pad, for Omacloud storage to keep.
#[must_use]
pub fn new_pad() -> String {
    random_secret()
}

/// The contact's card for `code`, with the pad Omacloud keeps.
///
/// # Errors
///
/// If `code` isn't a recovery code or `pad` isn't a pad.
pub fn card(code: &str, pad: &str) -> Result<String> {
    let n = ALPHABET.len();
    let code = digits(code, "a recovery code", CODE_LEN)?;
    let pad = digits(pad, "a pad", CODE_LEN)?;
    let body = text(
        &code
            .iter()
            .zip(&pad)
            .map(|(c, p)| (c + p) % n)
            .collect::<Vec<_>>(),
    );
    let check = check(&body);
    Ok(grouped(&format!("{body}{check}")))
}

/// Whether `s` is written like a card rather than a recovery code.
#[must_use]
pub fn is_card(s: &str) -> bool {
    s.chars().filter(char::is_ascii_alphanumeric).count() == CODE_LEN + CHECK_LEN
}

/// A card's characters before the check, if the check matches: whether it
/// was typed right, before signing in for the pad.
///
/// # Errors
///
/// If the card is mistyped.
pub fn checked(card: &str) -> Result<Vec<usize>> {
    let mut all = digits(card, "a contact's card", CODE_LEN + CHECK_LEN)?;
    let sum = all.split_off(CODE_LEN);
    ensure!(
        check(&text(&all)) == text(&sum),
        "that card is mistyped (its last four characters don't match the rest)"
    );
    Ok(all)
}

/// The recovery code a card and the account's pad give back. A card made
/// with another pad (an earlier card, or another account's) gives a code
/// that isn't the account's, which joining then refuses.
///
/// # Errors
///
/// If the card is mistyped or `pad` isn't a pad.
pub fn open(card: &str, pad: &str) -> Result<String> {
    let n = ALPHABET.len();
    let body = checked(card)?;
    let pad = digits(pad, "a pad", CODE_LEN)?;
    let code: Vec<usize> = body
        .iter()
        .zip(&pad)
        .map(|(c, p)| (c + n - p) % n)
        .collect();
    Ok(grouped(&text(&code)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{new_recovery_code, root_key};

    #[test]
    fn card_and_pad_give_back_the_code() -> Result<()> {
        let code = new_recovery_code();
        let pad = new_pad();
        let card = card(&code, &pad)?;
        assert!(is_card(&card));
        assert!(!is_card(&code));
        assert_eq!(open(&card, &pad)?, code);
        // as someone might type it
        assert_eq!(open(&card.to_uppercase().replace('-', " "), &pad)?, code);
        root_key(&open(&card, &pad)?)?;
        Ok(())
    }

    #[test]
    fn another_pad_gives_another_code() -> Result<()> {
        let code = new_recovery_code();
        let card = card(&code, &new_pad())?;
        assert_ne!(open(&card, &new_pad())?, code);
        Ok(())
    }

    #[test]
    fn typos_are_caught() -> Result<()> {
        let card = card(&new_recovery_code(), &new_pad())?;
        let pad = new_pad();
        let mut typo = card.clone().into_bytes();
        typo[0] = if typo[0] == b'2' { b'3' } else { b'2' };
        assert!(checked(&String::from_utf8(typo)?).is_err());
        checked(&card)?;
        assert!(open(&card[..card.len() - 1], &pad).is_err());
        assert!(open(&card.replacen(&card[..1], "0", 1), &pad).is_err());
        assert!(super::card("too-short", &pad).is_err());
        Ok(())
    }
}
