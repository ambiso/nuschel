// SPDX-License-Identifier: EUPL-1.2
//! Safety numbers and their QR payload (docs/e2ee.md §4).
//!
//! **Digits.** For each identity, 30 digits from Signal's numeric fingerprint
//! construction with the identity fingerprint as the key and no stable identifier (the
//! identity is the same on every server):
//!
//! ```text
//! h_1 = SHA-512(0x00 0x01 || fp || fp)              // version 1, empty identifier
//! h_i = SHA-512(h_{i-1} || fp)                      // i = 2 ..= 5200
//! digits(fp) = for each 5-byte chunk c of h_5200[0..30]:
//!                  format("{:05}", uint40_be(c) mod 100000)
//! safety_number(a, b) = min(digits(a), digits(b)) || max(digits(a), digits(b))
//! ```
//!
//! Sorting the two halves makes the number the same on both sides; it is shown as 12
//! groups of 5 digits.
//!
//! **QR payload** (68 bytes): `"NSQ1" || min(fp_a, fp_b) || max(fp_a, fp_b)`, the raw
//! fingerprints in byte order. Scanning compares both and says which side differs.

use std::fmt;

use subtle::ConstantTimeEq as _;

use crate::Error;
use crate::crypto;
use crate::keys::IdentityFingerprint;

const ITERATIONS: usize = 5200;
const VERSION: [u8; 2] = [0x00, 0x01];
const QR_MAGIC: &[u8; 4] = b"NSQ1";

fn digits(fingerprint: &IdentityFingerprint) -> [u8; 30] {
    let key = fingerprint.as_bytes();
    let mut hash = crypto::sha512(&[&VERSION, key, key]);
    for _ in 1..ITERATIONS {
        hash = crypto::sha512(&[&hash, key]);
    }
    let mut out = [0u8; 30];
    let (chunks, _) = hash[..30].as_chunks::<5>();
    let (groups, _) = out.as_chunks_mut::<5>();
    for (chunk, group) in chunks.iter().zip(groups) {
        let value = chunk.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b)) % 100_000;
        group.copy_from_slice(format!("{value:05}").as_bytes());
    }
    out
}

/// 60 decimal digits naming a pair of identities, the same for both of them.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SafetyNumber([u8; 60]);

impl SafetyNumber {
    /// The safety number of two identities (in either order).
    pub fn new(a: &IdentityFingerprint, b: &IdentityFingerprint) -> Self {
        let (x, y) = (digits(a), digits(b));
        let (first, second) = if x <= y { (x, y) } else { (y, x) };
        let mut out = [0u8; 60];
        out[..30].copy_from_slice(&first);
        out[30..].copy_from_slice(&second);
        Self(out)
    }

    /// The 60 digits without separators.
    pub fn digits(&self) -> &str {
        std::str::from_utf8(&self.0).expect("ASCII digits")
    }

    /// The 12 groups of 5 digits.
    pub fn groups(&self) -> [&str; 12] {
        std::array::from_fn(|i| &self.digits()[i * 5..i * 5 + 5])
    }

    /// Parses what a user typed or pasted: exactly 60 ASCII digits, either contiguous or
    /// as 12 groups of 5 separated by single spaces. Nothing else is accepted.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let bytes = text.as_bytes();
        let compact: Vec<u8> = match bytes.len() {
            60 => bytes.to_vec(),
            71 => {
                let mut out = Vec::with_capacity(60);
                for (i, &b) in bytes.iter().enumerate() {
                    if i % 6 == 5 {
                        if b != b' ' {
                            return Err(Error::Malformed("safety number grouping"));
                        }
                    } else {
                        out.push(b);
                    }
                }
                out
            }
            _ => return Err(Error::Malformed("safety number length")),
        };
        if !compact.iter().all(u8::is_ascii_digit) {
            return Err(Error::Malformed("safety number digits"));
        }
        Ok(Self(compact.try_into().expect("60 digits")))
    }

    /// Exact comparison of all 60 digits, in constant time.
    pub fn matches(&self, other: &SafetyNumber) -> bool {
        bool::from(self.0.ct_eq(&other.0))
    }
}

impl fmt::Display for SafetyNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.groups().join(" "))
    }
}

impl fmt::Debug for SafetyNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SafetyNumber({self})")
    }
}

/// The QR payload for a pair of identities.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SafetyQr([u8; 68]);

/// The outcome of scanning another person's safety QR code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QrComparison {
    /// Both sides have the same two identities.
    Match,
    /// The other side presents an identity different from the one you have for them.
    TheirIdentityDiffers,
    /// The other side has a different identity for you.
    YourIdentityDiffers,
    /// Neither identity matches.
    BothDiffer,
}

impl SafetyQr {
    pub const LEN: usize = 68;

    pub fn new(a: &IdentityFingerprint, b: &IdentityFingerprint) -> Self {
        let (first, second) = if a <= b { (a, b) } else { (b, a) };
        let mut out = [0u8; 68];
        out[..4].copy_from_slice(QR_MAGIC);
        out[4..36].copy_from_slice(first.as_bytes());
        out[36..].copy_from_slice(second.as_bytes());
        Self(out)
    }

    pub fn as_bytes(&self) -> &[u8; 68] {
        &self.0
    }

