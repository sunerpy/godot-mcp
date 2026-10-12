//! Godot exports: the presets a project declares and the exact templates an
//! export of one of them needs.
//!
//! | module | responsibility |
//! | --- | --- |
//! | [`presets`] | reading `export_presets.cfg` without the engine, and naming inline secrets |
//! | [`templates`] | the one template directory and file an exact build's export needs |
//! | [`bundle`] | checking and unpacking a macOS export's app bundle to run it once |

pub mod bundle;
pub mod presets;
pub mod templates;
