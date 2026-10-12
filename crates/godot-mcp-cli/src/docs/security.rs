//! The documentation security lint.
//!
//! ## Scan surface
//!
//! Only files under `docs/` are scanned. That bounds the exemption machinery to
//! genuine in-documentation quotations: this module's own source, the checker's
//! source and every test fixture necessarily contain the terms being forbidden,
//! and none of them is documentation, so none of them needs an opt-out.
//!
//! ## Two narrow exemptions, both verified rather than trusted
//!
//! A blanket "ignore this line" marker would make the lint decorative, so neither
//! exemption is a free-text opt-out:
//!
//! * [`QUOTED_TERM_MARKER`] permits a forbidden term only on a line of
//!   [`QUOTED_TERM_PAGE`], and only when every back-ticked span on that line
//!   containing the term is a substring of the engine's own `NOT_A_SANDBOX`
//!   constant. A sentence such as "godot-mcp is fully contained" written with the
//!   forbidden term is not a substring of that constant, so the marker cannot
//!   launder a false claim.
//! * [`ABSENCE_CLAIM_MARKER`] permits naming a forbidden operator affordance only
//!   on a line that also asserts, in one of a fixed set of absence phrasings, that
//!   the affordance does not exist. It cannot be used to document usage.
//!
//! [`self_check`] proves both exemptions still reject the abuse they exist to
//! prevent, and the checker runs it on every invocation, so an over-broad rewrite
//! of the exemption logic fails in production and not only under `cargo test`.

use godot_mcp_engine::disposable::posture::NOT_A_SANDBOX;

use super::scan::{Scanned, inline_spans, scan};
use super::{DocFile, Finding};

/// Marker permitting a verbatim quotation of the canonical containment posture.
pub const QUOTED_TERM_MARKER: &str = "<!-- lint:quoted-term -->";

/// Marker permitting an explicit statement that an affordance does not exist.
pub const ABSENCE_CLAIM_MARKER: &str = "<!-- lint:absence-claim -->";

/// The only page allowed to quote the canonical containment posture verbatim.
pub const QUOTED_TERM_PAGE: &str = "docs/security/threat-model.md";

/// Phrasings that make a line an absence claim rather than usage guidance.
pub const ABSENCE_PHRASES: &[&str] = &[
    "there is no",
    "there are no",
    "does not exist",
    "do not exist",
    "is deliberately absent",
    "are deliberately absent",
    "is not a recognized",
    "are not recognized",
    "is refused",
    "never accepts",
];

/// Terms that must never describe this project's isolation.
pub const CONTAINMENT_TERMS: &[&str] = &["sandbox", "sandboxed", "sandboxing", "sandboxes"];

/// Operator affordances that must never be documented as available.
pub const FORBIDDEN_AFFORDANCES: &[&str] = &[
    "--force",
    "force overwrite",
    "force-overwrite",
    "quarantine clear",
    "clear quarantine",
    "quarantine-clear",
    "--clear-quarantine",
    "--skip-proof",
];

/// Phrasings that would present a service model this project does not offer.
pub const FORBIDDEN_SERVICE_CLAIMS: &[&str] = &[
    "multi-tenant",
    "multitenant",
    "multi tenancy",
    "multi-tenancy",
    "tenant isolation",
    "saas",
    "hosted service",
    "shared deployment",
];

/// Configuration or prose that would put TLS termination inside this process.
pub const FORBIDDEN_TLS: &[&str] = &[
    "tls_cert",
    "tls_key",
    "tls-cert",
    "tls-key",
    "certificate_file",
    "serves https",
    "serve https",
    "https listener",
];

/// Phrasings that would treat a loopback address as an identity.
pub const FORBIDDEN_LOOPBACK_IDENTITY: &[&str] = &[
    "loopback is identity",
    "loopback means trusted",
    "loopback implies trust",
    "loopback proves",
    "trusted because it is loopback",
    "trusted because it is local",
    "localhost is trusted",
    "no authentication is needed on loopback",
];

/// Positive promises about surfaces this project does not support.
pub const FORBIDDEN_SUPPORT_CLAIMS: &[&str] = &[
    "supports c#",
    "support for c#",
    "c# is supported",
    "csharp is supported",
    "supports binary scene",
    "supports binary resource",
    "binary scenes are supported",
    "supports rendering",
    "renders the scene",
    "pixel-accurate",
];

