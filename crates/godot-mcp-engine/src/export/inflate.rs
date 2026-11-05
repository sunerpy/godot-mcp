//! A bounded raw DEFLATE decoder (RFC 1951), for the compressed entries of an
//! exported APK or ZIP pack.
//!
//! The input was written by a process that ran project code, so the decoder
//! never trusts a length: every read is checked against the input, every
//! back-reference against what was already produced, and the output never
//! grows past the bound the caller gives — a stream that would is refused
//! rather than truncated.

/// Why a stream could not be inflated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InflateError {
    /// The stream ends inside a block.
    #[error("the compressed stream ends before its last block does")]
    Truncated,
    /// A block names the reserved block type.
    #[error("the compressed stream uses the reserved block type")]
    ReservedBlock,
    /// A stored block's length does not match its complement.
    #[error("a stored block's length does not match its check")]
    StoredLength,
    /// A dynamic block's code lengths do not describe a usable code.
    #[error("a block's Huffman code is malformed")]
    BadCode,
    /// A symbol no code describes, or one that is not allowed where it is.
    #[error("the compressed stream holds an invalid symbol")]
    BadSymbol,
    /// A back-reference reaches before the start of the output.
    #[error("a back-reference reaches before the start of the data")]
    DistanceTooFar,
    /// The output would grow past the caller's bound.
    #[error("the data inflates to more than {limit} bytes")]
    TooLarge {
        /// The caller's bound.
        limit: usize,
    },
}

const MAX_BITS: usize = 15;

/// The base length of each length symbol, 257..=285.
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
/// The base distance of each distance symbol, 0..=29.
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DISTANCE_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
/// The order the code-length code's lengths are sent in.
const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

struct Bits<'a> {
    input: &'a [u8],
    at: usize,
    buffer: u32,
    count: u32,
}

impl Bits<'_> {
    fn take(&mut self, need: u32) -> Result<u32, InflateError> {
        while self.count < need {
            let byte = *self.input.get(self.at).ok_or(InflateError::Truncated)?;
            self.at += 1;
            self.buffer |= u32::from(byte) << self.count;
            self.count += 8;
        }
        let value = self.buffer & ((1_u32 << need) - 1);
        self.buffer >>= need;
        self.count -= need;
        Ok(value)
    }

    /// Drop the bits left in the current byte, before a stored block.
    fn align(&mut self) {
        self.buffer = 0;
        self.count = 0;
    }
}

/// A canonical Huffman code: how many codes of each length, and the symbols
/// in code order.
struct Huffman {
    counts: [u16; MAX_BITS + 1],
    symbols: Vec<u16>,
}

impl Huffman {
    /// The code of `lengths`, and how many codes it leaves unused: zero for a
    /// complete code.
    fn new(lengths: &[u8]) -> Result<(Self, i32), InflateError> {
        let mut counts = [0_u16; MAX_BITS + 1];
        for &length in lengths {
            counts[usize::from(length)] += 1;
        }
        let mut left: i32 = 1;
        for &count in &counts[1..] {
            left <<= 1;
            left -= i32::from(count);
            if left < 0 {
                return Err(InflateError::BadCode);
            }
        }
        let mut offsets = [0_u16; MAX_BITS + 1];
        for length in 1..MAX_BITS {
            offsets[length + 1] = offsets[length] + counts[length];
        }
        let mut symbols = vec![0_u16; lengths.len()];
        for (symbol, &length) in lengths.iter().enumerate() {
            if length != 0 {
                let slot = &mut offsets[usize::from(length)];
                symbols[usize::from(*slot)] =
                    u16::try_from(symbol).map_err(|_| InflateError::BadCode)?;
                *slot += 1;
            }
        }
        Ok((Self { counts, symbols }, left))
    }