    /// Parses a scanned payload: the magic, two fingerprints in order, nothing else.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        let bytes: [u8; 68] = bytes
            .try_into()
            .map_err(|_| Error::Malformed("safety QR length"))?;
        if &bytes[..4] != QR_MAGIC {
            return Err(Error::Malformed("safety QR magic"));
        }
        if bytes[4..36] > bytes[36..] {
            return Err(Error::Malformed("safety QR order"));
        }
        Ok(Self(bytes))
    }

    fn contains(&self, fingerprint: &IdentityFingerprint) -> bool {
        &self.0[4..36] == fingerprint.as_bytes() || &self.0[36..] == fingerprint.as_bytes()
    }

    /// Compares a scanned payload with your own view: your identity `own` and the
    /// identity you have for the other person, `theirs`.
    pub fn compare(
        scanned: &[u8],
        own: &IdentityFingerprint,
        theirs: &IdentityFingerprint,
    ) -> Result<QrComparison, Error> {
        let scanned = Self::parse(scanned)?;
        if scanned == Self::new(own, theirs) {
            return Ok(QrComparison::Match);
        }
        Ok(match (scanned.contains(own), scanned.contains(theirs)) {
            (true, false) => QrComparison::TheirIdentityDiffers,
            (false, true) => QrComparison::YourIdentityDiffers,
            _ => QrComparison::BothDiffer,
        })
    }
}

impl fmt::Debug for SafetyQr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SafetyQr({} bytes)", self.0.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(byte: u8) -> IdentityFingerprint {
        IdentityFingerprint::from_bytes([byte; 32])
    }

    #[test]
    fn symmetric_sixty_digits_and_distinct() {
        let (alice, bob, carol) = (fp(1), fp(2), fp(3));
        let number = SafetyNumber::new(&alice, &bob);
        assert_eq!(number, SafetyNumber::new(&bob, &alice));
        assert_eq!(number.digits().len(), 60);
        assert!(number.digits().bytes().all(|b| b.is_ascii_digit()));
        assert_ne!(number, SafetyNumber::new(&alice, &carol));
        assert_eq!(number.to_string().len(), 71);
        assert_eq!(number.groups().len(), 12);
        // The lower half comes first.
        let (x, y) = (digits(&alice), digits(&bob));
        assert_eq!(
            &number.digits()[..30],
            std::str::from_utf8(&x.min(y)).unwrap()
        );
    }

    #[test]
    fn parsing_tolerates_nothing() {
        let number = SafetyNumber::new(&fp(1), &fp(2));
        assert_eq!(SafetyNumber::parse(number.digits()).unwrap(), number);
        assert_eq!(SafetyNumber::parse(&number.to_string()).unwrap(), number);
        let digits = number.digits();
        for bad in [
            &digits[..59],
            &format!("{digits}0"),
            &format!(" {}", &digits[1..]),
            &format!("{}a", &digits[..59]),
            &number.to_string().replacen(' ', "-", 1),
            &number.to_string().replacen(' ', "  ", 1)[..71],
            &format!("{}٣", &digits[..58]),
            &format!("{}\n", &digits[..59]),
        ] {
            assert!(SafetyNumber::parse(bad).is_err(), "accepted {bad:?}");
        }
        // Every single-digit change is a mismatch.
        for i in 0..60 {
            let mut changed = number.0;
            changed[i] = if changed[i] == b'9' {
                b'0'
            } else {
                changed[i] + 1
            };
            assert!(!number.matches(&SafetyNumber(changed)));
        }
        assert!(number.matches(&SafetyNumber::new(&fp(2), &fp(1))));
    }

    #[test]
    fn qr_compare_says_which_side_differs() {
        let (alice, bob, mallory) = (fp(1), fp(2), fp(9));
        let bobs_code = SafetyQr::new(&bob, &alice);
        assert_eq!(bobs_code, SafetyQr::new(&alice, &bob));
        assert_eq!(bobs_code.as_bytes().len(), 68);
        let scanned = bobs_code.as_bytes();
        assert_eq!(
            SafetyQr::compare(scanned, &alice, &bob).unwrap(),
            QrComparison::Match
        );
        // Alice was given Mallory's key for Bob.
        assert_eq!(
            SafetyQr::compare(scanned, &alice, &mallory).unwrap(),
            QrComparison::TheirIdentityDiffers
        );
        // Bob was given Mallory's key for Alice.
        let bobs_wrong_code = SafetyQr::new(&bob, &mallory);
        assert_eq!(
            SafetyQr::compare(bobs_wrong_code.as_bytes(), &alice, &bob).unwrap(),
            QrComparison::YourIdentityDiffers
        );
        assert_eq!(
            SafetyQr::compare(SafetyQr::new(&fp(7), &fp(8)).as_bytes(), &alice, &bob).unwrap(),
            QrComparison::BothDiffer
        );
        // Malformed payloads.
        assert!(SafetyQr::compare(&scanned[..67], &alice, &bob).is_err());
        let mut wrong_magic = *scanned;
        wrong_magic[3] = b'2';
        assert!(SafetyQr::parse(&wrong_magic).is_err());
        let mut unordered = *scanned;
        unordered[4..36].copy_from_slice(&[0xff; 32]);
        assert!(SafetyQr::parse(&unordered).is_err());
    }
}