/// Unverified competitor tool-count claims.
pub const FORBIDDEN_TOOL_COUNTS: &[&str] = &["169", "175"];

/// The reserved example domain every documented example must use.
pub const RESERVED_DOMAIN: &str = "example.invalid";

/// Hosts an example may name besides the reserved domain.
pub const ALLOWED_EXAMPLE_HOSTS: &[&str] = &["127.0.0.1", "localhost", "[::1]", "::1"];

/// Redaction placeholders a credential-shaped example value may use.
pub const REDACTION_PLACEHOLDERS: &[&str] = &["<redacted>", "[redacted]", "redacted"];

/// Indirection spellings that stand in for a credential value.
pub const CREDENTIAL_INDIRECTIONS: &[&str] = &["env =", "file =", "path =", "env.var", "file.path"];

/// Keys whose example value must be redacted or an indirection.
pub const CREDENTIAL_KEYS: &[&str] = &[
    "secret",
    "token",
    "password",
    "api_key",
    "api-key",
    "apikey",
    "bearer",
    "authorization",
];

fn is_absence_claim(line_lower: &str) -> bool {
    line_lower.contains(ABSENCE_CLAIM_MARKER)
        && ABSENCE_PHRASES
            .iter()
            .any(|phrase| line_lower.contains(phrase))
}

fn is_verified_posture_quotation(file: &str, line: &str) -> bool {
    if file != QUOTED_TERM_PAGE || !line.contains(QUOTED_TERM_MARKER) {
        return false;
    }
    let quoted: Vec<String> = inline_spans(line)
        .into_iter()
        .filter(|span| {
            let lower = span.to_ascii_lowercase();
            CONTAINMENT_TERMS.iter().any(|term| lower.contains(term))
        })
        .collect();
    // A marker with no quoted span would exempt the rest of the line, so the
    // exemption only takes effect when there is at least one offending span and
    // every one of them is genuinely part of the canonical constant.
    !quoted.is_empty()
        && quoted
            .iter()
            .all(|span| NOT_A_SANDBOX.contains(span.as_str()))
}

fn deny_terms(
    findings: &mut Vec<Finding>,
    file: &str,
    line_number: usize,
    line_lower: &str,
    terms: &[&str],
    rule: &str,
    detail: &str,
) {
    for term in terms {
        if line_lower.contains(term) {
            findings.push(Finding::at(
                file,
                line_number,
                rule,
                format!("{detail} (found {term:?})"),
            ));
        }
    }
}

/// The first bare major-minor Godot alias on a line, if any.
///
/// A bare `4.x` must be rejected while `4.x.y` must not, so the scan checks the
/// byte after the minor digit and refuses a match when it continues the version.
fn godot_alias_at(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    for start in 0..bytes.len() {
        if bytes[start] != b'4' {
            continue;
        }
        if start > 0 && (bytes[start - 1].is_ascii_digit() || bytes[start - 1] == b'.') {
            continue;
        }
        let rest = &bytes[start..];
        if rest.len() < 3 || rest[1] != b'.' || !(b'4'..=b'7').contains(&rest[2]) {
            continue;
        }
        match rest.get(3).copied() {
            Some(byte) if byte == b'.' || byte.is_ascii_digit() => {}
            _ => return Some(String::from_utf8_lossy(&rest[..3]).into_owned()),
        }
    }
    None
}

/// This project's own repository and release downloads, as `host/path` prefixes.
/// Installing reaches them anonymously by design, so an example that clones the
/// repository or fetches a release asset or installer is not an example endpoint.
pub const PROJECT_DISTRIBUTION_URLS: &[&str] = &[
    "github.com/sunerpy/godot-mcp",
    "raw.githubusercontent.com/sunerpy/godot-mcp",
];

/// Whether the URL text after `://` is one of [`PROJECT_DISTRIBUTION_URLS`]: the
/// prefix followed by the end of the URL, a path or `.git`, never by a longer
/// repository name.
fn is_project_distribution_url(after_scheme: &str) -> bool {
    PROJECT_DISTRIBUTION_URLS.iter().any(|prefix| {
        after_scheme.strip_prefix(prefix).is_some_and(|rest| {
            rest.is_empty()
                || rest.starts_with('/')
                || rest.starts_with(".git")
                || rest.starts_with(['"', '\'', ' ', ')', '>', ',', ';'])
        })
    })
}

