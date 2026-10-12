//! Standard (RFC 4648 §4) padded base64, implemented here rather than pulled in
//! as a dependency, for binary file contents and asset uploads.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode `bytes` as padded standard base64.
pub(super) fn encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(ALPHABET[usize::from(first >> 2)] as char);
        output.push(ALPHABET[usize::from(((first & 0x03) << 4) | (second >> 4))] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[usize::from(((second & 0x0f) << 2) | (third >> 6))] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[usize::from(third & 0x3f)] as char
        } else {
            '='
        });
    }
    output
}

fn value_of(symbol: u8) -> Option<u8> {
    match symbol {
        b'A'..=b'Z' => Some(symbol - b'A'),
        b'a'..=b'z' => Some(symbol - b'a' + 26),
        b'0'..=b'9' => Some(symbol - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decode padded standard base64, or `None` when `text` is not canonical
/// base64. ASCII whitespace is ignored, so wrapped encodings decode too.
pub(super) fn decode(text: &str) -> Option<Vec<u8>> {
    let symbols: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if !symbols.len().is_multiple_of(4) {
        return None;
    }
    let mut output = Vec::with_capacity(symbols.len() / 4 * 3);
    let quads = symbols.len() / 4;
    for (index, quad) in symbols.chunks(4).enumerate() {
        let last = index + 1 == quads;
        let padding = quad.iter().rev().take_while(|byte| **byte == b'=').count();
        if padding > 2 || (padding > 0 && !last) {
            return None;
        }
        let mut values = [0_u8; 4];
        for (slot, symbol) in values.iter_mut().zip(&quad[..4 - padding]) {
            *slot = value_of(*symbol)?;
        }
        output.push((values[0] << 2) | (values[1] >> 4));
        if padding < 2 {
            output.push((values[1] << 4) | (values[2] >> 2));
        } else if values[1] & 0x0f != 0 {
            return None;
        }
        if padding < 1 {
            output.push((values[2] << 6) | values[3]);
        } else if padding == 1 && values[2] & 0x03 != 0 {
            return None;
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_padding_length() {
        for bytes in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            assert_eq!(decode(&encode(bytes)).as_deref(), Some(bytes));
        }
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(encode(b"fo"), "Zm8=");
        let binary: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&binary)), Some(binary));
    }

    #[test]
    fn non_canonical_input_is_refused() {
        for text in ["Zm9", "Zm8", "Z===", "Zm9v!mFy", "Zm=8", "Zh==", "Zm9=Zm9v"] {
            assert_eq!(decode(text), None, "{text}");
        }
        assert_eq!(decode("Zm9v\nYmFy").as_deref(), Some(&b"foobar"[..]));
    }
}
