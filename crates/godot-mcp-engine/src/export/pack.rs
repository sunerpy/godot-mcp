//! Reading the data packs Godot exports: `--export-pack` and `--export-patch`.
//!
//! A pack is a PCK file or a ZIP archive. The server reads what Godot wrote
//! itself, rather than trusting the exit status, before it answers what a pack
//! holds. A PCK starts with the magic `GDPC`, its format version and the
//! engine version that wrote it, its flags and the offset its file data is
//! relative to; formats 3 and later add the offset of the directory, which
//! format 2 keeps right after the header. The directory lists every file:
//! its path, offset, size, MD5 and flags (encrypted, a removal a patch
//! records, or a delta against a base pack's copy).
//!
//! Measured on the locked builds: Godot 4.4.1 writes format 2, 4.5.1 and
//! 4.6.1 format 3, and 4.7.1 format 4, which lays out its header and
//! directory as format 3 does. Every entry's MD5 is the digest of the bytes
//! stored for it. A pack whose directory is encrypted cannot be read without
//! its key, so it is refused as unreadable rather than guessed at.
//!
//! The input was written by a process that ran project code: every count,
//! length and offset is checked against the file before it is used.

use std::collections::BTreeMap;

use super::bundle::{check_archive, crc32, entry_bytes};
use super::md5::{hex, md5};

/// The magic a PCK file starts with.
pub const PCK_MAGIC: [u8; 4] = *b"GDPC";
/// The newest PCK format this reader knows: Godot 4.7.1 writes it.
pub const NEWEST_FORMAT: u32 = 4;
/// The oldest PCK format this reader knows: Godot 4.4.1 writes it, and 4.5.1
/// and 4.6.1 write format 3.
pub const OLDEST_FORMAT: u32 = 2;

/// The pack's directory is encrypted.
pub const PACK_DIRECTORY_ENCRYPTED: u32 = 1 << 0;
/// File offsets are relative to the pack's own start.
pub const PACK_RELATIVE_FILE_BASE: u32 = 1 << 1;
/// The pack lists files whose data is stored outside it.
pub const PACK_SPARSE_BUNDLE: u32 = 1 << 2;
const KNOWN_PACK_FLAGS: u32 =
    PACK_DIRECTORY_ENCRYPTED | PACK_RELATIVE_FILE_BASE | PACK_SPARSE_BUNDLE;

/// The file's data is encrypted.
pub const FILE_ENCRYPTED: u32 = 1 << 0;
/// The entry removes the file a base pack holds.
pub const FILE_REMOVAL: u32 = 1 << 1;
/// The entry's data is a delta against a base pack's copy.
pub const FILE_DELTA: u32 = 1 << 2;

/// The most entries one pack may list.
pub const MAX_ENTRIES: u32 = 1_000_000;
/// The longest path an entry may name, in bytes.
pub const MAX_PATH_BYTES: usize = 4096;
/// The smallest directory entry: a path length, an empty path, an offset, a
/// size, an MD5 and flags.
const MIN_ENTRY_BYTES: usize = 4 + 8 + 8 + 16 + 4;
/// The header of a format 2 pack, after which its directory starts.
const FORMAT_2_HEADER_BYTES: usize = 0x60;
/// The fixed part of a format 3 or later header.
const FORMAT_3_HEADER_BYTES: usize = 0x68;

