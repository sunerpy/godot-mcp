//! Deterministic, dependency-free archive writers and a matching extractor.
//!
//! Release archives must be byte-reproducible, so every field that an ordinary
//! archiver fills from the ambient environment is pinned here instead:
//!
//! * entry order is the caller's order (the packager sorts by path);
//! * mtime is 0, uid/gid are 0 and the owner/group names are empty;
//! * the gzip header carries no name, no mtime and the "unknown" OS byte;
//! * the deflate payload uses **stored** blocks only, so compression can never
//!   introduce an implementation-dependent byte;
//! * ZIP entries use a fixed 1980-01-01 DOS timestamp and the stored method.
//!
//! Nothing here shells out to `tar`/`zip` and nothing here adds a dependency:
//! the release archive must not depend on whatever archiver happens to be
//! installed on a build host.
//!
//! [`extract_tar_gz`] deliberately supports exactly the gzip flavour
//! [`write_tar_gz`] emits (stored deflate blocks). A dynamic-Huffman member is
//! rejected with an error rather than silently mis-decoded, because the only
//! archives this extractor is asked to open are the ones this module wrote.

use std::io;
use std::path::{Component, Path, PathBuf};

/// One file destined for an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    /// Archive-relative path, always `/`-separated.
    pub path: String,
    /// File contents.
    pub bytes: Vec<u8>,
    /// Unix permission bits (only `0o755` and `0o644` are used by the packager).
    pub mode: u32,
}

const CRC32_POLYNOMIAL: u32 = 0xEDB8_8320;

fn crc32(bytes: &[u8]) -> u32 {
    let mut table = [0_u32; 256];
    let mut index = 0_usize;
    while index < 256 {
        let mut value = u32::try_from(index).unwrap_or(0);
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 == 1 {
                (value >> 1) ^ CRC32_POLYNOMIAL
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    let mut crc = 0xFFFF_FFFF_u32;
    for byte in bytes {
        crc = table[usize::from((crc as u8) ^ *byte)] ^ (crc >> 8);
    }
    !crc
}

// ---------------------------------------------------------------------------
// tar (ustar)
// ---------------------------------------------------------------------------

fn octal_field(out: &mut Vec<u8>, value: u64, width: usize) {
    // `width - 1` octal digits followed by a NUL, which is what every ustar
    // reader accepts and what keeps the encoding deterministic.
    let text = format!("{value:0width$o}", width = width - 1);
    out.extend_from_slice(text.as_bytes());
    out.push(0);
}

fn pad_to(out: &mut Vec<u8>, target: usize) {
    while out.len() < target {
        out.push(0);
    }
}

/// Build the 512-byte ustar header for one regular file.
fn tar_header(path: &str, size: u64, mode: u32) -> Result<Vec<u8>, String> {
    if path.len() > 100 {
        return Err(format!(
            "archive entry path is longer than the 100-byte ustar name field: {path}"
        ));
    }
    let mut header = Vec::with_capacity(512);
    header.extend_from_slice(path.as_bytes());
    pad_to(&mut header, 100);
    octal_field(&mut header, u64::from(mode & 0o7777), 8);
    octal_field(&mut header, 0, 8); // uid
    octal_field(&mut header, 0, 8); // gid
    octal_field(&mut header, size, 12);
    octal_field(&mut header, 0, 12); // mtime
    // Checksum placeholder: eight spaces while the sum is computed.
    header.extend_from_slice(b"        ");
    header.push(b'0'); // typeflag: regular file
    pad_to(&mut header, 257); // linkname
    header.extend_from_slice(b"ustar\0");
    header.extend_from_slice(b"00");
    pad_to(&mut header, 512); // uname/gname/dev*/prefix all empty

    let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
    let mut encoded = format!("{checksum:06o}").into_bytes();
    encoded.push(0);
    encoded.push(b' ');
    header[148..156].copy_from_slice(&encoded);
    Ok(header)
}

/// Serialize `entries` as an uncompressed ustar stream.
///
/// # Errors
/// Returns an error if an entry path does not fit the ustar name field.
pub fn write_tar(entries: &[ArchiveEntry]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for entry in entries {
        let size = u64::try_from(entry.bytes.len())
            .map_err(|_| format!("archive entry {} is too large", entry.path))?;
        out.extend_from_slice(&tar_header(&entry.path, size, entry.mode)?);
        out.extend_from_slice(&entry.bytes);
        let remainder = entry.bytes.len() % 512;
        if remainder != 0 {
            out.extend(std::iter::repeat_n(0_u8, 512 - remainder));
        }
    }
    // Two zero blocks terminate the archive.
    out.extend(std::iter::repeat_n(0_u8, 1024));
    Ok(out)
}

// ---------------------------------------------------------------------------
// gzip (stored deflate)
// ---------------------------------------------------------------------------

/// Wrap `payload` in a gzip member built from stored deflate blocks.
#[must_use]
pub fn write_gzip(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 64);
    // ID1 ID2 CM FLG MTIME(4)=0 XFL=0 OS=255(unknown).
    out.extend_from_slice(&[0x1F, 0x8B, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xFF]);

    let mut offset = 0_usize;
    loop {
        let remaining = payload.len() - offset;
        let chunk = remaining.min(0xFFFF);
        let final_block = offset + chunk >= payload.len();
        out.push(u8::from(final_block)); // BFINAL, BTYPE=00 (stored)
        let length = u16::try_from(chunk).unwrap_or(u16::MAX);
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&(!length).to_le_bytes());
        out.extend_from_slice(&payload[offset..offset + chunk]);
        offset += chunk;
        if final_block {
            break;
        }
    }
    out.extend_from_slice(&crc32(payload).to_le_bytes());
    let size = u32::try_from(payload.len() % (1_u64 << 32) as usize).unwrap_or(0);
    out.extend_from_slice(&size.to_le_bytes());
    out
}