/// Every `scheme://host` host appearing on a line, except this project's own
/// repository and release downloads.
fn extract_hosts(line: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    let mut rest = line;
    while let Some(position) = rest.find("://") {
        let after = &rest[position + 3..];
        let end = after
            .find(['/', '"', '\'', ' ', ')', '>', ',', ';', '\\'])
            .unwrap_or(after.len());
        if is_project_distribution_url(after) {
            rest = &after[end..];
            continue;
        }
        let authority = &after[..end];
        let host = authority
            .rsplit_once(':')
            .map_or(authority, |(host, port)| {
                if !port.is_empty() && port.chars().all(|ch| ch.is_ascii_digit()) {
                    host
                } else {
                    authority
                }
            });
        if !host.is_empty() {
            hosts.push(host.to_owned());
        }
        rest = &after[end..];
    }
    hosts
}

fn check_hosts_in_fence(findings: &mut Vec<Finding>, file: &str, scanned: &Scanned) {
    for fence in &scanned.fences {
        for (offset, body_line) in fence.body.iter().enumerate() {
            let line_number = fence.body_start_line + offset;
            for host in extract_hosts(body_line) {
                let allowed = host == RESERVED_DOMAIN
                    || host.ends_with(&format!(".{RESERVED_DOMAIN}"))
                    || ALLOWED_EXAMPLE_HOSTS.contains(&host.as_str());
                if !allowed {
                    findings.push(Finding::at(
                        file,
                        line_number,
                        "example-host",
                        format!(
                            "example host {host:?} is neither loopback nor under the reserved \
                             domain {RESERVED_DOMAIN}"
                        ),
                    ));
                }
            }
        }
    }
}

fn check_credential_values(findings: &mut Vec<Finding>, file: &str, scanned: &Scanned) {
    for fence in &scanned.fences {
        for (offset, body_line) in fence.body.iter().enumerate() {
            let line_number = fence.body_start_line + offset;
            let lower = body_line.to_ascii_lowercase();
            // Only an assignment whose *key* names a credential can carry one. A
            // credential word appearing in a value — an environment variable's
            // name, for instance — is an indirection, not a secret.
            let Some((key, value)) = lower
                .split_once('=')
                .or_else(|| lower.split_once(':'))
                .map(|(key, value)| (key.trim(), value.trim()))
            else {
                continue;
            };
            if !CREDENTIAL_KEYS.iter().any(|name| key.contains(name)) {
                continue;
            }
            let redacted = REDACTION_PLACEHOLDERS
                .iter()
                .any(|marker| value.contains(marker))
                || CREDENTIAL_INDIRECTIONS
                    .iter()
                    .any(|marker| lower.contains(marker))
                || value.contains("$godot_mcp")
                || value.contains("${godot_mcp");
            if !redacted {
                findings.push(Finding::at(
                    file,
                    line_number,
                    "unredacted-credential-example",
                    "an example naming a credential must show a redaction placeholder or a \
                     configured indirection, never a value",
                ));
            }
        }
    }
}

/// Evidence that an example carries an authenticated request rather than an open
/// one. A public endpoint reached without any of these is an unauthenticated
/// public example.
pub const AUTHENTICATION_EVIDENCE: &[&str] = &[
    "authorization:",
    "authorization\"",
    "bearer",
    "x-godot-mcp-proxy-hop",
    "proxy_hop",
    "local_secret",
    "<redacted>",
];

/// Fence languages whose body is a request or a request-forwarding rule, and so
/// must itself show the credential presented. A configuration document (`toml`,
/// `json`) declares an endpoint rather than issuing a request; its authentication
/// is expressed by separate keys and verified by the checker's accept/reject
/// annotation, not by an inline header.
pub const REQUEST_FENCE_INFO: &[&str] = &["sh", "bash", "console", "http", "nginx"];

fn check_unauthenticated_public_examples(
    findings: &mut Vec<Finding>,
    file: &str,
    scanned: &Scanned,
) {
    for fence in scanned
        .fences
        .iter()
        .filter(|fence| REQUEST_FENCE_INFO.contains(&fence.info.as_str()))
    {
        let body = fence.body.join("\n").to_ascii_lowercase();
        let external = fence.body.iter().enumerate().find(|(_, body_line)| {
            extract_hosts(body_line)
                .iter()
                .any(|host| !ALLOWED_EXAMPLE_HOSTS.contains(&host.as_str()))
        });
        let Some((offset, _)) = external else {
            continue;
        };
        let authenticated = AUTHENTICATION_EVIDENCE
            .iter()
            .any(|marker| body.contains(marker));
        if !authenticated {
            findings.push(Finding::at(
                file,
                fence.body_start_line + offset,
                "unauthenticated-public-example",
                "an example that reaches a non-loopback endpoint must show the credential it \
                 presents; every externally reachable request is authenticated",
            ));
        }
    }
}