/// Why a pack cannot be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PackError {
    /// The file is not a PCK.
    #[error("the file is not a Godot pack: it does not start with the GDPC magic")]
    NotAPack,
    /// A format this reader does not know.
    #[error(
        "the pack uses format {0}, which this server does not read (it reads formats \
         {OLDEST_FORMAT} to {NEWEST_FORMAT})"
    )]
    UnsupportedFormat(u32),
    /// The directory is encrypted.
    #[error("the pack's directory is encrypted, so what it holds cannot be read without its key")]
    EncryptedDirectory,
    /// Flags this reader does not know.
    #[error("the pack carries flags {0:#x} this server does not know")]
    UnknownFlags(u32),
    /// A record ends past the end of the file.
    #[error("the pack is truncated: its {0} ends past the end of the file")]
    Truncated(&'static str),
    /// More entries than [`MAX_ENTRIES`].
    #[error("the pack lists {0} files, more than the {MAX_ENTRIES} this server reads")]
    TooManyEntries(u32),
    /// An entry's path is not a usable path.
    #[error("the pack lists a path that is not a usable project path: {0}")]
    BadPath(String),
    /// An entry's data lies outside the file.
    #[error("the pack's data for `{0}` lies outside the file")]
    DataOutOfRange(String),
    /// A ZIP pack this server cannot read.
    #[error("the ZIP pack cannot be read: {0}")]
    Zip(String),
}

/// What checking an entry's stored bytes against its recorded digest found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestCheck {
    /// The stored bytes have the recorded digest.
    Verified,
    /// They do not.
    Mismatch,
    /// A removal stores no bytes.
    NoData,
    /// Encrypted data cannot be checked without its key.
    Encrypted,
    /// A sparse bundle keeps the data outside the pack.
    Outside,
}

impl DigestCheck {
    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Mismatch => "mismatch",
            Self::NoData => "no_data",
            Self::Encrypted => "encrypted",
            Self::Outside => "outside_pack",
        }
    }
}

/// One file a PCK lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackEntry {
    /// The project path, without `res://`.
    pub path: String,
    /// The offset recorded for its data, relative to the file base.
    pub offset: u64,
    /// The bytes stored for it.
    pub size: u64,
    /// The MD5 recorded for it.
    pub md5: [u8; 16],
    /// Its file flags.
    pub flags: u32,
    /// What its stored bytes are against its MD5.
    pub digest: DigestCheck,
}

impl PackEntry {
    /// Whether the entry removes a base pack's file.
    #[must_use]
    pub const fn removal(&self) -> bool {
        self.flags & FILE_REMOVAL != 0
    }

    /// Whether the entry stores a delta against a base pack's copy.
    #[must_use]
    pub const fn delta(&self) -> bool {
        self.flags & FILE_DELTA != 0
    }

    /// Whether the entry's data is encrypted.
    #[must_use]
    pub const fn encrypted(&self) -> bool {
        self.flags & FILE_ENCRYPTED != 0
    }

    /// The recorded MD5, in hexadecimal.
    #[must_use]
    pub fn md5_hex(&self) -> String {
        hex(&self.md5)
    }
}

/// A read PCK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pack {
    /// The pack format version.
    pub format_version: u32,
    /// The engine version that wrote it.
    pub engine_version: (u32, u32, u32),
    /// The pack flags.
    pub flags: u32,
    /// The offset file data is relative to.
    pub file_base: u64,
    /// Where the directory starts, for format 3 and later.
    pub directory_offset: Option<u64>,
    /// Every listed file, in directory order.
    pub entries: Vec<PackEntry>,
}

impl Pack {
    /// The entries whose stored bytes do not have their recorded digest.
    #[must_use]
    pub fn mismatches(&self) -> Vec<&PackEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.digest == DigestCheck::Mismatch)
            .collect()
    }
}

fn u32_at(bytes: &[u8], at: usize, what: &'static str) -> Result<u32, PackError> {
    bytes
        .get(at..at + 4)
        .map(|field| u32::from_le_bytes([field[0], field[1], field[2], field[3]]))
        .ok_or(PackError::Truncated(what))
}

fn u64_at(bytes: &[u8], at: usize, what: &'static str) -> Result<u64, PackError> {
    bytes
        .get(at..at + 8)
        .map(|field| {
            let mut word = [0_u8; 8];
            word.copy_from_slice(field);
            u64::from_le_bytes(word)
        })
        .ok_or(PackError::Truncated(what))
}

