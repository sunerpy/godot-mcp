//! The exact export templates one export needs.
//!
//! Godot looks for a build's templates in
//! `<data directory>/godot/export_templates/<version>.<status>/`, and a
//! template directory says which build it belongs to in its own
//! `version.txt`. An export is only as exact as its template, so this module
//! resolves the one directory whose `version.txt` names the exact build, and
//! the one template file the preset's platform, architecture and mode need —
//! or says precisely what is missing. It never searches more widely and never
//! accepts a template of a neighbouring version.

use std::path::{Path, PathBuf};

use crate::export::presets::ExportPreset;
use crate::releases::ExactVersion;

/// The `version.<status>` label of a build, the way Godot names its template
/// directory: `4.7.1.stable`, or `4.4.stable` for a `.0` patch.
#[must_use]
pub fn template_label(version: ExactVersion, status: &str) -> String {
    if version.patch == 0 {
        format!("{}.{}.{status}", version.major, version.minor)
    } else {
        format!("{version}.{status}")
    }
}

/// Where Godot keeps export templates on this host, from the data-directory
/// variables Godot itself reads; `None` when none is set.
#[must_use]
pub fn default_templates_root(
    xdg_data_home: Option<&Path>,
    home: Option<&Path>,
    appdata: Option<&Path>,
) -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        return appdata.map(|appdata| appdata.join("Godot").join("export_templates"));
    }
    if cfg!(target_os = "macos") {
        return home.map(|home| {
            home.join("Library")
                .join("Application Support")
                .join("Godot")
                .join("export_templates")
        });
    }
    xdg_data_home
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .or_else(|| home.map(|home| home.join(".local").join("share")))
        .map(|data| data.join("godot").join("export_templates"))
}

/// What one export produces for its platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportTarget {
    /// The template file name this export needs.
    pub template: String,
    /// The output's file extension, without the dot.
    pub extension: &'static str,
    /// Whether the output is an app bundle packed into a zip, which Godot
    /// writes only when the output path ends in `.zip`.
    pub app_bundle: bool,
    /// Whether this host can run the output for a smoke check.
    pub runnable_here: bool,
}

/// How Godot names the macOS platform in `export_presets.cfg`.
const MACOS: &str = "macOS";

/// Whether a preset for `platform` exports an app bundle.
#[must_use]
pub fn exports_app_bundle(platform: &str) -> bool {
    platform == MACOS
}

/// Why a preset's template cannot be resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    /// No template directory for this exact build.
    #[error(
        "no export templates for Godot {label} are installed: the build needs \
         `{label}/` under its export template directory"
    )]
    Missing {
        /// The build's template label.
        label: String,
    },
    /// The directory exists but its `version.txt` names another build.
    #[error(
        "the export template directory for {label} declares `{declared}` in version.txt, so \
         it is not this build's templates"
    )]
    WrongVersion {
        /// The build's template label.
        label: String,
        /// What `version.txt` says.
        declared: String,
    },
    /// The directory lacks the one template the preset needs.
    #[error("the export templates for {label} have no `{template}`, which this preset needs")]
    MissingFile {
        /// The build's template label.
        label: String,
        /// The template file name.
        template: String,
    },
    /// The preset's platform or architecture is not one this server exports.
    #[error("{0}")]
    Unsupported(String),
    /// The preset names a custom template.
    #[error(
        "the preset names a custom {mode} template; this server exports only through the \
         build's own installed templates"
    )]
    CustomTemplate {
        /// `debug` or `release`.
        mode: &'static str,
    },
}

