//! Godot exports: the presets a project declares, the exact templates an
//! export of one of them needs, and what the export produced.
//!
//! | module | responsibility |
//! | --- | --- |
//! | [`presets`] | reading `export_presets.cfg` without the engine, and naming inline secrets |
//! | [`templates`] | the one template directory and file an exact build's export needs |
//! | [`bundle`] | checking zip archives, and unpacking a macOS export's app bundle to run it once |
//! | [`pack`] | reading the PCK and ZIP packs `--export-pack` and `--export-patch` write |
//! | [`apk`] | checking an Android export's APK and reading what it declares |
//! | [`axml`] | Android's binary XML, as far as an APK's manifest needs |
//! | [`inflate`] | a bounded DEFLATE decoder for compressed archive entries |
//! | [`md5`] | the digest a pack's file table records |

pub mod apk;
pub mod axml;
pub mod bundle;
pub mod inflate;
pub mod md5;
pub mod pack;
pub mod presets;
pub mod templates;