/// The path an entry names, without its NUL padding and `res://` prefix.
fn entry_path(raw: &[u8]) -> Result<String, PackError> {
    let end = raw.iter().position(|&byte| byte == 0).unwrap_or(raw.len());
    let text = std::str::from_utf8(&raw[..end])
        .map_err(|_| PackError::BadPath(String::from_utf8_lossy(&raw[..end]).into_owned()))?;
    let path = text.strip_prefix("res://").unwrap_or(text);
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.contains(['\\', '\0'])
        || path.split('/').any(|part| part.is_empty() || part == "..");
    if bad {
        return Err(PackError::BadPath(text.chars().take(256).collect()));
    }
    Ok(path.to_owned())
}

/// Read the PCK `bytes`: its header, its directory, and each entry's stored
/// bytes against its recorded MD5.
///
/// # Errors
/// Returns [`PackError`] for a file that is not a PCK of a known format, an
/// encrypted directory, unknown flags, a truncated file or an entry whose path
/// or data range is unusable.
pub fn read_pack(bytes: &[u8]) -> Result<Pack, PackError> {
    if bytes.get(..4) != Some(&PCK_MAGIC[..]) {
        return Err(PackError::NotAPack);
    }
    let format_version = u32_at(bytes, 4, "header")?;
    if !(OLDEST_FORMAT..=NEWEST_FORMAT).contains(&format_version) {
        return Err(PackError::UnsupportedFormat(format_version));
    }
    let engine_version = (
        u32_at(bytes, 8, "header")?,
        u32_at(bytes, 12, "header")?,
        u32_at(bytes, 16, "header")?,
    );
    let flags = u32_at(bytes, 20, "header")?;
    if flags & !KNOWN_PACK_FLAGS != 0 {
        return Err(PackError::UnknownFlags(flags & !KNOWN_PACK_FLAGS));
    }
    if flags & PACK_DIRECTORY_ENCRYPTED != 0 {
        return Err(PackError::EncryptedDirectory);
    }
    let file_base = u64_at(bytes, 24, "header")?;
    let (directory_offset, directory) = if format_version == 2 {
        if bytes.len() < FORMAT_2_HEADER_BYTES {
            return Err(PackError::Truncated("header"));
        }
        (None, FORMAT_2_HEADER_BYTES)
    } else {
        if bytes.len() < FORMAT_3_HEADER_BYTES {
            return Err(PackError::Truncated("header"));
        }
        let offset = u64_at(bytes, 32, "header")?;
        let at = usize::try_from(offset).map_err(|_| PackError::Truncated("directory"))?;
        if at < FORMAT_3_HEADER_BYTES {
            return Err(PackError::Truncated("directory"));
        }
        (Some(offset), at)
    };
    let count = u32_at(bytes, directory, "directory")?;
    if count > MAX_ENTRIES {
        return Err(PackError::TooManyEntries(count));
    }
    let count = usize::try_from(count).map_err(|_| PackError::TooManyEntries(count))?;
    let mut at = directory + 4;
    if bytes.len().saturating_sub(at) / MIN_ENTRY_BYTES < count {
        return Err(PackError::Truncated("directory"));
    }
    let sparse = flags & PACK_SPARSE_BUNDLE != 0;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let length = usize::try_from(u32_at(bytes, at, "directory")?)
            .map_err(|_| PackError::Truncated("directory"))?;
        if length > MAX_PATH_BYTES {
            return Err(PackError::BadPath(format!(
                "a path of {length} bytes, more than {MAX_PATH_BYTES}"
            )));
        }
        at += 4;
        let raw = bytes
            .get(at..at + length)
            .ok_or(PackError::Truncated("directory"))?;
        let path = entry_path(raw)?;
        at += length;
        let offset = u64_at(bytes, at, "directory")?;
        let size = u64_at(bytes, at + 8, "directory")?;
        let mut digest = [0_u8; 16];
        digest.copy_from_slice(
            bytes
                .get(at + 16..at + 32)
                .ok_or(PackError::Truncated("directory"))?,
        );
        let file_flags = u32_at(bytes, at + 32, "directory")?;
        at += 36;
        let check = if file_flags & FILE_REMOVAL != 0 {
            DigestCheck::NoData
        } else if file_flags & FILE_ENCRYPTED != 0 {
            DigestCheck::Encrypted
        } else if sparse {
            DigestCheck::Outside
        } else {
            let start = file_base
                .checked_add(offset)
                .and_then(|start| usize::try_from(start).ok());
            let end = start
                .zip(usize::try_from(size).ok())
                .and_then(|(start, size)| start.checked_add(size));
            let data = start
                .zip(end)
                .and_then(|(start, end)| bytes.get(start..end))
                .ok_or_else(|| PackError::DataOutOfRange(path.clone()))?;
            if md5(data) == digest {
                DigestCheck::Verified
            } else {
                DigestCheck::Mismatch
            }
        };
        entries.push(PackEntry {
            path,
            offset,
            size,
            md5: digest,
            flags: file_flags,
            digest: check,
        });
    }
    Ok(Pack {
        format_version,
        engine_version,
        flags,
        file_base,
        directory_offset,
        entries,
    })
}