/// The template a preset needs, from its platform, architecture and mode.
///
/// A macOS preset needs the build's one `macos.zip`, for both modes, and its
/// architecture defaults to `universal`, as in Godot; it exports an app
/// bundle packed as a zip.
///
/// # Errors
/// Returns [`TemplateError::Unsupported`] for a platform or architecture this
/// server does not export, and [`TemplateError::CustomTemplate`] for a preset
/// that names its own template.
pub fn target_for(preset: &ExportPreset, debug: bool) -> Result<ExportTarget, TemplateError> {
    let mode = if debug { "debug" } else { "release" };
    if preset.custom_template(debug).is_some() {
        return Err(TemplateError::CustomTemplate { mode });
    }
    let platform = preset.platform();
    let app_bundle = exports_app_bundle(platform);
    let architecture = preset
        .option_str("binary_format/architecture")
        .unwrap_or(if app_bundle { "universal" } else { "x86_64" });
    let architectures: &[&str] = match platform {
        "Linux" | "Linux/X11" => &["x86_64", "x86_32", "arm64", "arm32"],
        "Windows Desktop" => &["x86_64", "x86_32", "arm64"],
        MACOS => &["universal", "x86_64", "arm64"],
        other => {
            return Err(TemplateError::Unsupported(format!(
                "exporting for `{other}` is not supported by this server; it exports Linux, \
                 Windows Desktop and macOS presets"
            )));
        }
    };
    if !architectures.contains(&architecture) {
        return Err(TemplateError::Unsupported(format!(
            "the preset's architecture `{architecture}` is not one this server exports for \
             `{platform}`"
        )));
    }
    let host_linux_x86_64 = cfg!(all(target_os = "linux", target_arch = "x86_64"));
    let host_windows_x86_64 = cfg!(all(target_os = "windows", target_arch = "x86_64"));
    let host_mac = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x86_64"
    };
    Ok(match platform {
        MACOS => ExportTarget {
            template: "macos.zip".to_owned(),
            extension: "zip",
            app_bundle: true,
            runnable_here: cfg!(target_os = "macos")
                && (architecture == "universal" || architecture == host_mac),
        },
        "Windows Desktop" => ExportTarget {
            template: format!("windows_{mode}_{architecture}.exe"),
            extension: "exe",
            app_bundle: false,
            runnable_here: host_windows_x86_64 && architecture == "x86_64",
        },
        _ => ExportTarget {
            template: format!("linux_{mode}.{architecture}"),
            extension: if architecture == "x86_64" {
                "x86_64"
            } else {
                "bin"
            },
            app_bundle: false,
            runnable_here: host_linux_x86_64 && architecture == "x86_64",
        },
    })
}

/// The exact template directory of one build, verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateSet {
    /// The build's template label.
    pub label: String,
    /// The verified directory.
    pub directory: PathBuf,
}

impl TemplateSet {
    /// Resolve and verify the template directory of the build labelled
    /// `label` under `root`.
    ///
    /// # Errors
    /// Returns [`TemplateError::Missing`] when the directory is absent and
    /// [`TemplateError::WrongVersion`] when its `version.txt` names another
    /// build.
    pub fn resolve(root: &Path, label: &str) -> Result<Self, TemplateError> {
        let directory = root.join(label);
        let declared = std::fs::read_to_string(directory.join("version.txt")).map_err(|_| {
            TemplateError::Missing {
                label: label.to_owned(),
            }
        })?;
        let declared = declared.trim();
        if declared != label {
            return Err(TemplateError::WrongVersion {
                label: label.to_owned(),
                declared: declared.chars().take(64).collect(),
            });
        }
        Ok(Self {
            label: label.to_owned(),
            directory,
        })
    }

