//! Base64 (RFC 4648 §4) and SHA-1 (RFC 3174), implemented in-tree.
//!
//! # Why these are here rather than pulled in
//!
//! The RFC 6455 opening handshake requires exactly two transformations that
//! nothing else in this workspace needs: `SHA-1(Sec-WebSocket-Key + GUID)` and
//! standard base64 of those 20 bytes. Both are short, closed, fully deterministic
//! algorithms with **official test vectors published in their own RFCs**, so this
//! module can be proven correct against the specification itself rather than
//! trusted. Adding two more crates to the shipping graph for ~120 lines of
//! arithmetic would enlarge the supply-chain surface without buying any
//! assurance the RFC vectors do not already give.
//!
//! The precedent is Todo 18: the `Content-Length` codec both Godot protocols
//! speak is likewise hand-written in-tree rather than delegated.
//!
//! # SHA-1 is used only where RFC 6455 mandates it
//!
//! SHA-1 appears here for **one** purpose: computing `Sec-WebSocket-Accept`,
//! which RFC 6455 §4.2.2 defines in terms of SHA-1 and which is a
//! protocol-conformance token, not a security mechanism (the client key is a
//! plaintext nonce echoed back, and every RFC 6455 implementation, Godot's
//! `WebSocketPeer` included, computes exactly this). Authentication of the bridge
//! uses the per-launch 256-bit rendezvous secret compared in constant time; see
//! [`crate::protocol::rendezvous`]. No digest in this module is ever used to
//! authenticate, sign, or derive a key.

/// The standard base64 alphabet (RFC 4648 §4, Table 1).
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode `input` as standard base64 with `=` padding (RFC 4648 §4).
#[must_use]
pub fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).map_or(0, |byte| u32::from(*byte));
        let b2 = chunk.get(2).map_or(0, |byte| u32::from(*byte));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Decode canonical standard base64 with `=` padding (RFC 4648 §4).
///
/// Returns `None` for a non-alphabet byte, misplaced padding, a non-multiple-of-four
/// length, or a non-canonical spelling whose unused trailing bits are nonzero.
#[must_use]
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }

    let quartets = bytes.len() / 4;
    let mut out = Vec::with_capacity(quartets * 3);
    for (index, chunk) in bytes.chunks_exact(4).enumerate() {
        let is_last = index + 1 == quartets;
        let first = base64_value(chunk[0])?;
        let second = base64_value(chunk[1])?;
        let padding = match (chunk[2], chunk[3]) {
            (b'=', b'=') if is_last => 2,
            (_, b'=') if is_last => 1,
            (b'=', _) => return None,
            _ => 0,
        };
        let third = if padding == 2 {
            0
        } else {
            base64_value(chunk[2])?
        };
        let fourth = if padding == 0 {
            base64_value(chunk[3])?
        } else {
            0
        };
        let packed = (u32::from(first) << 18)
            | (u32::from(second) << 12)
            | (u32::from(third) << 6)
            | u32::from(fourth);
        out.push(((packed >> 16) & 0xff) as u8);
        if padding < 2 {
            out.push(((packed >> 8) & 0xff) as u8);
        }
        if padding == 0 {
            out.push((packed & 0xff) as u8);
        }
    }

    (base64_encode(&out) == input).then_some(out)
}

const fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Lowercase hexadecimal of `bytes`.
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(nibble(byte >> 4));
        out.push(nibble(byte & 0x0f));
    }
    out
}

const fn nibble(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        _ => (b'a' + (value - 10)) as char,
    }
}

/// Decode lowercase or uppercase hexadecimal into bytes.
///
/// Returns `None` for an odd length or a non-hex digit. Used to read the
/// rendezvous secret back out of its hex spelling.
#[must_use]
pub fn hex_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let high = from_nibble(pair[0])?;
        let low = from_nibble(pair[1])?;
        out.push((high << 4) | low);
    }
    Some(out)
}

