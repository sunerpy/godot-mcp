//! Running a macOS export: the app bundle inside its zip.
//!
//! Godot exports a macOS preset as an app bundle, `<name>.app`, and packs it
//! into a zip when the output path ends in `.zip`, the only form this server
//! exports. Running such a build once means unpacking the bundle and starting
//! the executable its `Contents/Info.plist` names.
//!
//! The archive was written by a process that ran project code, so it is
//! checked before anything is unpacked: every entry stays inside the
//! destination, none is a symbolic link, each local header names the same file
//! as the central directory, and the whole archive unpacks to a bounded size.
//! The unpacking itself is macOS's own `ditto`, into a directory that did not
//! exist before.

use std::path::{Path, PathBuf};

/// The most bytes a checked archive may unpack to.
pub const MAX_UNPACKED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Why an exported app bundle cannot be unpacked and run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BundleError {
    /// The archive is not a zip this server reads.
    #[error("the export is not a zip archive this server unpacks: {0}")]
    Malformed(String),
    /// An entry would be written outside the destination.
    #[error("the export's entry `{0}` would be unpacked outside its directory")]
    Escapes(String),
    /// An entry is a symbolic link.
    #[error("the export holds the symbolic link `{0}`, which this server does not unpack")]
    SymbolicLink(String),
    /// The archive unpacks to more than [`MAX_UNPACKED_BYTES`].
    #[error("the export would unpack to more than {limit} bytes")]
    TooLarge {
        /// The bound.
        limit: u64,
    },
    /// The unpacked archive holds no single app bundle with an executable.
    #[error("the export holds no runnable app bundle: {0}")]
    NoBundle(String),
    /// Unpacking failed.
    #[error("the export could not be unpacked: {0}")]
    Unpack(String),
}

/// One entry of a checked archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    /// The entry's path inside the archive, `/`-separated.
    pub name: String,
    /// The bytes it unpacks to.
    pub size: u64,
}

const END_OF_CENTRAL_DIRECTORY: u32 = 0x0605_4b50;
const CENTRAL_DIRECTORY_HEADER: u32 = 0x0201_4b50;
const LOCAL_HEADER: u32 = 0x0403_4b50;
/// The fixed part of an end-of-central-directory record.
const END_RECORD_BYTES: usize = 22;
/// The fixed part of a central directory header.
const CENTRAL_HEADER_BYTES: usize = 46;
/// The fixed part of a local header.
const LOCAL_HEADER_BYTES: usize = 30;
/// How long `ditto` may take to unpack one checked archive.
const UNPACK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);

fn malformed(reason: impl Into<String>) -> BundleError {
    BundleError::Malformed(reason.into())
}