    fn decode(&self, bits: &mut Bits<'_>) -> Result<u16, InflateError> {
        let (mut code, mut first, mut index): (i32, i32, i32) = (0, 0, 0);
        for &count in &self.counts[1..] {
            code |= i32::try_from(bits.take(1)?).map_err(|_| InflateError::BadSymbol)?;
            let count = i32::from(count);
            if code - count < first {
                let slot =
                    usize::try_from(index + (code - first)).map_err(|_| InflateError::BadSymbol)?;
                return self
                    .symbols
                    .get(slot)
                    .copied()
                    .ok_or(InflateError::BadSymbol);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(InflateError::BadSymbol)
    }
}

/// Inflate the raw DEFLATE stream `input`, refusing to produce more than
/// `limit` bytes.
///
/// # Errors
/// Returns [`InflateError`] for a malformed or truncated stream, and
/// [`InflateError::TooLarge`] for one that inflates past `limit`.
pub fn inflate(input: &[u8], limit: usize) -> Result<Vec<u8>, InflateError> {
    let mut bits = Bits {
        input,
        at: 0,
        buffer: 0,
        count: 0,
    };
    let mut out: Vec<u8> = Vec::new();
    loop {
        let last = bits.take(1)? == 1;
        match bits.take(2)? {
            0 => stored(&mut bits, &mut out, limit)?,
            1 => {
                let (literals, distances) = fixed_codes()?;
                codes(&mut bits, &mut out, limit, &literals, &distances)?;
            }
            2 => {
                let (literals, distances) = dynamic_codes(&mut bits)?;
                codes(&mut bits, &mut out, limit, &literals, &distances)?;
            }
            _ => return Err(InflateError::ReservedBlock),
        }
        if last {
            return Ok(out);
        }
    }
}

fn stored(bits: &mut Bits<'_>, out: &mut Vec<u8>, limit: usize) -> Result<(), InflateError> {
    bits.align();
    let header = bits
        .input
        .get(bits.at..bits.at + 4)
        .ok_or(InflateError::Truncated)?;
    let length = u16::from_le_bytes([header[0], header[1]]);
    let check = u16::from_le_bytes([header[2], header[3]]);
    if length != !check {
        return Err(InflateError::StoredLength);
    }
    bits.at += 4;
    let length = usize::from(length);
    let data = bits
        .input
        .get(bits.at..bits.at + length)
        .ok_or(InflateError::Truncated)?;
    if out.len() + length > limit {
        return Err(InflateError::TooLarge { limit });
    }
    out.extend_from_slice(data);
    bits.at += length;
    Ok(())
}

fn fixed_codes() -> Result<(Huffman, Huffman), InflateError> {
    let mut lengths = [0_u8; 288];
    for (symbol, length) in lengths.iter_mut().enumerate() {
        *length = match symbol {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let (literals, _) = Huffman::new(&lengths)?;
    let (distances, _) = Huffman::new(&[5_u8; 30])?;
    Ok((literals, distances))
}

fn dynamic_codes(bits: &mut Bits<'_>) -> Result<(Huffman, Huffman), InflateError> {
    let literal_count = bits.take(5)? as usize + 257;
    let distance_count = bits.take(5)? as usize + 1;
    let code_count = bits.take(4)? as usize + 4;
    if literal_count > 286 || distance_count > 30 {
        return Err(InflateError::BadCode);
    }
    let mut code_lengths = [0_u8; 19];
    for &slot in &CODE_LENGTH_ORDER[..code_count] {
        code_lengths[slot] = u8::try_from(bits.take(3)?).map_err(|_| InflateError::BadCode)?;
    }
    let (length_code, unused) = Huffman::new(&code_lengths)?;
    if unused != 0 {
        return Err(InflateError::BadCode);
    }
    let total = literal_count + distance_count;
    let mut lengths = vec![0_u8; total];
    let mut at = 0;
    while at < total {
        let symbol = length_code.decode(bits)?;
        if symbol < 16 {
            lengths[at] = u8::try_from(symbol).map_err(|_| InflateError::BadCode)?;
            at += 1;
            continue;
        }
        let (value, repeat) = match symbol {
            16 => {
                let previous = *lengths
                    .get(at.wrapping_sub(1))
                    .filter(|_| at > 0)
                    .ok_or(InflateError::BadCode)?;
                (previous, 3 + bits.take(2)? as usize)
            }
            17 => (0, 3 + bits.take(3)? as usize),
            _ => (0, 11 + bits.take(7)? as usize),
        };
        if at + repeat > total {
            return Err(InflateError::BadCode);
        }
        lengths[at..at + repeat].fill(value);
        at += repeat;
    }
    // A block without an end-of-block code could never end.
    if lengths[256] == 0 {
        return Err(InflateError::BadCode);
    }
    let (literals, unused) = Huffman::new(&lengths[..literal_count])?;
    let single = usize::from(literals.counts[0]) + usize::from(literals.counts[1]);
    if unused != 0 && (unused < 0 || literal_count != single) {
        return Err(InflateError::BadCode);
    }
    let (distances, unused) = Huffman::new(&lengths[literal_count..])?;
    let single = usize::from(distances.counts[0]) + usize::from(distances.counts[1]);
    if unused != 0 && (unused < 0 || distance_count != single) {
        return Err(InflateError::BadCode);
    }
    Ok((literals, distances))
}

fn codes(
    bits: &mut Bits<'_>,
    out: &mut Vec<u8>,
    limit: usize,
    literals: &Huffman,
    distances: &Huffman,
) -> Result<(), InflateError> {
    loop {
        let symbol = literals.decode(bits)?;
        match symbol {
            0..=255 => {
                if out.len() + 1 > limit {
                    return Err(InflateError::TooLarge { limit });
                }
                out.push(u8::try_from(symbol).map_err(|_| InflateError::BadSymbol)?);
            }
            256 => return Ok(()),
            257..=285 => {
                let slot = usize::from(symbol - 257);
                let length = usize::from(LENGTH_BASE[slot])
                    + bits.take(u32::from(LENGTH_EXTRA[slot]))? as usize;
                let symbol = usize::from(distances.decode(bits)?);
                if symbol >= DISTANCE_BASE.len() {
                    return Err(InflateError::BadSymbol);
                }
                let distance = usize::from(DISTANCE_BASE[symbol])
                    + bits.take(u32::from(DISTANCE_EXTRA[symbol]))? as usize;
                if distance > out.len() {
                    return Err(InflateError::DistanceTooFar);
                }
                if out.len() + length > limit {
                    return Err(InflateError::TooLarge { limit });
                }
                let start = out.len() - distance;
                for offset in 0..length {
                    let byte = out[start + offset];
                    out.push(byte);
                }
            }
            _ => return Err(InflateError::BadSymbol),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("hex"))
            .collect()
    }

    /// Streams written by zlib 1.3 (`wbits=-15`, level 9) from known data.
    #[test]
    fn streams_zlib_wrote_inflate_to_their_data() {
        assert_eq!(
            inflate(&bytes("cb48cdc9c957c840277514d2f353f24b140a1293b301"), 1024)
                .expect("a fixed-code block"),
            b"hello hello hello hello, godot pack"
        );
        assert_eq!(
            inflate(&bytes("010d00f2ff73746f72656420627974657321"), 1024).expect("a stored block"),
            b"stored bytes!"
        );
        assert_eq!(inflate(&bytes("0300"), 1024).expect("an empty stream"), b"");
        let multi =
            bytes("4acb2c2a2e5148cac94fce5648c3c106000000ffff2b4e4dcecf4b5148cac94fce5628c6c90100");
        assert_eq!(
            inflate(&multi, 1024).expect("three blocks"),
            b"first block first block first block second block second block second block "
        );
    }

    #[test]
    fn a_dynamic_block_inflates_to_its_data() {
        let compressed = bytes(concat!(
            "9dd6595203310c45d17f56e125c4833cb01b860602210d8130ad9e827e5ac0f577ea9565eb489dc3feb884dd65787f58",
            "c2eb797ff314ae4febe731dcad5fe1f1fcfcf216d68fe5f4fff3e1eae73bdcaef7617771f84b45968a5b2ab154d95299",
            "a5c6962ab0c2bac58cc5926db1ca6259a735f820ba5b67b1aa871c2cd6d5b5088964c5a091a42a2355d294834e5a520e",
            "4af1189492fd352195eae7412b433223c492bd7d504bd36648504bd4fd12e462aa33412e43a3972097229e097219befb",
            "a097e2f7835e86d709bd98f70f7ae98a412e553c33e49234ed1972e9aa33432ee61f21c8258975865cba567c865caa98",
            "65fa25f23e402ed1cfa35c347e997a51ae402f457d2fd04b56ff0af412fd3ce8c5af47b78bb644815cbada5e2097e6ff",
            "ae2097e6e7412ecdef37e672b69babd3e2dcbb589aeb83e5b9be5b99626636a7daeadc14599b9b5aeb735bc206de4abf",
        ));
        let expected: Vec<u8> = (0..60)
            .flat_map(|line: u32| {
                format!(
                    "line {line}: the quick brown fox jumps over the lazy dog {}\n",
                    line * line % 97
                )
                .into_bytes()
            })
            .collect();
        assert_eq!((compressed[0] >> 1) & 3, 2, "the vector is a dynamic block");
        assert_eq!(
            inflate(&compressed, 4096).expect("a dynamic block"),
            expected
        );
    }

    #[test]
    fn a_stream_that_would_grow_past_its_bound_is_refused() {
        let fixed = bytes("cb48cdc9c957c840277514d2f353f24b140a1293b301");
        assert_eq!(
            inflate(&fixed, 10),
            Err(InflateError::TooLarge { limit: 10 })
        );
        assert_eq!(
            inflate(&bytes("010d00f2ff73746f72656420627974657321"), 12),
            Err(InflateError::TooLarge { limit: 12 })
        );
    }

    #[test]
    fn a_truncated_or_damaged_stream_is_refused() {
        let fixed = bytes("cb48cdc9c957c840277514d2f353f24b140a1293b301");
        for length in 0..fixed.len() - 1 {
            assert!(
                inflate(&fixed[..length], 1024).is_err(),
                "a stream cut at {length} bytes"
            );
        }
        assert_eq!(
            inflate(&bytes("010d00f2fe73746f72656420627974657321"), 1024),
            Err(InflateError::StoredLength)
        );
        assert_eq!(inflate(&[0x07], 1024), Err(InflateError::ReservedBlock));
        // A fixed block whose first symbol is a back-reference to nothing.
        assert_eq!(
            inflate(&[0x03, 0x02], 1024).err(),
            Some(InflateError::DistanceTooFar)
        );
    }
}