const fn from_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// SHA-1 of `input` (RFC 3174), returning the 20-byte digest.
///
/// Used only for RFC 6455's `Sec-WebSocket-Accept`. See this module's header.
#[must_use]
#[expect(
    clippy::many_single_char_names,
    reason = "a, b, c, d, e are RFC 3174's own working-variable names"
)]
pub fn sha1(input: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xefcd_ab89,
        0x98ba_dcfe,
        0x1032_5476,
        0xc3d2_e1f0,
    ];

    // RFC 3174 §4: append 0x80, pad with zeros to 56 mod 64, then the 64-bit
    // big-endian bit length.
    let bit_length = (input.len() as u64).wrapping_mul(8);
    let mut message = Vec::with_capacity(input.len() + 72);
    message.extend_from_slice(input);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    for block in message.chunks(64) {
        let mut w = [0_u32; 80];
        for (index, word) in block.chunks(4).enumerate() {
            w[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..80 {
            w[index] = (w[index - 3] ^ w[index - 8] ^ w[index - 14] ^ w[index - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (index, word) in w.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5a82_7999_u32),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut digest = [0_u8; 20];
    for (index, word) in h.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::{base64_decode, base64_encode, hex_decode, hex_encode, sha1};

    /// RFC 4648 §10's own test vectors, verbatim.
    #[test]
    fn base64_matches_the_rfc_4648_test_vectors() {
        for (plain, encoded) in [
            (&b""[..], ""),
            (&b"f"[..], "Zg=="),
            (&b"fo"[..], "Zm8="),
            (&b"foo"[..], "Zm9v"),
            (&b"foob"[..], "Zm9vYg=="),
            (&b"fooba"[..], "Zm9vYmE="),
            (&b"foobar"[..], "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(plain), encoded);
            assert_eq!(base64_decode(encoded).as_deref(), Some(plain));
        }
    }

    #[test]
    fn base64_rejects_malformed_or_non_canonical_input() {
        for malformed in ["x", "Zg=", "=g==", "Z=g=", "Zg=A", "Zg==AAAA", "Zh=="] {
            assert_eq!(base64_decode(malformed), None, "input {malformed:?}");
        }
    }

    /// RFC 3174 §7.3's sample digests, plus the canonical empty-input digest.
    #[test]
    fn sha1_matches_the_rfc_3174_test_vectors() {
        assert_eq!(
            hex_encode(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex_encode(&sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(
            hex_encode(&sha1(&b"a".repeat(1_000_000))),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
        assert_eq!(
            hex_encode(&sha1(b"")),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
    }

    /// A message whose length lands exactly on the 56-mod-64 boundary, so the
    /// padding must add a whole extra block.
    #[test]
    fn sha1_pads_a_block_boundary_message_correctly() {
        assert_eq!(
            hex_encode(&sha1(&b"a".repeat(55))),
            "c1c8bbdc22796e28c0e15163d20899b65621d65a"
        );
        assert_eq!(
            hex_encode(&sha1(&b"a".repeat(56))),
            "c2db330f6083854c99d4b5bfb6e8f29f201be699"
        );
        assert_eq!(
            hex_encode(&sha1(&b"a".repeat(64))),
            "0098ba824b5c16427bd7a1122a5a442a25ec644d"
        );
    }

    #[test]
    fn hex_round_trips_and_rejects_malformed_input() {
        let bytes = [0x00_u8, 0x0f, 0xf0, 0xff, 0x5a];
        assert_eq!(hex_encode(&bytes), "000ff0ff5a");
        assert_eq!(hex_decode("000ff0ff5a").as_deref(), Some(&bytes[..]));
        assert_eq!(hex_decode("000FF0FF5A").as_deref(), Some(&bytes[..]));
        assert_eq!(hex_decode("abc"), None, "odd length");
        assert_eq!(hex_decode("zz"), None, "non-hex digit");
    }
}