fn u16_at(bytes: &[u8], at: usize) -> Result<usize, BundleError> {
    bytes
        .get(at..at + 2)
        .map(|field| usize::from(u16::from_le_bytes([field[0], field[1]])))
        .ok_or_else(|| malformed("a record ends past the end of the archive"))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, BundleError> {
    bytes
        .get(at..at + 4)
        .map(|field| u32::from_le_bytes([field[0], field[1], field[2], field[3]]))
        .ok_or_else(|| malformed("a record ends past the end of the archive"))
}

fn offset_at(bytes: &[u8], at: usize) -> Result<usize, BundleError> {
    usize::try_from(u32_at(bytes, at)?).map_err(|_| malformed("an offset does not fit"))
}

/// The end-of-central-directory record: the last one whose comment ends
/// exactly at the end of the archive.
fn end_record(archive: &[u8]) -> Result<usize, BundleError> {
    let last = archive
        .len()
        .checked_sub(END_RECORD_BYTES)
        .ok_or_else(|| malformed("it is too short to be a zip archive"))?;
    let first = last.saturating_sub(usize::from(u16::MAX));
    (first..=last)
        .rev()
        .find(|&at| {
            u32_at(archive, at).is_ok_and(|signature| signature == END_OF_CENTRAL_DIRECTORY)
                && u16_at(archive, at + 20)
                    .is_ok_and(|comment| at + END_RECORD_BYTES + comment == archive.len())
        })
        .ok_or_else(|| malformed("it has no end-of-central-directory record"))
}

/// Refuse a name that would leave the destination when unpacked anywhere: an
/// absolute or drive path, a `..` or `.` component, an empty component, a
/// backslash or a NUL.
fn check_name(name: &str) -> Result<(), BundleError> {
    let escapes = name.is_empty()
        || name.starts_with('/')
        || name.contains(['\\', ':', '\0'])
        || name
            .strip_suffix('/')
            .unwrap_or(name)
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..");
    if escapes {
        Err(BundleError::Escapes(name.to_owned()))
    } else {
        Ok(())
    }
}

/// Check that `archive` is a zip whose every entry may be unpacked, and list
/// its entries.
///
/// # Errors
/// Returns [`BundleError`] naming the first entry that may not be unpacked, or
/// what makes the archive unreadable.
pub fn check_archive(archive: &[u8]) -> Result<Vec<ArchiveEntry>, BundleError> {
    let end = end_record(archive)?;
    let (disk, central_disk) = (u16_at(archive, end + 4)?, u16_at(archive, end + 6)?);
    let (here, total) = (u16_at(archive, end + 8)?, u16_at(archive, end + 10)?);
    let central_size = offset_at(archive, end + 12)?;
    let central_offset = offset_at(archive, end + 16)?;
    if disk != 0 || central_disk != 0 || here != total {
        return Err(malformed("it spans more than one disk"));
    }
    if total == usize::from(u16::MAX)
        || u32::try_from(central_size).is_ok_and(|size| size == u32::MAX)
        || u32::try_from(central_offset).is_ok_and(|offset| offset == u32::MAX)
    {
        return Err(malformed("it is a zip64 archive"));
    }
    if central_offset
        .checked_add(central_size)
        .is_none_or(|limit| limit > end)
    {
        return Err(malformed("its central directory overlaps its end record"));
    }
    let mut entries = Vec::with_capacity(total);
    let mut unpacked: u64 = 0;
    let mut at = central_offset;
    for _ in 0..total {
        if u32_at(archive, at)? != CENTRAL_DIRECTORY_HEADER {
            return Err(malformed("its central directory is damaged"));
        }
        let made_by = u16_at(archive, at + 4)?;
        let flags = u16_at(archive, at + 8)?;
        let size = u64::from(u32_at(archive, at + 24)?);
        let name_length = u16_at(archive, at + 28)?;
        let extra_length = u16_at(archive, at + 30)?;
        let comment_length = u16_at(archive, at + 32)?;
        let external = u32_at(archive, at + 38)?;
        let local = offset_at(archive, at + 42)?;
        let name_start = at + CENTRAL_HEADER_BYTES;
        let name_bytes = archive
            .get(name_start..name_start + name_length)
            .ok_or_else(|| malformed("a name ends past the end of the archive"))?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| malformed("an entry's name is not UTF-8"))?;
        if flags & 1 != 0 {
            return Err(malformed(format!("its entry `{name}` is encrypted")));
        }
        check_name(name)?;
        // A Unix "version made by" keeps the mode, file type included, in the
        // upper half of the external attributes.
        if made_by >> 8 == 3 && (external >> 16) & 0o170_000 == 0o120_000 {
            return Err(BundleError::SymbolicLink(name.to_owned()));
        }
        if local >= central_offset
            || u32_at(archive, local)? != LOCAL_HEADER
            || archive
                .get(local + LOCAL_HEADER_BYTES..local + LOCAL_HEADER_BYTES + name_length)
                .is_none_or(|local_name| {
                    u16_at(archive, local + 26).ok() != Some(name_length)
                        || local_name != name_bytes
                })
        {
            return Err(malformed(format!(
                "the local header of `{name}` does not name the same file"
            )));
        }
        unpacked = unpacked.saturating_add(size);
        if unpacked > MAX_UNPACKED_BYTES {
            return Err(BundleError::TooLarge {
                limit: MAX_UNPACKED_BYTES,
            });
        }
        entries.push(ArchiveEntry {
            name: name.to_owned(),
            size,
        });
        at = name_start + name_length + extra_length + comment_length;
    }
    Ok(entries)
}

