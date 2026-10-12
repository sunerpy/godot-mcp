//! The committed `Cargo.lock`, parsed into the exact resolved graph the release
//! binary links.
//!
//! Both the CycloneDX SBOM and the release-graph audit need the same answer to
//! the same question — *which crates actually end up inside the shipping
//! binary?* — so the lock is parsed once, here, and both consume this view. The
//! transitive closure from a root member is the release binary's linked-crate
//! list: nothing outside it can be linked, and nothing inside it can be omitted.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// One locked package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedPackage {
    /// Crate name exactly as the registry spells it.
    pub name: String,
    /// Resolved version.
    pub version: String,
    /// Registry/git source, absent for path (workspace) members.
    pub source: Option<String>,
    /// Direct dependency entries, as spelled in the lock (`name` or
    /// `name version` or `name version (source)`).
    pub dependencies: Vec<String>,
}

impl LockedPackage {
    /// The dependency names this package declares, with any version/source
    /// suffix stripped.
    #[must_use]
    pub fn dependency_names(&self) -> Vec<String> {
        self.dependencies
            .iter()
            .map(|entry| entry.split_whitespace().next().unwrap_or(entry).to_string())
            .collect()
    }
}

/// The parsed `Cargo.lock`.
#[derive(Debug, Clone, Default)]
pub struct CargoLock {
    /// Every locked package, in lock order.
    pub packages: Vec<LockedPackage>,
}

impl CargoLock {
    /// Parse the lock file at `path`.
    ///
    /// # Errors
    /// Returns an error if the file cannot be read or is not a valid lock.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        Self::parse(&text)
    }

    /// Parse lock text.
    ///
    /// # Errors
    /// Returns an error if the text is not valid TOML or a package entry is
    /// missing its name/version.
    pub fn parse(text: &str) -> Result<Self, String> {
        let value: toml::Value =
            toml::from_str(text).map_err(|error| format!("parse Cargo.lock: {error}"))?;
        let mut packages = Vec::new();
        let array = value
            .get("package")
            .and_then(toml::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for entry in array {
            let name = entry
                .get("name")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| "Cargo.lock package without a name".to_string())?
                .to_string();
            let version = entry
                .get("version")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| format!("Cargo.lock package {name} without a version"))?
                .to_string();
            let source = entry
                .get("source")
                .and_then(toml::Value::as_str)
                .map(str::to_string);
            let dependencies = entry
                .get("dependencies")
                .and_then(toml::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            packages.push(LockedPackage {
                name,
                version,
                source,
                dependencies,
            });
        }
        Ok(Self { packages })
    }

    /// Every locked crate name.
    #[must_use]
    pub fn names(&self) -> BTreeSet<String> {
        self.packages
            .iter()
            .map(|package| package.name.clone())
            .collect()
    }

    fn by_name(&self) -> BTreeMap<&str, &LockedPackage> {
        self.packages
            .iter()
            .map(|package| (package.name.as_str(), package))
            .collect()
    }

    /// The transitive closure of `root`, inclusive: the exact linked-crate list
    /// of a binary built from that workspace member.
    ///
    /// A dependency named in the lock but absent from it is reported rather than
    /// silently dropped, because a linked-crate list with a hole proves nothing.
    ///
    /// # Errors
    /// Returns an error if `root` is not in the lock or a named dependency is
    /// missing from it.
    pub fn linked_crates(&self, root: &str) -> Result<BTreeSet<String>, String> {
        let index = self.by_name();
        if !index.contains_key(root) {
            return Err(format!("Cargo.lock does not contain root package {root}"));
        }
        let mut linked = BTreeSet::new();
        let mut frontier = vec![root.to_string()];
        while let Some(name) = frontier.pop() {
            if !linked.insert(name.clone()) {
                continue;
            }
            let package = index.get(name.as_str()).ok_or_else(|| {
                format!("Cargo.lock names dependency {name} but does not lock it")
            })?;
            for dependency in package.dependency_names() {
                if !linked.contains(&dependency) {
                    frontier.push(dependency);
                }
            }
        }
        Ok(linked)
    }
}
