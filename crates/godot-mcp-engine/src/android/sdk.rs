//! The tools inside the operator's Android SDK and JDK.
//!
//! An SDK keeps `adb` in `platform-tools/` and `apksigner` in each
//! `build-tools/<version>/`; a JDK keeps `java` and `keytool` in `bin/`.
//! Each tool is found at exactly that place, verified to be a regular file,
//! and never searched for anywhere else.

use std::path::{Path, PathBuf};

/// Why a configured SDK or JDK lacks a tool.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SdkError {
    /// The directory itself is missing.
    #[error("the configured {what} directory does not exist or is not a directory")]
    NotADirectory {
        /// `Android SDK` or `JDK`.
        what: &'static str,
    },
    /// A tool is missing at its place.
    #[error("the configured {what} has no `{tool}`")]
    MissingTool {
        /// `Android SDK` or `JDK`.
        what: &'static str,
        /// The tool's place inside it.
        tool: String,
    },
}

const SDK: &str = "Android SDK";
const JDK: &str = "JDK";

fn executable(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

fn regular_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// The operator's Android SDK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidSdk {
    root: PathBuf,
}

impl AndroidSdk {
    /// The SDK at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Its directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn present(&self) -> Result<(), SdkError> {
        if self.root.is_dir() {
            Ok(())
        } else {
            Err(SdkError::NotADirectory { what: SDK })
        }
    }

    /// `platform-tools/adb`.
    ///
    /// # Errors
    /// Returns [`SdkError`] when the SDK or its `adb` is missing.
    pub fn adb(&self) -> Result<PathBuf, SdkError> {
        self.present()?;
        let tool = executable("adb");
        let path = self.root.join("platform-tools").join(&tool);
        if regular_file(&path) {
            Ok(path)
        } else {
            Err(SdkError::MissingTool {
                what: SDK,
                tool: format!("platform-tools/{tool}"),
            })
        }
    }

    /// The `build-tools` versions holding `apksigner`, newest first.
    #[must_use]
    pub fn apksigner_versions(&self) -> Vec<String> {
        let tool = if cfg!(windows) {
            "apksigner.bat"
        } else {
            "apksigner"
        };
        let Ok(listing) = std::fs::read_dir(self.root.join("build-tools")) else {
            return Vec::new();
        };
        let mut versions: Vec<(Vec<u64>, String)> = listing
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_owned();
                let numbers: Option<Vec<u64>> =
                    name.split('.').map(|part| part.parse().ok()).collect();
                let numbers = numbers?;
                regular_file(&entry.path().join(tool)).then_some((numbers, name))
            })
            .collect();
        versions.sort();
        versions.reverse();
        versions.into_iter().map(|(_, name)| name).collect()
    }

    /// The `apksigner` of the newest `build-tools` version that has one, and
    /// that version.
    ///
    /// # Errors
    /// Returns [`SdkError`] when the SDK is missing or no `build-tools`
    /// version holds `apksigner`.
    pub fn apksigner(&self) -> Result<(String, PathBuf), SdkError> {
        self.present()?;
        let tool = if cfg!(windows) {
            "apksigner.bat"
        } else {
            "apksigner"
        };
        let version = self
            .apksigner_versions()
            .into_iter()
            .next()
            .ok_or_else(|| SdkError::MissingTool {
                what: SDK,
                tool: format!("build-tools/<version>/{tool}"),
            })?;
        let path = self.root.join("build-tools").join(&version).join(tool);
        Ok((version, path))
    }
}

/// The operator's JDK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jdk {
    home: PathBuf,
}

impl Jdk {
    /// The JDK at `home`.
    #[must_use]
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// Its directory.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Its `bin` directory, which a tool that runs `java` needs on `PATH`.
    #[must_use]
    pub fn bin(&self) -> PathBuf {
        self.home.join("bin")
    }

    fn tool(&self, name: &str) -> Result<PathBuf, SdkError> {
        if !self.home.is_dir() {
            return Err(SdkError::NotADirectory { what: JDK });
        }
        let tool = executable(name);
        let path = self.bin().join(&tool);
        if regular_file(&path) {
            Ok(path)
        } else {
            Err(SdkError::MissingTool {
                what: JDK,
                tool: format!("bin/{tool}"),
            })
        }
    }

    /// `bin/java`.
    ///
    /// # Errors
    /// Returns [`SdkError`] when the JDK or its `java` is missing.
    pub fn java(&self) -> Result<PathBuf, SdkError> {
        self.tool("java")
    }

    /// `bin/keytool`.
    ///
    /// # Errors
    /// Returns [`SdkError`] when the JDK or its `keytool` is missing.
    pub fn keytool(&self) -> Result<PathBuf, SdkError> {
        self.tool("keytool")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, b"tool").expect("tool");
    }

    #[test]
    fn each_tool_is_found_exactly_where_the_sdk_keeps_it() {
        let root = tempfile::tempdir().expect("tempdir");
        let sdk = AndroidSdk::new(root.path());
        assert_eq!(
            sdk.adb(),
            Err(SdkError::MissingTool {
                what: SDK,
                tool: format!("platform-tools/{}", executable("adb")),
            })
        );
        touch(&root.path().join("platform-tools").join(executable("adb")));
        assert_eq!(
            sdk.adb().expect("adb"),
            root.path().join("platform-tools").join(executable("adb"))
        );
        assert!(sdk.apksigner().is_err());
        let signer = if cfg!(windows) {
            "apksigner.bat"
        } else {
            "apksigner"
        };
        for version in ["35.0.0", "36.0.0", "9.0.0"] {
            touch(&root.path().join("build-tools").join(version).join(signer));
        }
        // A version without apksigner and one that is not a version are skipped.
        std::fs::create_dir_all(root.path().join("build-tools").join("37.0.0")).expect("dir");
        touch(&root.path().join("build-tools").join("preview").join(signer));
        assert_eq!(sdk.apksigner_versions(), ["36.0.0", "35.0.0", "9.0.0"]);
        assert_eq!(sdk.apksigner().expect("apksigner").0, "36.0.0");
        assert_eq!(
            AndroidSdk::new(root.path().join("absent")).adb(),
            Err(SdkError::NotADirectory { what: SDK })
        );
    }

    #[test]
    fn a_jdk_keeps_its_tools_in_bin() {
        let root = tempfile::tempdir().expect("tempdir");
        let jdk = Jdk::new(root.path());
        assert!(jdk.keytool().is_err());
        touch(&root.path().join("bin").join(executable("keytool")));
        assert_eq!(
            jdk.keytool().expect("keytool"),
            root.path().join("bin").join(executable("keytool"))
        );
        assert!(jdk.java().is_err());
        assert_eq!(jdk.bin(), root.path().join("bin"));
    }
}