/// Produce a deterministic `.tar.gz` byte stream from `entries`.
///
/// # Errors
/// Returns an error if the tar layer rejects an entry path.
pub fn write_tar_gz(entries: &[ArchiveEntry]) -> Result<Vec<u8>, String> {
    Ok(write_gzip(&write_tar(entries)?))
}

/// Undo [`write_gzip`], accepting only the stored-block flavour it emits.
///
/// # Errors
/// Returns an error on a malformed member, an unsupported flag, a compressed
/// (non-stored) deflate block, or a CRC/length mismatch.
pub fn read_gzip(member: &[u8]) -> Result<Vec<u8>, String> {
    if member.len() < 18 {
        return Err("gzip member is truncated".to_string());
    }
    if member[0] != 0x1F || member[1] != 0x8B {
        return Err("gzip magic is missing".to_string());
    }
    if member[2] != 0x08 {
        return Err("gzip compression method is not deflate".to_string());
    }
    if member[3] != 0x00 {
        return Err("gzip member carries flags this reader does not accept".to_string());
    }
    let mut cursor = 10_usize;
    let mut payload = Vec::new();
    loop {
        if cursor >= member.len() {
            return Err("gzip deflate stream is truncated".to_string());
        }
        let header = member[cursor];
        let block_type = (header >> 1) & 0b11;
        if block_type != 0 {
            return Err(
                "gzip member uses a compressed deflate block; only stored blocks are supported"
                    .to_string(),
            );
        }
        let final_block = header & 1 == 1;
        cursor += 1;
        if cursor + 4 > member.len() {
            return Err("gzip stored block header is truncated".to_string());
        }
        let length = usize::from(u16::from_le_bytes([member[cursor], member[cursor + 1]]));
        let inverse = u16::from_le_bytes([member[cursor + 2], member[cursor + 3]]);
        if u16::try_from(length).map(|value| !value) != Ok(inverse) {
            return Err("gzip stored block length complement mismatch".to_string());
        }
        cursor += 4;
        if cursor + length > member.len() {
            return Err("gzip stored block payload is truncated".to_string());
        }
        payload.extend_from_slice(&member[cursor..cursor + length]);
        cursor += length;
        if final_block {
            break;
        }
    }
    if cursor + 8 > member.len() {
        return Err("gzip trailer is truncated".to_string());
    }
    let recorded_crc = u32::from_le_bytes([
        member[cursor],
        member[cursor + 1],
        member[cursor + 2],
        member[cursor + 3],
    ]);
    let recorded_size = u32::from_le_bytes([
        member[cursor + 4],
        member[cursor + 5],
        member[cursor + 6],
        member[cursor + 7],
    ]);
    if recorded_crc != crc32(&payload) {
        return Err("gzip CRC-32 mismatch".to_string());
    }
    if u64::from(recorded_size) != payload.len() as u64 % (1_u64 << 32) {
        return Err("gzip ISIZE mismatch".to_string());
    }
    Ok(payload)
}

