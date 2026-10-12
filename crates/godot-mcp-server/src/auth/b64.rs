//! Unpadded base64url, implemented here rather than pulled in as a dependency.
//!
//! The JOSE wire format is base64url without padding (RFC 7515 §2). The workspace
//! pins every direct dependency exactly and this is the only place the encoding is
//! needed, so a thirty-line codec is cheaper than a new pinned crate — and it
//! keeps the decoder strict: no padding is accepted, no standard-alphabet
//! characters are accepted, and a trailing partial group that could not have come
//! from any input is rejected instead of being silently truncated.

/// Encode `bytes` as unpadded base64url.
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut buffer = [0_u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let packed =
            (u32::from(buffer[0]) << 16) | (u32::from(buffer[1]) << 8) | u32::from(buffer[2]);
        let symbols = match chunk.len() {
            1 => 2,
            2 => 3,
            _ => 4,
        };
        for index in 0..symbols {
            let shift = 18 - index * 6;
            let position = usize::try_from((packed >> shift) & 0x3f).expect("6 bits fit in usize");
            out.push(char::from(ALPHABET[position]));
        }
    }
    out
}

/// Decode unpadded base64url, or `None` when `text` is not valid.
#[must_use]
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3 + 2);
    for group in bytes.chunks(4) {
        let mut packed = 0_u32;
        for (index, byte) in group.iter().enumerate() {
            packed |= u32::from(symbol(*byte)?) << (18 - index * 6);
        }
        let produced = group.len() - 1;
        for index in 0..produced {
            let shift = 16 - index * 8;
            out.push(u8::try_from((packed >> shift) & 0xff).expect("8 bits fit in u8"));
        }
        // Bits beyond the produced bytes must be zero, or the encoding was not
        // canonical and two distinct texts would decode to the same bytes.
        let used = produced * 8;
        if used < 24 && (packed & ((1 << (24 - used)) - 1)) != 0 {
            return None;
        }
    }
    Some(out)
}

const fn symbol(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_short_length() {
        for length in 0_usize..=32 {
            let payload: Vec<u8> = (0..length)
                .map(|index| u8::try_from(index * 7 % 251).expect("fits"))
                .collect();
            let text = encode(&payload);
            assert!(!text.contains('='), "padding must never be emitted");
            assert_eq!(decode(&text).as_deref(), Some(payload.as_slice()));
        }
    }

    #[test]
    fn rejects_padding_standard_alphabet_and_impossible_lengths() {
        assert_eq!(decode("QQ=="), None);
        assert_eq!(decode("++++"), None);
        assert_eq!(decode("////"), None);
        assert_eq!(decode("A"), None);
        assert_eq!(decode("QQQQQ"), None);
        // Non-canonical trailing bits.
        assert_eq!(decode("QR"), None);
        assert_eq!(decode("QQ"), Some(vec![0x41]));
    }
}