    /// The template file `target` needs, verified to be a regular file.
    ///
    /// # Errors
    /// Returns [`TemplateError::MissingFile`] when it is absent.
    pub fn template(&self, target: &ExportTarget) -> Result<PathBuf, TemplateError> {
        let path = self.directory.join(&target.template);
        if std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
            Ok(path)
        } else {
            Err(TemplateError::MissingFile {
                label: self.label.clone(),
                template: target.template.clone(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::presets::parse_presets;

    fn preset(platform: &str, options: &str) -> ExportPreset {
        parse_presets(&format!(
            "[preset.0]\nname=\"P\"\nplatform=\"{platform}\"\n\n[preset.0.options]\n{options}\n"
        ))
        .expect("parse")
        .remove(0)
    }

    #[test]
    fn labels_follow_godots_template_directory_names() {
        assert_eq!(
            template_label(ExactVersion::new(4, 7, 1), "stable"),
            "4.7.1.stable"
        );
        assert_eq!(
            template_label(ExactVersion::new(4, 4, 0), "stable"),
            "4.4.stable"
        );
    }

    #[test]
    fn each_desktop_preset_needs_exactly_one_template() {
        let linux = target_for(
            &preset("Linux", "binary_format/architecture=\"x86_64\""),
            false,
        )
        .expect("linux");
        assert_eq!(linux.template, "linux_release.x86_64");
        assert_eq!(linux.extension, "x86_64");
        let debug = target_for(&preset("Linux", ""), true).expect("default architecture");
        assert_eq!(debug.template, "linux_debug.x86_64");
        let windows = target_for(
            &preset("Windows Desktop", "binary_format/architecture=\"arm64\""),
            false,
        )
        .expect("windows");
        assert_eq!(windows.template, "windows_release_arm64.exe");
        assert!(!windows.runnable_here || cfg!(target_os = "windows"));
        assert!(matches!(
            target_for(&preset("Android", ""), false),
            Err(TemplateError::Unsupported(_))
        ));
        assert!(matches!(
            target_for(
                &preset("Linux", "custom_template/release=\"/opt/t\""),
                false
            ),
            Err(TemplateError::CustomTemplate { mode: "release" })
        ));
    }

    #[test]
    fn a_macos_preset_exports_its_app_bundle_through_the_one_macos_template() {
        for debug in [false, true] {
            let mac = target_for(
                &preset(
                    "macOS",
                    "application/bundle_identifier=\"org.example.game\"",
                ),
                debug,
            )
            .expect("a macOS preset");
            assert_eq!(mac.template, "macos.zip", "debug={debug}");
            assert_eq!(mac.extension, "zip");
            assert!(mac.app_bundle);
            // A preset without an architecture is universal, as in Godot.
            assert_eq!(mac.runnable_here, cfg!(target_os = "macos"));
        }
        let arm = target_for(
            &preset("macOS", "binary_format/architecture=\"arm64\""),
            false,
        )
        .expect("arm64");
        assert_eq!(
            arm.runnable_here,
            cfg!(all(target_os = "macos", target_arch = "aarch64"))
        );
        let intel = target_for(
            &preset("macOS", "binary_format/architecture=\"x86_64\""),
            false,
        )
        .expect("x86_64");
        assert_eq!(
            intel.runnable_here,
            cfg!(all(target_os = "macos", target_arch = "x86_64"))
        );
        for (platform, architecture) in [("macOS", "arm32"), ("Linux", "universal")] {
            assert!(
                matches!(
                    target_for(
                        &preset(
                            platform,
                            &format!("binary_format/architecture=\"{architecture}\"")
                        ),
                        false
                    ),
                    Err(TemplateError::Unsupported(_))
                ),
                "{platform} {architecture}"
            );
        }
        assert!(
            !target_for(&preset("Linux", ""), false)
                .expect("linux")
                .app_bundle
        );
        assert!(exports_app_bundle("macOS"));
        assert!(!exports_app_bundle("Linux") && !exports_app_bundle("Windows Desktop"));
    }

    #[test]
    fn only_the_exact_builds_templates_resolve() {
        let root = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            TemplateSet::resolve(root.path(), "4.7.1.stable"),
            Err(TemplateError::Missing {
                label: "4.7.1.stable".to_owned()
            })
        );
        let directory = root.path().join("4.7.1.stable");
        std::fs::create_dir(&directory).expect("directory");
        std::fs::write(directory.join("version.txt"), "4.7.0.stable\n").expect("version");
        assert!(matches!(
            TemplateSet::resolve(root.path(), "4.7.1.stable"),
            Err(TemplateError::WrongVersion { .. })
        ));
        std::fs::write(directory.join("version.txt"), "4.7.1.stable\n").expect("version");
        let set = TemplateSet::resolve(root.path(), "4.7.1.stable").expect("resolve");
        let target = target_for(&preset("Linux", ""), false).expect("target");
        assert!(matches!(
            set.template(&target),
            Err(TemplateError::MissingFile { .. })
        ));
        std::fs::write(directory.join("linux_release.x86_64"), b"template").expect("template");
        assert_eq!(
            set.template(&target).expect("template"),
            directory.join("linux_release.x86_64")
        );
    }

    #[test]
    fn the_templates_root_is_where_godot_itself_looks() {
        if cfg!(target_os = "linux") {
            assert_eq!(
                default_templates_root(Some(Path::new("/data")), Some(Path::new("/home/u")), None),
                Some(PathBuf::from("/data/godot/export_templates"))
            );
            assert_eq!(
                default_templates_root(None, Some(Path::new("/home/u")), None),
                Some(PathBuf::from("/home/u/.local/share/godot/export_templates"))
            );
            // A relative XDG_DATA_HOME is ignored, as the specification says.
            assert_eq!(
                default_templates_root(
                    Some(Path::new("relative")),
                    Some(Path::new("/home/u")),
                    None
                ),
                Some(PathBuf::from("/home/u/.local/share/godot/export_templates"))
            );
            assert_eq!(default_templates_root(None, None, None), None);
        }
    }
}