/// Parse an uncompressed ustar stream back into entries.
///
/// # Errors
/// Returns an error on a truncated stream or a bad header checksum.
pub fn read_tar(stream: &[u8]) -> Result<Vec<ArchiveEntry>, String> {
    let mut entries = Vec::new();
    let mut cursor = 0_usize;
    while cursor + 512 <= stream.len() {
        let header = &stream[cursor..cursor + 512];
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let mut zeroed = header.to_vec();
        zeroed[148..156].copy_from_slice(b"        ");
        let expected: u32 = zeroed.iter().map(|byte| u32::from(*byte)).sum();
        let recorded = parse_octal(&header[148..156])?;
        if u64::from(expected) != recorded {
            return Err("tar header checksum mismatch".to_string());
        }
        let name_end = header[..100]
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(100);
        let path = String::from_utf8(header[..name_end].to_vec())
            .map_err(|_| "tar entry name is not utf-8".to_string())?;
        let mode = u32::try_from(parse_octal(&header[100..108])?)
            .map_err(|_| "tar entry mode is out of range".to_string())?;
        let size = usize::try_from(parse_octal(&header[124..136])?)
            .map_err(|_| "tar entry size is out of range".to_string())?;
        cursor += 512;
        if cursor + size > stream.len() {
            return Err(format!("tar entry {path} payload is truncated"));
        }
        entries.push(ArchiveEntry {
            path,
            bytes: stream[cursor..cursor + size].to_vec(),
            mode,
        });
        cursor += size;
        let remainder = size % 512;
        if remainder != 0 {
            cursor += 512 - remainder;
        }
    }
    Ok(entries)
}

fn parse_octal(field: &[u8]) -> Result<u64, String> {
    let text: String = field
        .iter()
        .take_while(|byte| **byte != 0 && **byte != b' ')
        .map(|byte| char::from(*byte))
        .collect();
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(&text, 8).map_err(|error| format!("tar numeric field {text:?}: {error}"))
}

/// Reject a path that would escape the extraction root.
///
/// The raw segments are checked, not `Path::components()`: that iterator silently
/// normalizes `.` away, and an archive-slip check must reject exactly what the
/// archive says rather than a normalized reading of it.
fn safe_relative(path: &str) -> Result<PathBuf, String> {
    if path.contains('\\') {
        return Err(format!("archive entry {path} contains a backslash"));
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(format!(
                "archive entry {path} contains a non-relative component"
            ));
        }
    }
    let candidate = PathBuf::from(path);
    if candidate
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "archive entry {path} contains a non-relative component"
        ));
    }
    Ok(candidate)
}

/// Extract a `.tar.gz` produced by [`write_tar_gz`] into `destination`,
/// recreating parent directories and applying the recorded mode on Unix.
///
/// # Errors
/// Returns an error if the archive is malformed, an entry path is not strictly
/// relative, or a file cannot be written.
pub fn extract_tar_gz(archive: &[u8], destination: &Path) -> Result<Vec<PathBuf>, String> {
    let tar = read_gzip(archive)?;
    let entries = read_tar(&tar)?;
    let mut written = Vec::with_capacity(entries.len());
    for entry in &entries {
        let relative = safe_relative(&entry.path)?;
        let target = destination.join(&relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        std::fs::write(&target, &entry.bytes)
            .map_err(|error| format!("write {}: {error}", target.display()))?;
        apply_mode(&target, entry.mode)?;
        written.push(target);
    }
    Ok(written)
}

/// Extract a `.zip` produced by [`write_zip`] into `destination`, recreating
/// parent directories, exactly as [`extract_tar_gz`] does for its format.
///
/// # Errors
/// Returns an error if the archive is malformed, an entry path is not strictly
/// relative, or a file cannot be written.
pub fn extract_zip(archive: &[u8], destination: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = read_zip(archive)?;
    let mut written = Vec::with_capacity(entries.len());
    for entry in &entries {
        let relative = safe_relative(&entry.path)?;
        let target = destination.join(&relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        std::fs::write(&target, &entry.bytes)
            .map_err(|error| format!("write {}: {error}", target.display()))?;
        apply_mode(&target, entry.mode)?;
        written.push(target);
    }
    Ok(written)
}

#[cfg(unix)]
fn apply_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("set mode on {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn apply_mode(_path: &Path, _mode: u32) -> Result<(), String> {
    // Windows has no POSIX mode bits; the executable bit is implied by `.exe`.
    Ok(())
}

// ---------------------------------------------------------------------------
// zip (stored)
// ---------------------------------------------------------------------------

/// Fixed DOS timestamp: 1980-01-01 00:00:00, the earliest value the format can
/// represent, so no build clock leaks into the archive.
const DOS_DATE: u16 = 0x0021;
const DOS_TIME: u16 = 0x0000;

/// Serialize `entries` as a deterministic ZIP archive using the stored method.
///
/// The Windows release archive uses this format; the writer itself is
/// platform-independent and is exercised on every host.
///
/// # Errors
/// Returns an error if an entry is too large for the 32-bit ZIP fields.
pub fn write_zip(entries: &[ArchiveEntry]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut directory = Vec::new();
    for entry in entries {
        let size = u32::try_from(entry.bytes.len())
            .map_err(|_| format!("archive entry {} exceeds the zip size field", entry.path))?;
        let name = entry.path.as_bytes();
        let name_length = u16::try_from(name.len())
            .map_err(|_| format!("archive entry {} has an over-long name", entry.path))?;
        let crc = crc32(&entry.bytes);
        let offset = u32::try_from(out.len())
            .map_err(|_| "zip archive exceeds the 32-bit offset field".to_string())?;

        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20_u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0_u16.to_le_bytes()); // flags
        out.extend_from_slice(&0_u16.to_le_bytes()); // method: stored
        out.extend_from_slice(&DOS_TIME.to_le_bytes());
        out.extend_from_slice(&DOS_DATE.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&name_length.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes()); // extra length
        out.extend_from_slice(name);
        out.extend_from_slice(&entry.bytes);

        directory.extend_from_slice(b"PK\x01\x02");
        directory.extend_from_slice(&0x031E_u16.to_le_bytes()); // made by: unix, 3.0
        directory.extend_from_slice(&20_u16.to_le_bytes());
        directory.extend_from_slice(&0_u16.to_le_bytes());
        directory.extend_from_slice(&0_u16.to_le_bytes());
        directory.extend_from_slice(&DOS_TIME.to_le_bytes());
        directory.extend_from_slice(&DOS_DATE.to_le_bytes());
        directory.extend_from_slice(&crc.to_le_bytes());
        directory.extend_from_slice(&size.to_le_bytes());
        directory.extend_from_slice(&size.to_le_bytes());
        directory.extend_from_slice(&name_length.to_le_bytes());
        directory.extend_from_slice(&0_u16.to_le_bytes()); // extra
        directory.extend_from_slice(&0_u16.to_le_bytes()); // comment
        directory.extend_from_slice(&0_u16.to_le_bytes()); // disk number
        directory.extend_from_slice(&0_u16.to_le_bytes()); // internal attrs
        directory.extend_from_slice(&((entry.mode & 0o7777) << 16).to_le_bytes());
        directory.extend_from_slice(&offset.to_le_bytes());
        directory.extend_from_slice(name);
    }

    let directory_offset = u32::try_from(out.len())
        .map_err(|_| "zip central directory offset overflow".to_string())?;
    let directory_size = u32::try_from(directory.len())
        .map_err(|_| "zip central directory size overflow".to_string())?;
    let count = u16::try_from(entries.len()).map_err(|_| "zip entry count overflow".to_string())?;
    out.extend_from_slice(&directory);
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&0_u16.to_le_bytes()); // disk
    out.extend_from_slice(&0_u16.to_le_bytes()); // start disk
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&directory_size.to_le_bytes());
    out.extend_from_slice(&directory_offset.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes()); // comment length
    Ok(out)
}