/// One file a ZIP pack holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipPackEntry {
    /// The project path.
    pub path: String,
    /// The bytes it unpacks to.
    pub size: u64,
    /// The CRC-32 the archive records for it.
    pub crc32: u32,
    /// Whether its bytes, inflated, have that CRC-32.
    pub verified: bool,
}

/// The most bytes one ZIP pack entry may inflate to.
const MAX_ZIP_ENTRY_BYTES: u64 = 256 * 1024 * 1024;

/// Read the ZIP pack `bytes`: every entry's path, size and CRC-32, each
/// checked against its inflated bytes.
///
/// # Errors
/// Returns [`PackError::Zip`] for an archive this server does not read, an
/// entry it cannot unpack, or a compression method other than stored and
/// deflated.
pub fn read_zip_pack(bytes: &[u8]) -> Result<Vec<ZipPackEntry>, PackError> {
    let entries = check_archive(bytes).map_err(|error| PackError::Zip(error.to_string()))?;
    entries
        .iter()
        .filter(|entry| !entry.name.ends_with('/'))
        .map(|entry| {
            let data = entry_bytes(bytes, entry, MAX_ZIP_ENTRY_BYTES)
                .map_err(|error| PackError::Zip(error.to_string()))?;
            Ok(ZipPackEntry {
                path: entry.name.clone(),
                size: entry.size,
                crc32: entry.crc32,
                verified: crc32(&data) == entry.crc32,
            })
        })
        .collect()
}

/// How one entry of a patch pack relates to the base packs it was exported
/// against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchChange {
    /// No base pack holds the path.
    New,
    /// A base pack holds the path with other bytes.
    Changed,
    /// The entry is a delta against a base pack's copy.
    Delta,
    /// The entry removes a file a base pack holds.
    Removed,
    /// The entry removes a path no base pack holds.
    RemovedAbsent,
    /// A base pack holds the path with the same size and MD5: the patch
    /// carries a file that did not change.
    Identical,
}

impl PatchChange {
    /// Stable snake_case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Changed => "changed",
            Self::Delta => "delta",
            Self::Removed => "removed",
            Self::RemovedAbsent => "removed_absent",
            Self::Identical => "identical",
        }
    }
}

/// What a patch pack holds against its base packs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PatchComparison {
    /// Each patch entry's path and change, in directory order.
    pub changes: Vec<(String, PatchChange)>,
}

impl PatchComparison {
    /// How many entries have `change`.
    #[must_use]
    pub fn count(&self, change: PatchChange) -> usize {
        self.changes
            .iter()
            .filter(|(_, entry)| *entry == change)
            .count()
    }

    /// The paths of the entries identical to a base pack's.
    #[must_use]
    pub fn identical(&self) -> Vec<&str> {
        self.changes
            .iter()
            .filter(|(_, change)| *change == PatchChange::Identical)
            .map(|(path, _)| path.as_str())
            .collect()
    }
}

