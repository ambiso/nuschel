// SPDX-License-Identifier: EUPL-1.2
//! Percent-encoding (RFC 3986) of the URL parts Nuschl writes and reads.

use std::fmt::Write as _;

/// `text` with every byte except the unreserved characters (`A-Za-z0-9-._~`)
/// and those in `keep` written as `%XX` in uppercase hex.
pub fn encode(text: &str, keep: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric()
            || b"-._~".contains(&byte)
            || keep.as_bytes().contains(&byte)
        {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// `%XX` escapes to their bytes; with `plus_as_space`, `+` is a space as in
/// form-encoded queries. `None` unless every escape is two hex digits.
pub fn decode(text: &[u8], plus_as_space: bool) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len());
    let mut index = 0;
    while let Some(&byte) = text.get(index) {
        index += 1;
        match byte {
            b'%' => {
                let mut value = [0];
                hex::decode_to_slice(text.get(index..index + 2)?, &mut value).ok()?;
                out.push(value[0]);
                index += 2;
            }
            b'+' if plus_as_space => out.push(b' '),
            byte => out.push(byte),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_are_exactly_two_hex_digits() {
        assert_eq!(decode(b"a%2fb%2F+", false).unwrap(), b"a/b/+");
        assert_eq!(decode(b"a+b", true).unwrap(), b"a b");
        // `u8::from_str_radix` would take "+1" as 1.
        for malformed in ["%", "%4", "a%zz", "%+1", "%-1", "%\u{e9}"] {
            assert_eq!(decode(malformed.as_bytes(), false), None, "{malformed}");
        }
    }

    #[test]
    fn encoding_keeps_unreserved_and_chosen_bytes() {
        assert_eq!(encode("Dev Talk/ü~:", ":"), "Dev%20Talk%2F%C3%BC~:");
        let encoded = encode("a b+/ü", "");
        assert_eq!(
            decode(encoded.as_bytes(), true).unwrap(),
            "a b+/ü".as_bytes()
        );
    }
}
