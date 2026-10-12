//! One reported finding.
//!
//! A finding is structurally incapable of carrying sensitive material. Its only
//! variable parts are a [`AuditDigest`] subject and a `u64` measure; everything
//! else — the code, the component, the remediation — is a compiled-in constant.
//! There is no `String`, no `PathBuf` and no byte buffer in the type, so a raw
//! secret, a source line or an absolute private path cannot reach a rendered
//! diagnostic even if a caller passed one in.

use godot_mcp_domain::contracts::ComponentName;

use super::code::{DiagnosticCode, DiagnosticSeverity};
use crate::audit::AuditDigest;

/// One observed defect, classified and made actionable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    code: DiagnosticCode,
    severity: DiagnosticSeverity,
    subject: Option<AuditDigest>,
    measure: Option<u64>,
}

impl Finding {
    /// Record one finding.
    ///
    /// The severity is clamped for an optional absence, so a caller cannot
    /// promote a deliberately unshipped bridge into a server failure by passing a
    /// stronger severity.
    #[must_use]
    pub(super) fn new(
        code: DiagnosticCode,
        severity: DiagnosticSeverity,
        subject: Option<AuditDigest>,
        measure: Option<u64>,
    ) -> Self {
        let severity = if code.is_optional_absence() {
            severity.clamp_optional()
        } else {
            severity
        };
        Self {
            code,
            severity,
            subject,
            measure,
        }
    }

    /// The stable machine-readable code.
    #[must_use]
    pub const fn code(&self) -> DiagnosticCode {
        self.code
    }

    /// How much this finding matters.
    #[must_use]
    pub const fn severity(&self) -> DiagnosticSeverity {
        self.severity
    }

    /// Which public health component it belongs to.
    #[must_use]
    pub const fn component(&self) -> ComponentName {
        self.code.component()
    }

    /// The digest identifying what the finding is about, when there is one.
    #[must_use]
    pub fn subject(&self) -> Option<&AuditDigest> {
        self.subject.as_ref()
    }

    /// A bounded numeric detail, when there is one.
    #[must_use]
    pub const fn measure(&self) -> Option<u64> {
        self.measure
    }

    /// What an operator can do about it.
    #[must_use]
    pub const fn remediation(&self) -> &'static str {
        self.code.remediation()
    }

    /// Whether this finding holds mutations closed.
    #[must_use]
    pub const fn blocks_mutation(&self) -> bool {
        self.code.blocks_mutation()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A caller cannot promote an optional absence into a blocking finding.
    #[test]
    fn an_optional_absence_cannot_be_promoted_to_blocked() {
        let finding = Finding::new(
            DiagnosticCode::EditorPlaneNotShipped,
            DiagnosticSeverity::Blocked,
            None,
            None,
        );
        assert_eq!(finding.severity(), DiagnosticSeverity::Degraded);
        assert!(!finding.blocks_mutation());
        assert_eq!(finding.component(), ComponentName::Editor);
        assert!(finding.remediation().contains("this server attaches no"));
    }

    /// A mandatory finding keeps the severity it was given.
    #[test]
    fn a_mandatory_finding_keeps_its_severity() {
        let finding = Finding::new(
            DiagnosticCode::JwksUnreachable,
            DiagnosticSeverity::Blocked,
            Some(AuditDigest::of_str("issuer", "https://issuer.example")),
            Some(3),
        );
        assert_eq!(finding.severity(), DiagnosticSeverity::Blocked);
        assert_eq!(finding.measure(), Some(3));
        assert!(finding.subject().is_some());
    }
}