/// Compare the entries of `patch` with what `bases`, applied in order, hold:
/// a later base's entry replaces an earlier one's, and a removal takes the
/// path away.
#[must_use]
pub fn compare_patch(patch: &Pack, bases: &[Pack]) -> PatchComparison {
    let mut held: BTreeMap<&str, (u64, [u8; 16])> = BTreeMap::new();
    for base in bases {
        for entry in &base.entries {
            if entry.removal() {
                held.remove(entry.path.as_str());
            } else {
                held.insert(entry.path.as_str(), (entry.size, entry.md5));
            }
        }
    }
    let changes = patch
        .entries
        .iter()
        .map(|entry| {
            let base = held.get(entry.path.as_str());
            let change = if entry.removal() {
                if base.is_some() {
                    PatchChange::Removed
                } else {
                    PatchChange::RemovedAbsent
                }
            } else if entry.delta() {
                PatchChange::Delta
            } else {
                match base {
                    None => PatchChange::New,
                    Some(&(size, digest)) if size == entry.size && digest == entry.md5 => {
                        PatchChange::Identical
                    }
                    Some(_) => PatchChange::Changed,
                }
            };
            (entry.path.clone(), change)
        })
        .collect();
    PatchComparison { changes }
}

/// Packs laid out the way the locked builds write them, for tests.
#[cfg(any(test, feature = "test-hooks"))]
pub mod fixture {
    use super::{FILE_REMOVAL, PACK_RELATIVE_FILE_BASE, PCK_MAGIC, md5};