/// The `<string>` an XML property list gives `key`.
fn plist_string(plist: &str, key: &str) -> Option<String> {
    let marker = format!("<key>{key}</key>");
    let after = &plist[plist.find(&marker)? + marker.len()..];
    let value = after.trim_start().strip_prefix("<string>")?;
    let value = &value[..value.find("</string>")?];
    Some(
        value
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

/// The executable of the one app bundle at the top of `unpacked`: the file in
/// its `Contents/MacOS` that its `Contents/Info.plist` names.
///
/// # Errors
/// Returns [`BundleError::NoBundle`] when there is not exactly one bundle, or
/// it names no executable that is a regular file inside it.
pub fn bundle_executable(unpacked: &Path) -> Result<PathBuf, BundleError> {
    let none = |reason: &str| BundleError::NoBundle(reason.to_owned());
    let listing = std::fs::read_dir(unpacked)
        .map_err(|error| BundleError::Unpack(format!("the unpacked export: {error}")))?;
    let bundles: Vec<PathBuf> = listing
        .flatten()
        .filter(|entry| {
            entry.file_name().to_string_lossy().ends_with(".app")
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
        })
        .map(|entry| entry.path())
        .collect();
    let [bundle] = bundles.as_slice() else {
        return Err(BundleError::NoBundle(format!(
            "it holds {} `.app` directories at its top, not one",
            bundles.len()
        )));
    };
    let contents = bundle.join("Contents");
    let plist_path = contents.join("Info.plist");
    let plist = std::fs::symlink_metadata(&plist_path)
        .ok()
        .filter(|metadata| metadata.is_file() && metadata.len() <= 1024 * 1024)
        .and_then(|_| std::fs::read_to_string(&plist_path).ok())
        .ok_or_else(|| none("its bundle has no readable XML Contents/Info.plist"))?;
    let name = plist_string(&plist, "CFBundleExecutable")
        .ok_or_else(|| none("its Info.plist names no CFBundleExecutable"))?;
    if name.is_empty() || name.contains(['/', '\0']) || name == "." || name == ".." {
        return Err(none(
            "its Info.plist names an executable outside Contents/MacOS",
        ));
    }
    let executable = contents.join("MacOS").join(&name);
    if std::fs::symlink_metadata(&executable).is_ok_and(|metadata| metadata.is_file()) {
        Ok(executable)
    } else {
        Err(BundleError::NoBundle(format!(
            "its executable Contents/MacOS/{name} is not a file in the bundle"
        )))
    }
}

/// Check `archive`, unpack it into `destination`, which must not exist yet,
/// and answer the bundle's executable, which the owner may run.
///
/// # Errors
/// Returns [`BundleError`] when the archive may not be unpacked, unpacking
/// fails, or what it unpacks to holds no runnable bundle.
pub fn unpack_app(archive: &Path, destination: &Path) -> Result<PathBuf, BundleError> {
    let bytes = std::fs::read(archive)
        .map_err(|error| BundleError::Unpack(format!("the export could not be read: {error}")))?;
    check_archive(&bytes)?;
    drop(bytes);
    std::fs::create_dir(destination).map_err(|error| {
        BundleError::Unpack(format!("its private unpacking directory: {error}"))
    })?;
    ditto(archive, destination)?;
    let executable = bundle_executable(destination)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let metadata = std::fs::metadata(&executable)
            .map_err(|error| BundleError::Unpack(error.to_string()))?;
        let mut permissions = metadata.permissions();
        permissions.set_mode(permissions.mode() | 0o700);
        std::fs::set_permissions(&executable, permissions)
            .map_err(|error| BundleError::Unpack(error.to_string()))?;
    }
    Ok(executable)
}

/// Unpack a checked archive with macOS's own `ditto`, keeping none of the
/// metadata an archive may carry besides each file's mode.
#[cfg(target_os = "macos")]
fn ditto(archive: &Path, destination: &Path) -> Result<(), BundleError> {
    use std::process::{Command, Stdio};

    let mut child = Command::new("/usr/bin/ditto")
        .args(["-x", "-k", "--norsrc", "--noextattr", "--noqtn", "--noacl"])
        .arg(archive)
        .arg(destination)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| BundleError::Unpack(format!("ditto did not start: {error}")))?;
    let deadline = std::time::Instant::now() + UNPACK_DEADLINE;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(BundleError::Unpack(format!(
                    "ditto did not finish within {} s",
                    UNPACK_DEADLINE.as_secs()
                )));
            }
            Err(error) => return Err(BundleError::Unpack(error.to_string())),
        }
    };
    if status.success() {
        return Ok(());
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        use std::io::Read as _;
        let _ = pipe.by_ref().take(4096).read_to_string(&mut stderr);
    }
    Err(BundleError::Unpack(format!(
        "ditto exited with {status}: {}",
        stderr.trim()
    )))
}