/// Read back the entry names of a ZIP written by [`write_zip`], so the audit can
/// inspect a Windows archive's contents on any host.
///
/// # Errors
/// Returns an error if the archive is not the stored-method layout this module
/// writes.
pub fn read_zip(archive: &[u8]) -> Result<Vec<ArchiveEntry>, String> {
    let mut entries = Vec::new();
    let mut cursor = 0_usize;
    while cursor + 30 <= archive.len() && &archive[cursor..cursor + 4] == b"PK\x03\x04" {
        let size = usize::try_from(u32::from_le_bytes([
            archive[cursor + 18],
            archive[cursor + 19],
            archive[cursor + 20],
            archive[cursor + 21],
        ]))
        .map_err(|_| "zip entry size is out of range".to_string())?;
        let name_length = usize::from(u16::from_le_bytes([
            archive[cursor + 26],
            archive[cursor + 27],
        ]));
        let extra_length = usize::from(u16::from_le_bytes([
            archive[cursor + 28],
            archive[cursor + 29],
        ]));
        let name_start = cursor + 30;
        let data_start = name_start + name_length + extra_length;
        if data_start + size > archive.len() {
            return Err("zip local entry is truncated".to_string());
        }
        let path = String::from_utf8(archive[name_start..name_start + name_length].to_vec())
            .map_err(|_| "zip entry name is not utf-8".to_string())?;
        entries.push(ArchiveEntry {
            path,
            bytes: archive[data_start..data_start + size].to_vec(),
            mode: 0o644,
        });
        cursor = data_start + size;
    }
    if entries.is_empty() && !archive.is_empty() {
        return Err("zip archive has no stored local entries".to_string());
    }
    Ok(entries)
}

/// Read every file under `root`, returning `/`-separated relative paths.
///
/// # Errors
/// Returns an I/O error if the tree cannot be walked or a file cannot be read.
pub fn collect_tree(root: &Path, prefix: &str) -> io::Result<Vec<ArchiveEntry>> {
    let mut entries = Vec::new();
    let mut frontier = vec![root.to_path_buf()];
    while let Some(directory) = frontier.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                frontier.push(path);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| io::Error::other("tree walk escaped its root"))?
                    .to_string_lossy()
                    .replace('\\', "/");
                entries.push(ArchiveEntry {
                    path: format!("{prefix}/{relative}"),
                    bytes: std::fs::read(&path)?,
                    mode: 0o644,
                });
            }
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}