    /// One file of a fixture pack.
    #[derive(Debug, Clone, Copy)]
    pub struct Entry<'a> {
        /// The project path, without `res://`.
        pub path: &'a str,
        /// The stored bytes; ignored for a removal.
        pub data: &'a [u8],
        /// The file flags.
        pub flags: u32,
    }

    fn align(value: usize, to: usize) -> usize {
        value.div_ceil(to) * to
    }

    fn directory(entries: &[Entry<'_>], offsets: &[usize]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&u32::try_from(entries.len()).expect("count").to_le_bytes());
        for (entry, offset) in entries.iter().zip(offsets) {
            let padded = align(entry.path.len(), 4);
            out.extend_from_slice(&u32::try_from(padded).expect("length").to_le_bytes());
            out.extend_from_slice(entry.path.as_bytes());
            out.resize(out.len() + padded - entry.path.len(), 0);
            let removal = entry.flags & FILE_REMOVAL != 0;
            let size = if removal { 0 } else { entry.data.len() };
            out.extend_from_slice(&(*offset as u64).to_le_bytes());
            out.extend_from_slice(&(size as u64).to_le_bytes());
            out.extend_from_slice(&md5(if removal { b"" } else { entry.data }));
            out.extend_from_slice(&entry.flags.to_le_bytes());
        }
        out
    }

    /// A PCK of `format` (2, 3 or 4) written by engine `version`: format 2
    /// keeps its directory after the header and its data after that, as Godot
    /// 4.4.1 does; formats 3 and 4 keep the data after the header and the
    /// directory last, with offsets relative to the file base, as 4.5.1 to
    /// 4.7.1 do.
    #[must_use]
    pub fn pck(format: u32, version: (u32, u32, u32), entries: &[Entry<'_>]) -> Vec<u8> {
        let mut offsets = Vec::with_capacity(entries.len());
        let mut data = Vec::new();
        for entry in entries {
            if entry.flags & FILE_REMOVAL == 0 {
                // Each file starts 16-byte aligned; the last one ends the data.
                data.resize(align(data.len(), 16), 0);
                offsets.push(data.len());
                data.extend_from_slice(entry.data);
            } else {
                offsets.push(data.len());
            }
        }
        let mut out = Vec::new();
        out.extend_from_slice(&PCK_MAGIC);
        for word in [format, version.0, version.1, version.2] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        let table = directory(entries, &offsets);
        if format == 2 {
            out.extend_from_slice(&0_u32.to_le_bytes());
            let file_base = align(0x60 + table.len(), 16);
            out.extend_from_slice(&(file_base as u64).to_le_bytes());
            out.resize(0x60, 0);
            out.extend_from_slice(&table);
            out.resize(file_base, 0);
            out.extend_from_slice(&data);
        } else {
            out.extend_from_slice(&PACK_RELATIVE_FILE_BASE.to_le_bytes());
            let file_base = 0x70;
            let directory_offset = align(file_base + data.len(), 16);
            out.extend_from_slice(&(file_base as u64).to_le_bytes());
            out.extend_from_slice(&(directory_offset as u64).to_le_bytes());
            out.resize(file_base, 0);
            out.extend_from_slice(&data);
            out.resize(directory_offset, 0);
            out.extend_from_slice(&table);
        }
        out
    }

    /// The header-only file Godot 4.5.1 to 4.7.1 leave behind when a patch
    /// has nothing to export: 112 bytes of a `format` header, no directory.
    #[must_use]
    pub fn header_only(format: u32, version: (u32, u32, u32)) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&PCK_MAGIC);
        for word in [
            format,
            version.0,
            version.1,
            version.2,
            PACK_RELATIVE_FILE_BASE,
        ] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(&0x70_u64.to_le_bytes());
        out.resize(0x70, 0);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{Entry, header_only, pck};
    use super::*;

    const LEVELS: &[u8] = b"{\"levels\": [1, 2, 3]}\n";
    const PROJECT: &[u8] = b"ECFG project settings";

    fn entry<'a>(path: &'a str, data: &'a [u8]) -> Entry<'a> {
        Entry {
            path,
            data,
            flags: 0,
        }
    }

    #[test]
    fn every_locked_format_lists_its_files_with_verified_digests() {
        for (format, version) in [
            (2, (4, 4, 1)),
            (3, (4, 5, 1)),
            (3, (4, 6, 1)),
            (4, (4, 7, 1)),
        ] {
            let bytes = pck(
                format,
                version,
                &[
                    entry("data/levels.json", LEVELS),
                    entry("project.binary", PROJECT),
                    entry(".godot/uid_cache.bin", b"uid"),
                ],
            );
            let pack = read_pack(&bytes).expect("a pack the locked builds write");
            assert_eq!(pack.format_version, format);
            assert_eq!(pack.engine_version, version);
            assert_eq!(pack.directory_offset.is_some(), format >= 3, "{format}");
            let paths: Vec<&str> = pack
                .entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect();
            assert_eq!(
                paths,
                ["data/levels.json", "project.binary", ".godot/uid_cache.bin"]
            );
            assert!(
                pack.entries
                    .iter()
                    .all(|entry| entry.digest == DigestCheck::Verified),
                "{format}"
            );
            assert_eq!(pack.entries[0].size, LEVELS.len() as u64);
            assert_eq!(pack.entries[0].md5_hex(), hex(&md5(LEVELS)));
            assert!(pack.mismatches().is_empty());
        }
    }

    #[test]
    fn a_res_prefix_is_not_part_of_the_path() {
        let bytes = pck(4, (4, 7, 1), &[entry("res://data/levels.json", LEVELS)]);
        assert_eq!(
            read_pack(&bytes).expect("pack").entries[0].path,
            "data/levels.json"
        );
    }

    #[test]
    fn a_stored_byte_that_changed_is_a_mismatch_not_a_pass() {
        let mut bytes = pck(3, (4, 5, 1), &[entry("data/levels.json", LEVELS)]);
        bytes[0x70] ^= 0xff;
        let pack = read_pack(&bytes).expect("a readable pack");
        assert_eq!(pack.entries[0].digest, DigestCheck::Mismatch);
        assert_eq!(pack.mismatches().len(), 1);
    }

    #[test]
    fn removals_and_deltas_are_told_apart() {
        let bytes = pck(
            4,
            (4, 7, 1),
            &[
                entry("data/extra.json", b"{}"),
                Entry {
                    path: "data/levels.json",
                    data: b"",
                    flags: FILE_REMOVAL,
                },
                Entry {
                    path: "data/big.json",
                    data: b"GDDL delta",
                    flags: FILE_DELTA,
                },
            ],
        );
        let pack = read_pack(&bytes).expect("a patch");
        assert!(pack.entries[1].removal() && pack.entries[1].size == 0);
        assert_eq!(pack.entries[1].digest, DigestCheck::NoData);
        assert!(pack.entries[2].delta());
        assert_eq!(pack.entries[2].digest, DigestCheck::Verified);
    }

    #[test]
    fn an_encrypted_directory_is_refused_as_unreadable() {
        let mut bytes = pck(4, (4, 7, 1), &[entry("data/levels.json", LEVELS)]);
        bytes[20] |= 1;
        assert_eq!(read_pack(&bytes), Err(PackError::EncryptedDirectory));
    }

    #[test]
    fn an_encrypted_file_is_listed_but_not_checked() {
        let bytes = pck(
            4,
            (4, 7, 1),
            &[Entry {
                path: "data/levels.json",
                data: LEVELS,
                flags: FILE_ENCRYPTED,
            }],
        );
        assert_eq!(
            read_pack(&bytes).expect("pack").entries[0].digest,
            DigestCheck::Encrypted
        );
    }

    #[test]
    fn what_is_not_a_known_pack_is_refused() {
        assert_eq!(read_pack(b""), Err(PackError::NotAPack));
        assert_eq!(read_pack(b"PK\x03\x04 a zip"), Err(PackError::NotAPack));
        let mut bytes = pck(4, (4, 7, 1), &[entry("data/levels.json", LEVELS)]);
        bytes[4] = 5;
        assert_eq!(read_pack(&bytes), Err(PackError::UnsupportedFormat(5)));
        bytes[4] = 1;
        assert_eq!(read_pack(&bytes), Err(PackError::UnsupportedFormat(1)));
        let mut flagged = pck(4, (4, 7, 1), &[entry("data/levels.json", LEVELS)]);
        flagged[20] |= 0x80;
        assert_eq!(read_pack(&flagged), Err(PackError::UnknownFlags(0x80)));
    }

    #[test]
    fn a_truncated_pack_is_refused_at_every_length() {
        for format in [2, 3, 4] {
            let bytes = pck(
                format,
                (4, 7, 1),
                &[
                    entry("data/levels.json", LEVELS),
                    entry("project.binary", PROJECT),
                ],
            );
            for length in 0..bytes.len() {
                let cut = &bytes[..length];
                // A format 2 pack keeps its data last; a cut there leaves a
                // readable directory whose data is missing.
                assert!(
                    read_pack(cut).is_err(),
                    "format {format} cut at {length} of {} bytes was read",
                    bytes.len()
                );
            }
        }
    }

    #[test]
    fn the_file_a_patch_with_nothing_to_export_leaves_has_no_directory() {
        assert_eq!(
            read_pack(&header_only(4, (4, 7, 1))),
            Err(PackError::Truncated("directory"))
        );
    }

    #[test]
    fn malformed_directories_are_refused() {
        // A count no file of this size could hold.
        let mut bytes = pck(4, (4, 7, 1), &[entry("data/levels.json", LEVELS)]);
        let directory = usize::try_from(u64::from_le_bytes(
            bytes[32..40].try_into().expect("eight bytes"),
        ))
        .expect("offset");
        bytes[directory..directory + 4].copy_from_slice(&50_000_u32.to_le_bytes());
        assert_eq!(read_pack(&bytes), Err(PackError::Truncated("directory")));
        bytes[directory..directory + 4].copy_from_slice(&(MAX_ENTRIES + 1).to_le_bytes());
        assert_eq!(
            read_pack(&bytes),
            Err(PackError::TooManyEntries(MAX_ENTRIES + 1))
        );
        // A path that leaves the project.
        for path in ["../escape.txt", "/absolute", "a//b", "a\\b"] {
            let bytes = pck(4, (4, 7, 1), &[entry(path, LEVELS)]);
            assert!(
                matches!(read_pack(&bytes), Err(PackError::BadPath(_))),
                "{path}"
            );
        }
        // Data that lies outside the file.
        let mut bytes = pck(4, (4, 7, 1), &[entry("data/levels.json", LEVELS)]);
        let size_at = directory_entry_size_offset(&bytes);
        bytes[size_at..size_at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(
            read_pack(&bytes),
            Err(PackError::DataOutOfRange("data/levels.json".to_owned()))
        );
        // A directory offset inside the header.
        let mut bytes = pck(4, (4, 7, 1), &[entry("data/levels.json", LEVELS)]);
        bytes[32..40].copy_from_slice(&8_u64.to_le_bytes());
        assert_eq!(read_pack(&bytes), Err(PackError::Truncated("directory")));
    }

    /// Where the first entry's size field is, in a one-entry format 4 pack.
    fn directory_entry_size_offset(bytes: &[u8]) -> usize {
        let directory = usize::try_from(u64::from_le_bytes(
            bytes[32..40].try_into().expect("eight bytes"),
        ))
        .expect("offset");
        let length = u32::from_le_bytes(
            bytes[directory + 4..directory + 8]
                .try_into()
                .expect("four bytes"),
        ) as usize;
        directory + 8 + length + 8
    }

    #[test]
    fn a_patch_is_compared_with_its_bases_applied_in_order() {
        let base = read_pack(&pck(
            4,
            (4, 7, 1),
            &[
                entry("data/levels.json", LEVELS),
                entry("project.binary", PROJECT),
                entry("data/old.json", b"old"),
            ],
        ))
        .expect("base");
        // A second base removes `data/old.json` and changes `project.binary`.
        let second = read_pack(&pck(
            4,
            (4, 7, 1),
            &[
                Entry {
                    path: "data/old.json",
                    data: b"",
                    flags: FILE_REMOVAL,
                },
                entry("project.binary", b"ECFG second"),
            ],
        ))
        .expect("second base");
        let patch = read_pack(&pck(
            4,
            (4, 7, 1),
            &[
                entry("data/extra.json", b"{}"),
                entry("data/levels.json", b"{\"levels\": [1]}\n"),
                entry("project.binary", b"ECFG second"),
                Entry {
                    path: "data/old.json",
                    data: b"",
                    flags: FILE_REMOVAL,
                },
                Entry {
                    path: "data/levels.json",
                    data: b"",
                    flags: FILE_REMOVAL,
                },
                Entry {
                    path: "data/big.json",
                    data: b"GDDL",
                    flags: FILE_DELTA,
                },
            ],
        ))
        .expect("patch");
        let comparison = compare_patch(&patch, &[base, second]);
        let changes: Vec<PatchChange> = comparison
            .changes
            .iter()
            .map(|(_, change)| *change)
            .collect();
        assert_eq!(
            changes,
            [
                PatchChange::New,
                PatchChange::Changed,
                PatchChange::Identical,
                PatchChange::RemovedAbsent,
                PatchChange::Removed,
                PatchChange::Delta,
            ]
        );
        assert_eq!(comparison.identical(), ["project.binary"]);
        assert_eq!(comparison.count(PatchChange::New), 1);
    }

    #[test]
    fn a_zip_pack_lists_its_files_with_checked_crcs() {
        use crate::export::bundle::fixture::{Entry as ZipEntry, stored_zip};
        let archive = stored_zip(&[
            ZipEntry {
                name: "data/levels.json",
                data: LEVELS,
                mode: 0o100_644,
            },
            ZipEntry {
                name: "project.binary",
                data: PROJECT,
                mode: 0o100_644,
            },
        ]);
        let entries = read_zip_pack(&archive).expect("a ZIP pack");
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.verified));
        assert_eq!(entries[0].path, "data/levels.json");
        assert_eq!(entries[0].size, LEVELS.len() as u64);
        assert!(matches!(read_zip_pack(b"GDPC"), Err(PackError::Zip(_))));
    }
}
