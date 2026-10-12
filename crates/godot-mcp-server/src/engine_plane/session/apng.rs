//! Assembling a recording's PNG frames into one animated PNG.
//!
//! The runtime probe encodes every recorded frame as an ordinary PNG. An
//! animated PNG is the first frame's PNG with an `acTL` chunk after `IHDR`,
//! an `fcTL` frame control chunk before each frame's image data, and every
//! later frame's `IDAT` data carried as sequence-numbered `fdAT` chunks. No
//! pixel is decoded or re-encoded, so the frames' bytes are exactly the ones
//! the probe hashed.

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// One chunk of a PNG stream.
struct Chunk<'a> {
    kind: [u8; 4],
    data: &'a [u8],
}

fn chunks(png: &[u8]) -> Result<Vec<Chunk<'_>>, String> {
    let body = png
        .strip_prefix(&SIGNATURE)
        .ok_or_else(|| "a frame is not a PNG".to_owned())?;
    let mut chunks = Vec::new();
    let mut at = 0_usize;
    while at < body.len() {
        let header = body
            .get(at..at + 8)
            .ok_or_else(|| "a frame has a truncated chunk header".to_owned())?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let kind = [header[4], header[5], header[6], header[7]];
        let data = body
            .get(at + 8..at + 8 + length)
            .ok_or_else(|| "a frame has a truncated chunk".to_owned())?;
        at += 12 + length;
        if at > body.len() {
            return Err("a frame has a truncated chunk checksum".to_owned());
        }
        chunks.push(Chunk { kind, data });
        if &kind == b"IEND" {
            break;
        }
    }
    Ok(chunks)
}

fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for part in parts {
        for byte in *part {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
    }
    !crc
}

fn push_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[kind, data]).to_be_bytes());
}

fn frame_control(sequence: u32, width: u32, height: u32, fps: u16) -> Vec<u8> {
    let mut data = Vec::with_capacity(26);
    data.extend_from_slice(&sequence.to_be_bytes());
    data.extend_from_slice(&width.to_be_bytes());
    data.extend_from_slice(&height.to_be_bytes());
    data.extend_from_slice(&0_u32.to_be_bytes());
    data.extend_from_slice(&0_u32.to_be_bytes());
    data.extend_from_slice(&1_u16.to_be_bytes());
    data.extend_from_slice(&fps.to_be_bytes());
    data.push(0);
    data.push(0);
    data
}

