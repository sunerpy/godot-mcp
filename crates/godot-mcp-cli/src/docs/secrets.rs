//! The documentation credential scanner.
//!
//! Two layers, because either alone would be weak:
//!
//! * the canonical markers the release packager already refuses in a shipping
//!   payload ([`crate::package::credential_markers`]), reused verbatim so the
//!   documentation tree and the release archive are held to one standard;
//! * structural credential shapes that a copy-pasted real credential would carry
//!   even when it matches no known vendor prefix.
//!
//! A hex digest is deliberately not a pattern: this repository documents SHA-256
//! digests everywhere, and treating long hex as a credential would make the
//! scanner noise rather than a gate. [`prove_detects_planted_credential`] pins
//! that the scanner is not vacuous, so a rule set that silently matches nothing
//! fails instead of reporting a clean tree.

use super::{DocFile, Finding};

/// Vendor-agnostic credential shapes, stored reversed for the same reason the
/// packager stores its markers reversed: this scanner's own image must not
/// contain the byte sequence it looks for, or every build would flag itself.
const REVERSED_STRUCTURAL_MARKERS: &[&str] = &[
    "-----YEK ETAVIRP CE NIGEB-----",
    "_phg",
    "-bxox",
    "iOicGbhJye",
    "azIA",
];

/// The structural markers, in plain form.
#[must_use]
pub fn structural_markers() -> Vec<String> {
    REVERSED_STRUCTURAL_MARKERS
        .iter()
        .map(|marker| marker.chars().rev().collect())
        .collect()
}

/// Every marker the documentation tree is scanned for.
#[must_use]
pub fn all_markers() -> Vec<String> {
    let mut markers = crate::package::credential_markers();
    markers.extend(structural_markers());
    markers.sort();
    markers.dedup();
    markers
}

/// Scan one page's text for a credential marker.
#[must_use]
pub fn scan_text(file: &str, text: &str, markers: &[String]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (index, line) in text.lines().enumerate() {
        for marker in markers {
            if line.contains(marker.as_str()) {
                findings.push(Finding::at(
                    file,
                    index + 1,
                    "credential-in-docs",
                    "a credential-shaped value must never appear in documentation; use a \
                     redaction placeholder or a configured indirection",
                ));
            }
        }
    }
    findings
}

/// Scan every documentation page.
#[must_use]
pub fn check(pages: &[DocFile]) -> Vec<Finding> {
    let markers = all_markers();
    pages
        .iter()
        .flat_map(|page| scan_text(&page.relative, &page.text, &markers))
        .collect()
}

/// Prove the scanner is not vacuous by planting one credential per marker.
///
/// Returns a finding for every marker the scanner failed to detect. An empty
/// result means every configured marker is live.
#[must_use]
pub fn prove_detects_planted_credential() -> Vec<Finding> {
    let markers = all_markers();
    if markers.is_empty() {
        return vec![Finding::file_level(
            "",
            "secret-scanner-self-check",
            "the credential scanner has no marker configured, so a clean result would be \
             meaningless",
        )];
    }
    let mut failures = Vec::new();
    for marker in &markers {
        let planted = format!("intro line\nleading {marker} trailing\ntail line\n");
        let detected = scan_text("docs/planted.md", &planted, &markers);
        if !detected.iter().any(|finding| finding.line == 2) {
            failures.push(Finding::file_level(
                "",
                "secret-scanner-self-check",
                format!("the credential scanner did not detect a planted {marker:?}"),
            ));
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scanner_detects_every_configured_marker() {
        assert_eq!(prove_detects_planted_credential(), Vec::new());
        assert!(all_markers().len() >= 10);
    }

    #[test]
    fn a_hex_digest_is_not_treated_as_a_credential() {
        let digest = "d6e382fb531019f85630c1f485a561a0d20c4a2344b6c3847735cfee7da812aa";
        assert_eq!(
            scan_text("docs/x.md", &format!("sha256 `{digest}`\n"), &all_markers()),
            Vec::new()
        );
    }

    #[test]
    fn the_reversed_markers_round_trip_to_the_expected_plaintext() {
        let markers = structural_markers();
        assert!(
            markers
                .iter()
                .any(|marker| marker.starts_with("-----BEGIN") && marker.ends_with("KEY-----")),
            "{markers:?}"
        );
        for expected in ["ghp_", "xoxb-", "AIza"] {
            assert!(
                markers.iter().any(|marker| marker == expected),
                "{markers:?}"
            );
        }
    }
}