/// Only macOS unpacks an app bundle: only a Mac runs one.
#[cfg(not(target_os = "macos"))]
fn ditto(archive: &Path, destination: &Path) -> Result<(), BundleError> {
    let _ = (archive, destination, UNPACK_DEADLINE);
    Err(BundleError::Unpack(
        "an app bundle is unpacked only on macOS".to_owned(),
    ))
}

/// Zip archives written the way Godot's exporter writes them, for tests.
#[cfg(any(test, feature = "test-hooks"))]
pub mod fixture {
    /// One entry of a fixture archive.
    #[derive(Debug, Clone, Copy)]
    pub struct Entry<'a> {
        /// The path inside the archive.
        pub name: &'a str,
        /// The entry's bytes; a symbolic link's are its target.
        pub data: &'a [u8],
        /// The Unix mode with the file type: `0o100755` for an executable,
        /// `0o100644` for a file, `0o120644` for a symbolic link.
        pub mode: u32,
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffff_u32;
        for byte in data {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }

    fn u16le(out: &mut Vec<u8>, value: usize) {
        out.extend_from_slice(&u16::try_from(value).expect("fits 16 bits").to_le_bytes());
    }

    fn u32le(out: &mut Vec<u8>, value: usize) {
        out.extend_from_slice(&u32::try_from(value).expect("fits 32 bits").to_le_bytes());
    }

    /// A zip archive of `entries`, stored uncompressed, each with its Unix
    /// mode under a Unix "version made by", as Godot's exporter records it.
    #[must_use]
    pub fn stored_zip(entries: &[Entry<'_>]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for entry in entries {
            let offset = out.len();
            let crc = crc32(entry.data) as usize;
            // The local header: version 2.0, UTF-8 names, stored, 1980-01-01.
            u32le(&mut out, 0x0403_4b50);
            u16le(&mut out, 20);
            u16le(&mut out, 1 << 11);
            u16le(&mut out, 0);
            u16le(&mut out, 0);
            u16le(&mut out, 0x21);
            u32le(&mut out, crc);
            u32le(&mut out, entry.data.len());
            u32le(&mut out, entry.data.len());
            u16le(&mut out, entry.name.len());
            u16le(&mut out, 0);
            out.extend_from_slice(entry.name.as_bytes());
            out.extend_from_slice(entry.data);

            u32le(&mut central, 0x0201_4b50);
            u16le(&mut central, 0x0314);
            u16le(&mut central, 20);
            u16le(&mut central, 1 << 11);
            u16le(&mut central, 0);
            u16le(&mut central, 0);
            u16le(&mut central, 0x21);
            u32le(&mut central, crc);
            u32le(&mut central, entry.data.len());
            u32le(&mut central, entry.data.len());
            u16le(&mut central, entry.name.len());
            u16le(&mut central, 0);
            u16le(&mut central, 0);
            u16le(&mut central, 0);
            u16le(&mut central, 0);
            u32le(&mut central, (entry.mode as usize) << 16);
            u32le(&mut central, offset);
            central.extend_from_slice(entry.name.as_bytes());
        }
        let central_offset = out.len();
        let central_size = central.len();
        out.extend_from_slice(&central);
        u32le(&mut out, 0x0605_4b50);
        u16le(&mut out, 0);
        u16le(&mut out, 0);
        u16le(&mut out, entries.len());
        u16le(&mut out, entries.len());
        u32le(&mut out, central_size);
        u32le(&mut out, central_offset);
        u16le(&mut out, 0);
        out
    }