/// Assemble `frames` (complete PNGs of one size and format) into an animated
/// PNG that shows each for `1 / fps` seconds and loops forever.
///
/// # Errors
/// Returns why when there is no frame, a frame is not a well-formed PNG, or
/// the frames differ in size or pixel format.
pub(crate) fn assemble(frames: &[Vec<u8>], fps: u16) -> Result<Vec<u8>, String> {
    let first = frames
        .first()
        .ok_or_else(|| "a recording needs at least one frame".to_owned())?;
    let first_chunks = chunks(first)?;
    let header = first_chunks
        .iter()
        .find(|chunk| &chunk.kind == b"IHDR")
        .ok_or_else(|| "the first frame has no IHDR".to_owned())?;
    if header.data.len() != 13 {
        return Err("the first frame's IHDR is malformed".to_owned());
    }
    let width = u32::from_be_bytes([
        header.data[0],
        header.data[1],
        header.data[2],
        header.data[3],
    ]);
    let height = u32::from_be_bytes([
        header.data[4],
        header.data[5],
        header.data[6],
        header.data[7],
    ]);
    let count = u32::try_from(frames.len()).map_err(|_| "too many frames".to_owned())?;
    let fps = fps.max(1);

    let mut out = Vec::with_capacity(frames.iter().map(Vec::len).sum::<usize>() + 64);
    out.extend_from_slice(&SIGNATURE);
    push_chunk(&mut out, b"IHDR", header.data);
    let mut animation = Vec::with_capacity(8);
    animation.extend_from_slice(&count.to_be_bytes());
    animation.extend_from_slice(&0_u32.to_be_bytes());
    push_chunk(&mut out, b"acTL", &animation);
    // Ancillary chunks of the first frame that precede its image data (colour
    // space, gamma, ...) apply to the whole animation.
    for chunk in &first_chunks {
        match &chunk.kind {
            b"IHDR" | b"IDAT" | b"IEND" => {}
            kind if kind[0].is_ascii_lowercase() => push_chunk(&mut out, kind, chunk.data),
            b"PLTE" => push_chunk(&mut out, b"PLTE", chunk.data),
            _ => {}
        }
    }
    let mut sequence = 0_u32;
    for (index, frame) in frames.iter().enumerate() {
        let frame_chunks = if index == 0 {
            None
        } else {
            Some(chunks(frame)?)
        };
        let parsed = frame_chunks.as_ref().unwrap_or(&first_chunks);
        let frame_header = parsed
            .iter()
            .find(|chunk| &chunk.kind == b"IHDR")
            .ok_or_else(|| format!("frame {index} has no IHDR"))?;
        if frame_header.data != header.data {
            return Err(format!(
                "frame {index} differs from the first frame in size or pixel format"
            ));
        }
        push_chunk(
            &mut out,
            b"fcTL",
            &frame_control(sequence, width, height, fps),
        );
        sequence += 1;
        for chunk in parsed.iter().filter(|chunk| &chunk.kind == b"IDAT") {
            if index == 0 {
                push_chunk(&mut out, b"IDAT", chunk.data);
            } else {
                let mut data = Vec::with_capacity(chunk.data.len() + 4);
                data.extend_from_slice(&sequence.to_be_bytes());
                data.extend_from_slice(chunk.data);
                push_chunk(&mut out, b"fdAT", &data);
                sequence += 1;
            }
        }
    }
    push_chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal 1x1 RGB PNG with one IDAT chunk holding `idat`.
    fn png(width: u32, idat: &[u8]) -> Vec<u8> {
        let mut out = SIGNATURE.to_vec();
        let mut header = Vec::new();
        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&1_u32.to_be_bytes());
        header.extend_from_slice(&[8, 2, 0, 0, 0]);
        push_chunk(&mut out, b"IHDR", &header);
        push_chunk(&mut out, b"sRGB", &[0]);
        push_chunk(&mut out, b"IDAT", idat);
        push_chunk(&mut out, b"IEND", &[]);
        out
    }

    #[test]
    fn the_crc_is_the_png_crc() {
        // The IEND chunk's CRC is fixed by the PNG specification.
        assert_eq!(crc32(&[b"IEND"]), 0xae42_6082);
    }

    #[test]
    fn frames_become_one_animation_in_order() {
        let frames = vec![png(1, b"first"), png(1, b"second"), png(1, b"third")];
        let animated = assemble(&frames, 10).expect("assemble");
        let parsed = chunks(&animated).expect("the animation is a PNG stream");
        let kinds: Vec<String> = parsed
            .iter()
            .map(|chunk| String::from_utf8_lossy(&chunk.kind).into_owned())
            .collect();
        assert_eq!(
            kinds,
            [
                "IHDR", "acTL", "sRGB", "fcTL", "IDAT", "fcTL", "fdAT", "fcTL", "fdAT", "IEND"
            ]
        );
        assert_eq!(&parsed[1].data[..4], &3_u32.to_be_bytes(), "three frames");
        assert_eq!(parsed[4].data, b"first");
        // Sequence numbers run 0..5 across fcTL and fdAT.
        let sequences: Vec<u32> = parsed
            .iter()
            .filter(|chunk| &chunk.kind == b"fcTL" || &chunk.kind == b"fdAT")
            .map(|chunk| u32::from_be_bytes(chunk.data[..4].try_into().expect("sequence")))
            .collect();
        assert_eq!(sequences, [0, 1, 2, 3, 4]);
        assert_eq!(&parsed[6].data[4..], b"second");
    }

    #[test]
    fn frames_of_another_size_are_refused() {
        let refused = assemble(&[png(1, b"a"), png(2, b"b")], 10).expect_err("mismatch");
        assert!(refused.contains("frame 1"), "{refused}");
        assert!(assemble(&[], 10).is_err());
        assert!(assemble(&[b"not a png".to_vec()], 10).is_err());
    }
}