fn check_non_loopback_bind(findings: &mut Vec<Finding>, file: &str, scanned: &Scanned) {
    const UNSPECIFIED: &[&str] = &["0.0.0.0", "[::]"];
    for fence in &scanned.fences {
        for (offset, body_line) in fence.body.iter().enumerate() {
            let line_number = fence.body_start_line + offset;
            for marker in UNSPECIFIED {
                if body_line.contains(marker) {
                    findings.push(Finding::at(
                        file,
                        line_number,
                        "non-loopback-bind",
                        format!(
                            "an example must never bind or advertise the unspecified address \
                             {marker}; the backend binds loopback only"
                        ),
                    ));
                }
            }
        }
    }
}

fn check_artifact_paths(findings: &mut Vec<Finding>, file: &str, scanned: &Scanned) {
    for (index, line) in scanned.lines.iter().enumerate() {
        let lower = line.to_ascii_lowercase();
        let mut cursor = 0usize;
        while let Some(position) = lower[cursor..].find("artifacts/") {
            let absolute = cursor + position;
            if !lower[..absolute].ends_with("godot-mcp://") {
                findings.push(Finding::at(
                    file,
                    index + 1,
                    "opaque-artifact-only",
                    "an artifact is addressable only as an opaque \
                     `godot-mcp://artifacts/{artifact_id}` resource, never as a path",
                ));
            }
            cursor = absolute + "artifacts/".len();
        }
        for needle in ["artifact_path", "artifact-path"] {
            if lower.contains(needle) {
                findings.push(Finding::at(
                    file,
                    index + 1,
                    "opaque-artifact-only",
                    format!("{needle:?} would expose an artifact's storage location"),
                ));
            }
        }
    }
}

/// Run the security lint over one page.
#[must_use]
pub fn check_page(page: &DocFile) -> Vec<Finding> {
    let scanned = scan(&page.text);
    let file = page.relative.as_str();
    let mut findings = Vec::new();

    for (index, line) in scanned.lines.iter().enumerate() {
        let line_number = index + 1;
        let lower = line.to_ascii_lowercase();

        if CONTAINMENT_TERMS.iter().any(|term| lower.contains(term))
            && !is_verified_posture_quotation(file, line)
        {
            findings.push(Finding::at(
                file,
                line_number,
                "overstated-containment",
                "this project's isolation must never be described with a containment term it \
                 does not provide; only a verified verbatim quotation of the engine's own \
                 posture constant is permitted, on the threat-model page",
            ));
        }

        if !is_absence_claim(&lower) {
            deny_terms(
                &mut findings,
                file,
                line_number,
                &lower,
                FORBIDDEN_AFFORDANCES,
                "forbidden-affordance",
                "no force-overwrite or quarantine-clear bypass exists, so naming one outside \
                 an explicit absence claim is misleading",
            );
        }

        for (terms, rule, detail) in [
            (
                FORBIDDEN_SERVICE_CLAIMS,
                "service-model-claim",
                "this is a local-first single-user server, not a shared or hosted service",
            ),
            (
                FORBIDDEN_TLS,
                "in-process-tls",
                "TLS is terminated by a same-host reverse proxy; this process never terminates it",
            ),
            (
                FORBIDDEN_LOOPBACK_IDENTITY,
                "loopback-as-identity",
                "a loopback address is never an identity or an authorization",
            ),
            (
                FORBIDDEN_SUPPORT_CLAIMS,
                "unsupported-promise",
                "this promises behavior the first release does not implement",
            ),
        ] {
            deny_terms(
                &mut findings,
                file,
                line_number,
                &lower,
                terms,
                rule,
                detail,
            );
        }

        for count in FORBIDDEN_TOOL_COUNTS {
            if lower.contains(count) && lower.contains("tool") {
                findings.push(Finding::at(
                    file,
                    line_number,
                    "unverified-tool-count",
                    format!(
                        "the {count}-tool figure is an unverified third-party marketing claim \
                         and must not be repeated"
                    ),
                ));
            }
        }

        if let Some(alias) = godot_alias_at(line) {
            findings.push(Finding::at(
                file,
                line_number,
                "godot-version-alias",
                format!(
                    "{alias:?} is a major-minor alias; documentation names exact patch versions \
                     from the releases lock only"
                ),
            ));
        }
    }

    check_hosts_in_fence(&mut findings, file, &scanned);
    check_unauthenticated_public_examples(&mut findings, file, &scanned);
    check_credential_values(&mut findings, file, &scanned);
    check_non_loopback_bind(&mut findings, file, &scanned);
    check_artifact_paths(&mut findings, file, &scanned);
    findings
}

