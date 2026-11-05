//! Checking an exported Android APK before it becomes an export's result.
//!
//! An APK is a zip archive. One a Godot export produced holds its binary
//! `AndroidManifest.xml` and its game data under `assets/` — at least
//! `assets/project.binary`, the exported project settings (measured on
//! Godot 4.7.1). The package name and the launcher are read from the manifest
//! the APK carries, never from the preset that asked for them. Whether the
//! APK is signed is a question for `apksigner verify`, which the caller runs.

use super::axml::{AxmlError, ManifestFacts, read_manifest};
use super::bundle::{check_archive, entry_bytes};

/// The entry Android reads the app's declarations from.
pub const MANIFEST_ENTRY: &str = "AndroidManifest.xml";
/// The game data an exported Godot APK must hold.
pub const GAME_DATA_ENTRY: &str = "assets/project.binary";
/// The most bytes the manifest may unpack to.
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;

/// Why an APK is not a usable export.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApkError {
    /// It is not a zip archive this server reads.
    #[error("the APK is not a zip archive this server reads: {0}")]
    Archive(String),
    /// It has no manifest.
    #[error("the APK holds no {MANIFEST_ENTRY}")]
    NoManifest,
    /// Its manifest cannot be read.
    #[error("the APK's {MANIFEST_ENTRY} cannot be read: {0}")]
    Manifest(AxmlError),
    /// It holds no game data.
    #[error("the APK holds no game data: {GAME_DATA_ENTRY} is missing")]
    NoGameData,
}

/// What a checked APK holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApkFacts {
    /// How many entries the archive holds.
    pub entries: usize,
    /// What its manifest declares.
    pub manifest: ManifestFacts,
    /// The native ABIs it carries libraries for, from `lib/<abi>/`.
    pub abis: Vec<String>,
    /// How many entries it holds under `assets/`.
    pub asset_entries: usize,
}

/// Check the APK `bytes` and read what it declares.
///
/// # Errors
/// Returns [`ApkError`] for an archive this server does not read, a missing
/// or unreadable manifest, and missing game data.
pub fn read_apk(bytes: &[u8]) -> Result<ApkFacts, ApkError> {
    let entries = check_archive(bytes).map_err(|error| ApkError::Archive(error.to_string()))?;
    let manifest = entries
        .iter()
        .find(|entry| entry.name == MANIFEST_ENTRY)
        .ok_or(ApkError::NoManifest)?;
    let manifest = entry_bytes(bytes, manifest, MAX_MANIFEST_BYTES)
        .map_err(|error| ApkError::Archive(error.to_string()))?;
    let manifest = read_manifest(&manifest).map_err(ApkError::Manifest)?;
    if !entries.iter().any(|entry| entry.name == GAME_DATA_ENTRY) {
        return Err(ApkError::NoGameData);
    }
    let mut abis: Vec<String> = entries
        .iter()
        .filter_map(|entry| {
            let rest = entry.name.strip_prefix("lib/")?;
            let (abi, file) = rest.split_once('/')?;
            (!abi.is_empty() && file.ends_with(".so")).then(|| abi.to_owned())
        })
        .collect();
    abis.sort();
    abis.dedup();
    Ok(ApkFacts {
        entries: entries.len(),
        manifest,
        abis,
        asset_entries: entries
            .iter()
            .filter(|entry| entry.name.starts_with("assets/") && !entry.name.ends_with('/'))
            .count(),
    })
}

/// APKs laid out the way a Godot export writes them, for tests.
#[cfg(any(test, feature = "test-hooks"))]
pub mod fixture {
    use super::super::axml::fixture::{Spec, manifest};
    use super::super::bundle::fixture::{Entry, stored_zip};
    use super::{GAME_DATA_ENTRY, MANIFEST_ENTRY};

    /// An APK whose manifest is `spec`'s, holding the game data and one
    /// native library.
    #[must_use]
    pub fn apk(spec: &Spec) -> Vec<u8> {
        let manifest = manifest(spec);
        stored_zip(&[
            Entry {
                name: MANIFEST_ENTRY,
                data: &manifest,
                mode: 0o100_644,
            },
            Entry {
                name: "classes.dex",
                data: b"dex\n035\0",
                mode: 0o100_644,
            },
            Entry {
                name: "lib/arm64-v8a/libgodot_android.so",
                data: b"\x7fELF",
                mode: 0o100_644,
            },
            Entry {
                name: GAME_DATA_ENTRY,
                data: b"ECFG",
                mode: 0o100_644,
            },
            Entry {
                name: "assets/data/levels.json",
                data: b"{}",
                mode: 0o100_644,
            },
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::super::axml::fixture::Spec;
    use super::super::bundle::fixture::{Entry, stored_zip};
    use super::fixture::apk;
    use super::*;

    #[test]
    fn an_exported_apk_names_its_package_launcher_and_libraries() {
        let facts = read_apk(&apk(&Spec::godot("org.godotmcp.measure"))).expect("an APK");
        assert_eq!(facts.manifest.package, "org.godotmcp.measure");
        assert_eq!(
            facts.manifest.launcher.expect("a launcher").name,
            "com.godot.game.GodotAppLauncher"
        );
        assert_eq!(facts.abis, ["arm64-v8a"]);
        assert_eq!(facts.asset_entries, 2);
        assert_eq!(facts.entries, 5);
    }

    #[test]
    fn an_apk_without_its_manifest_or_game_data_is_refused() {
        assert!(matches!(read_apk(b"not a zip"), Err(ApkError::Archive(_))));
        let without_manifest = stored_zip(&[Entry {
            name: GAME_DATA_ENTRY,
            data: b"ECFG",
            mode: 0o100_644,
        }]);
        assert_eq!(read_apk(&without_manifest), Err(ApkError::NoManifest));
        let manifest = super::super::axml::fixture::manifest(&Spec::godot("org.example"));
        let without_data = stored_zip(&[Entry {
            name: MANIFEST_ENTRY,
            data: &manifest,
            mode: 0o100_644,
        }]);
        assert_eq!(read_apk(&without_data), Err(ApkError::NoGameData));
        let text_manifest = stored_zip(&[
            Entry {
                name: MANIFEST_ENTRY,
                data: b"<manifest package=\"org.example\"/>",
                mode: 0o100_644,
            },
            Entry {
                name: GAME_DATA_ENTRY,
                data: b"ECFG",
                mode: 0o100_644,
            },
        ]);
        assert_eq!(
            read_apk(&text_manifest),
            Err(ApkError::Manifest(AxmlError::NotBinaryXml))
        );
    }
}