    /// The `Info.plist` of a bundle whose executable is `executable`.
    #[must_use]
    pub fn info_plist(executable: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \
             \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n\t<key>CFBundleExecutable</key>\n\t\
             <string>{executable}</string>\n\t<key>CFBundleIdentifier</key>\n\t\
             <string>org.godotmcp.fixture</string>\n</dict>\n</plist>\n"
        )
    }

    /// What a macOS export of an app called `name` packs: its `Info.plist`,
    /// its executable `program` and its pack, plus `extra` entries.
    #[must_use]
    pub fn app_bundle(name: &str, program: &[u8], extra: &[Entry<'_>]) -> Vec<u8> {
        let plist = info_plist(name);
        let plist_name = format!("{name}.app/Contents/Info.plist");
        let program_name = format!("{name}.app/Contents/MacOS/{name}");
        let pack_name = format!("{name}.app/Contents/Resources/{name}.pck");
        let mut entries = vec![
            Entry {
                name: &plist_name,
                data: plist.as_bytes(),
                mode: 0o100_644,
            },
            Entry {
                name: &program_name,
                data: program,
                mode: 0o100_755,
            },
            Entry {
                name: &pack_name,
                data: b"GDPC",
                mode: 0o100_644,
            },
        ];
        entries.extend_from_slice(extra);
        stored_zip(&entries)
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{Entry, app_bundle, info_plist, stored_zip};
    use super::*;

    const PROGRAM: &[u8] = b"#!/bin/sh\nexit 0\n";

    fn entry<'a>(name: &'a str, data: &'a [u8]) -> Entry<'a> {
        Entry {
            name,
            data,
            mode: 0o100_644,
        }
    }

    /// The offset of the first central directory header.
    fn central_offset(archive: &[u8]) -> usize {
        let end = archive.len() - 22;
        u32::from_le_bytes(archive[end + 16..end + 20].try_into().expect("four bytes")) as usize
    }

    #[test]
    fn a_bundle_archive_godot_would_write_lists_every_entry() {
        let archive = app_bundle("Game", PROGRAM, &[]);
        let entries = check_archive(&archive).expect("a well-formed bundle");
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Game.app/Contents/Info.plist",
                "Game.app/Contents/MacOS/Game",
                "Game.app/Contents/Resources/Game.pck",
            ]
        );
        assert_eq!(entries[1].size, PROGRAM.len() as u64);
    }

    #[test]
    fn an_entry_that_would_leave_its_directory_is_refused() {
        for name in [
            "../outside",
            "/absolute",
            "Game.app/../../outside",
            "Game.app\\Contents",
            "C:/outside",
            "Game.app//Contents",
            "./Game.app",
            "",
        ] {
            let archive = stored_zip(&[
                entry("Game.app/Contents/Info.plist", b"x"),
                entry(name, b"y"),
            ]);
            assert_eq!(
                check_archive(&archive),
                Err(BundleError::Escapes(name.to_owned())),
                "{name:?}"
            );
        }
    }

    #[test]
    fn a_symbolic_link_is_never_unpacked() {
        let link = Entry {
            name: "Game.app/Contents/Frameworks/Current",
            data: b"/etc",
            mode: 0o120_644,
        };
        let archive = app_bundle("Game", PROGRAM, &[link]);
        assert_eq!(
            check_archive(&archive),
            Err(BundleError::SymbolicLink(
                "Game.app/Contents/Frameworks/Current".to_owned()
            ))
        );
    }

    #[test]
    fn a_local_header_that_names_another_file_is_refused() {
        let mut archive = stored_zip(&[entry("Game.app/a", b"x")]);
        // The first local header's name starts at byte 30.
        archive[30 + "Game.app/".len()] = b'b';
        assert!(
            matches!(check_archive(&archive), Err(BundleError::Malformed(_))),
            "{:?}",
            check_archive(&archive)
        );
    }

    #[test]
    fn an_archive_that_unpacks_past_the_bound_is_refused() {
        let mut archive = stored_zip(&[entry("Game.app/a", b"x"), entry("Game.app/b", b"y")]);
        // Both central headers claim nearly 4 GiB uncompressed.
        let mut at = central_offset(&archive);
        for _ in 0..2 {
            archive[at + 24..at + 28].copy_from_slice(&0xffff_fff0_u32.to_le_bytes());
            let name_length = u16::from_le_bytes([archive[at + 28], archive[at + 29]]) as usize;
            at += 46 + name_length;
        }
        assert_eq!(
            check_archive(&archive),
            Err(BundleError::TooLarge {
                limit: MAX_UNPACKED_BYTES
            })
        );
    }

    #[test]
    fn what_is_not_a_zip_is_refused() {
        for bytes in [&b""[..], b"#!/bin/sh\nexit 0\n", &[0_u8; 64]] {
            assert!(
                matches!(check_archive(bytes), Err(BundleError::Malformed(_))),
                "{bytes:?}"
            );
        }
        let mut encrypted = stored_zip(&[entry("Game.app/a", b"x")]);
        let at = central_offset(&encrypted);
        encrypted[at + 8] |= 1;
        assert!(matches!(
            check_archive(&encrypted),
            Err(BundleError::Malformed(_))
        ));
    }

    fn unpacked_bundle(name: &str, plist: &str) -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("unpacked");
        let contents = directory.path().join(format!("{name}.app/Contents"));
        std::fs::create_dir_all(contents.join("MacOS")).expect("bundle");
        std::fs::write(contents.join("Info.plist"), plist).expect("plist");
        std::fs::write(contents.join("MacOS").join(name), PROGRAM).expect("program");
        directory
    }

    #[test]
    fn the_executable_is_the_one_the_info_plist_names() {
        let unpacked = unpacked_bundle("Game", &info_plist("Game"));
        assert_eq!(
            bundle_executable(unpacked.path()).expect("the executable"),
            unpacked.path().join("Game.app/Contents/MacOS/Game")
        );

        for plist in [
            info_plist("Other"),
            info_plist("../../Game.app/Contents/MacOS/Game"),
            info_plist(""),
            "<plist version=\"1.0\"><dict></dict></plist>".to_owned(),
        ] {
            let unpacked = unpacked_bundle("Game", &plist);
            assert!(
                matches!(
                    bundle_executable(unpacked.path()),
                    Err(BundleError::NoBundle(_))
                ),
                "{plist}"
            );
        }

        let empty = tempfile::tempdir().expect("empty");
        assert!(matches!(
            bundle_executable(empty.path()),
            Err(BundleError::NoBundle(_))
        ));
        let two = unpacked_bundle("Game", &info_plist("Game"));
        let second = two.path().join("Second.app/Contents/MacOS");
        std::fs::create_dir_all(&second).expect("a second bundle");
        assert!(matches!(
            bundle_executable(two.path()),
            Err(BundleError::NoBundle(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn an_executable_that_is_a_link_is_not_run() {
        let unpacked = unpacked_bundle("Game", &info_plist("Game"));
        let program = unpacked.path().join("Game.app/Contents/MacOS/Game");
        std::fs::remove_file(&program).expect("remove");
        std::os::unix::fs::symlink("/bin/sh", &program).expect("link");
        assert!(matches!(
            bundle_executable(unpacked.path()),
            Err(BundleError::NoBundle(_))
        ));
    }

    #[test]
    fn a_refused_archive_is_never_unpacked() {
        let scratch = tempfile::tempdir().expect("scratch");
        let archive = scratch.path().join("game.zip");
        std::fs::write(&archive, stored_zip(&[entry("../outside", b"x")])).expect("archive");
        let destination = scratch.path().join("bundle");
        assert_eq!(
            unpack_app(&archive, &destination),
            Err(BundleError::Escapes("../outside".to_owned()))
        );
        assert!(!destination.exists(), "nothing was unpacked");
        assert!(!scratch.path().join("outside").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_checked_bundle_unpacks_with_its_executable_runnable() {
        let scratch = tempfile::tempdir().expect("scratch");
        let archive = scratch.path().join("game.zip");
        std::fs::write(&archive, app_bundle("Game", PROGRAM, &[])).expect("archive");
        let destination = scratch.path().join("bundle");
        let executable = unpack_app(&archive, &destination).expect("unpacked");
        assert_eq!(executable, destination.join("Game.app/Contents/MacOS/Game"));
        let status = std::process::Command::new(&executable)
            .status()
            .expect("the executable runs");
        assert!(status.success(), "{status}");
        // A second unpack into the same place is refused rather than merged.
        assert!(matches!(
            unpack_app(&archive, &destination),
            Err(BundleError::Unpack(_))
        ));
    }
}