/// Run the security lint over every page.
#[must_use]
pub fn check(pages: &[DocFile]) -> Vec<Finding> {
    pages.iter().flat_map(check_page).collect()
}

/// Prove the lint's own exemptions still reject what they exist to prevent.
#[must_use]
pub fn self_check() -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut fail = |detail: &str| {
        findings.push(Finding::file_level(
            "",
            "lint-self-check",
            detail.to_owned(),
        ));
    };
    let term = CONTAINMENT_TERMS[1];
    if is_verified_posture_quotation(
        QUOTED_TERM_PAGE,
        &format!("{QUOTED_TERM_MARKER} godot-mcp is `fully {term}` for you"),
    ) {
        fail("the quoted-term exemption accepted a span outside the canonical posture constant");
    }
    if is_verified_posture_quotation(
        QUOTED_TERM_PAGE,
        &format!("{QUOTED_TERM_MARKER} {term} with no quoted span"),
    ) {
        fail("the quoted-term exemption accepted a line with no back-ticked quotation");
    }
    if is_verified_posture_quotation(
        "docs/operator/quickstart-stdio.md",
        &format!("{QUOTED_TERM_MARKER} `not a code {term}`"),
    ) {
        fail("the quoted-term exemption applied outside the threat-model page");
    }
    if is_absence_claim(
        &format!("{ABSENCE_CLAIM_MARKER} pass --force to override the proof").to_ascii_lowercase(),
    ) {
        fail("the absence-claim exemption accepted a line documenting usage");
    }
    if !is_absence_claim(
        &format!("{ABSENCE_CLAIM_MARKER} there is no `--force` flag").to_ascii_lowercase(),
    ) {
        fail("the absence-claim exemption rejected a genuine absence statement");
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named_page(relative: &str, text: &str) -> DocFile {
        DocFile {
            relative: relative.to_owned(),
            absolute: std::path::PathBuf::from(relative),
            text: text.to_owned(),
        }
    }

    fn page(text: &str) -> DocFile {
        named_page("docs/operator/example.md", text)
    }

    fn rules(findings: &[Finding]) -> Vec<String> {
        findings
            .iter()
            .map(|finding| finding.rule.clone())
            .collect()
    }

    fn triggers(text: &str, rule: &str) -> bool {
        rules(&check_page(&page(text))).iter().any(|r| r == rule)
    }

    #[test]
    fn the_containment_term_is_rejected_and_a_verified_quotation_is_not() {
        let term = CONTAINMENT_TERMS[1];
        assert!(triggers(
            &format!("godot-mcp runs your project {term}.\n"),
            "overstated-containment"
        ));
        let quoted = format!(
            "{QUOTED_TERM_MARKER} The engine states it is `not a code {}`.\n",
            CONTAINMENT_TERMS[0]
        );
        assert!(
            !rules(&check_page(&named_page(QUOTED_TERM_PAGE, &quoted)))
                .iter()
                .any(|rule| rule == "overstated-containment")
        );
        let laundered = format!("{QUOTED_TERM_MARKER} godot-mcp is `fully {term}`.\n");
        assert!(
            rules(&check_page(&named_page(QUOTED_TERM_PAGE, &laundered)))
                .iter()
                .any(|rule| rule == "overstated-containment")
        );
    }

    #[test]
    fn a_forbidden_affordance_needs_an_explicit_absence_claim() {
        assert!(triggers(
            "Use --force to overwrite.\n",
            "forbidden-affordance"
        ));
        let absence = format!("{ABSENCE_CLAIM_MARKER} There is no `--force` flag.\n");
        assert!(!triggers(&absence, "forbidden-affordance"));
    }

    #[test]
    fn examples_must_use_loopback_or_the_reserved_domain() {
        assert!(triggers(
            "```sh\ncurl https://mcp.internal.example.com/mcp\n```\n",
            "example-host"
        ));
        assert!(!triggers(
            "```sh\ncurl https://godot-mcp.example.invalid/mcp\n```\n",
            "example-host"
        ));
        assert!(!triggers(
            "```sh\ncurl http://127.0.0.1:8765/mcp\n```\n",
            "example-host"
        ));
    }

    /// Installing reaches this project's own repository and release downloads,
    /// anonymously by design; any other real host, including another repository
    /// on the same host, is still refused.
    #[test]
    fn the_projects_own_repository_is_not_an_example_host() {
        for own in [
            "```sh\ncurl -fsSL https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.sh | sh\n```\n",
            "```sh\ngit clone https://github.com/sunerpy/godot-mcp.git\n```\n",
            "```sh\ncurl -fsSLO https://github.com/sunerpy/godot-mcp/releases/download/v0.2.0/SHA256SUMS.txt\n```\n",
        ] {
            assert!(!triggers(own, "example-host"), "{own}");
            assert!(!triggers(own, "unauthenticated-public-example"), "{own}");
        }
        for other in [
            "```sh\ncurl -fsSL https://raw.githubusercontent.com/someone/godot-mcp/main/install.sh | sh\n```\n",
            "```sh\ncurl https://github.com/sunerpy/godot-mcp-mirror/releases\n```\n",
            "```sh\ncurl https://github.com/mcp\n```\n",
        ] {
            assert!(triggers(other, "example-host"), "{other}");
            assert!(triggers(other, "unauthenticated-public-example"), "{other}");
        }
    }

    #[test]
    fn a_public_example_must_show_the_credential_it_presents() {
        assert!(triggers(
            "```sh\ncurl https://godot-mcp.example.invalid/mcp\n```\n",
            "unauthenticated-public-example"
        ));
        assert!(!triggers(
            "```sh\ncurl -H 'Authorization: Bearer <redacted>' \
             https://godot-mcp.example.invalid/mcp\n```\n",
            "unauthenticated-public-example"
        ));
        assert!(!triggers(
            "```sh\ncurl http://127.0.0.1:8765/mcp\n```\n",
            "unauthenticated-public-example"
        ));
    }

    #[test]
    fn unspecified_binds_and_unredacted_credentials_are_rejected() {
        assert!(triggers(
            "```toml\nbind = \"0.0.0.0:8765\"\n```\n",
            "non-loopback-bind"
        ));
        assert!(triggers(
            "```sh\nexport GODOT_MCP_TOKEN=abcdef012345\n```\n",
            "unredacted-credential-example"
        ));
        assert!(!triggers(
            "```sh\nAuthorization: Bearer <redacted>\n```\n",
            "unredacted-credential-example"
        ));
    }

    #[test]
    fn raw_artifact_paths_and_version_aliases_are_rejected() {
        assert!(triggers(
            "Read /var/lib/godot-mcp/artifacts/shot.png directly.\n",
            "opaque-artifact-only"
        ));
        assert!(!triggers(
            "Read `godot-mcp://artifacts/{artifact_id}` instead.\n",
            "opaque-artifact-only"
        ));
        assert!(triggers(
            "Needs Godot 4.4 or newer.\n",
            "godot-version-alias"
        ));
        assert!(!triggers(
            "Needs Godot 4.4.1 or newer.\n",
            "godot-version-alias"
        ));
    }

    #[test]
    fn the_service_model_tls_identity_and_support_claims_are_rejected() {
        for (text, rule) in [
            (
                "A multi-tenant deployment shares one server.\n",
                "service-model-claim",
            ),
            ("Set tls_cert to enable it.\n", "in-process-tls"),
            (
                "Requests are trusted because it is loopback.\n",
                "loopback-as-identity",
            ),
            ("The server supports C# scripts.\n", "unsupported-promise"),
            ("It exposes 175 tools.\n", "unverified-tool-count"),
        ] {
            assert!(triggers(text, rule), "{text:?} must trigger {rule}");
        }
    }

    #[test]
    fn the_lint_self_check_is_clean() {
        assert_eq!(self_check(), Vec::new());
    }
}
