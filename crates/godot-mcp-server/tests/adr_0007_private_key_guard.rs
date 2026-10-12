//! Mechanical resource-server guard retained by ADR 0009.
//!
//! ADR 0007 originally accepted `rsa 0.9.10` because the shipped server only
//! *verifies* signatures with public keys. ADR 0009 removed that dependency, but
//! retained the public-key-only invariant as defense in depth: the resource server
//! owns no private key and performs no signing or decryption.
//!
//! That ADR states its invalidation condition in prose, and prose does not fail a
//! build. This file is the mechanical half of the same decision. It fails when
//! production source acquires an RSA private key or a signing/decryption call, and
//! it fails when the one test-only signing module loses its `cfg` gate. Without it,
//! the assumption could stop holding with nothing going red, and nobody would
//! detect the security-boundary expansion that both ADRs require reviewers to see.
//!
//! # What counts as production source
//!
//! Every `.rs` file under `crates/*/src`, with two subtractions, following the
//! `production_source` convention already used by
//! `crates/godot-mcp-server/src/test_support/router_harness.rs`:
//!
//! * comments are blanked, so a doc comment or an ordinary comment that *names* a
//!   banned API is prose, not a use of it. String literals are deliberately kept,
//!   because private key material and PEM labels arrive as literals;
//! * `#[cfg(test)]` items are blanked, because a unit test cannot be compiled into
//!   any shipping build.
//!
//! `#[cfg(any(test, feature = "test-hooks"))]` regions are **not** subtracted. The
//! ADR names "enabling `testkeys`/`test-hooks` in a release" as an invalidation
//! condition, so a signing operation added under `test-hooks` anywhere other than
//! the single audited fixture module must fail this guard too.
//!
//! # The one allowlisted file
//!
//! `crates/godot-mcp-server/src/auth/testkeys.rs` is the only file permitted to
//! construct an `EncodingKey`, and it is permitted *only while* its module
//! declaration stays behind `#[cfg(any(test, feature = "test-hooks"))]`. The gate
//! is asserted here rather than assumed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The decision this file enforces.
const ADR_PATH: &str = "docs/contributor/adr/0007-rustsec-2023-0071-resource-server.md";

/// The decision that removed the vulnerable dependency and retained this guard.
const SUPERSEDING_ADR_PATH: &str = "docs/contributor/adr/0009-aws-lc-rs-jwt-backend.md";

/// This guard's own repository path, quoted by the ADR so the two stay bound.
const GUARD_PATH: &str = "crates/godot-mcp-server/tests/adr_0007_private_key_guard.rs";

/// The only file allowed to hold private key material, and only while gated.
const KEY_FIXTURE: &str = "crates/godot-mcp-server/src/auth/testkeys.rs";

/// The module that declares the fixture, and must keep gating it.
const FIXTURE_PARENT: &str = "crates/godot-mcp-server/src/auth/mod.rs";

/// The exact gate the fixture declaration must carry.
const FIXTURE_GATE: &str = "#[cfg(any(test, feature = \"test-hooks\"))]";

/// The JWKS parser whose public-member shape must stay public-only.
const JWKS_MODULE: &str = "crates/godot-mcp-server/src/auth/jwks.rs";

/// Manifests whose default-feature closure must not reach `test-hooks`.
const RELEASE_MANIFESTS: &[&str] = &[
    "crates/godot-mcp-server/Cargo.toml",
    "crates/godot-mcp-cli/Cargo.toml",
];

/// The members a public JWK may declare, and nothing else.
///
/// RSA private members (`d`, `p`, `q`, `dp`, `dq`, `qi`) and the EC private member
/// (`d`) are absent by construction: adding one would mean the server ingests
/// private JWK material, which the ADR names as an invalidation condition.
const PUBLIC_JWK_MEMBERS: &[&str] = &[
    "alg", "crv", "e", "key_ops", "kid", "kty", "n", "use_", "x", "y",
];

/// JWK members that carry private key material and must never be declared.
const PRIVATE_JWK_MEMBERS: &[&str] = &["d", "dp", "dq", "p", "q", "qi"];

/// The API surface that would put a private key, a signature, or an RSA
/// decryption into production source.
///
/// Two paths reach the vulnerable operation, and both are covered at their
/// narrowest waist rather than by enumerating every leaf call:
///
/// * `jsonwebtoken` — every signing call in that crate needs an `EncodingKey`,
///   which is the single type all of `from_rsa_pem`, `from_ec_pem`, `from_ed_pem`,
///   `from_rsa_der`, `from_ec_der`, `from_ed_der`, `from_secret` and
///   `from_base64_secret` construct. Banning the type therefore covers the whole
///   constructor family, and `jsonwebtoken::encode` / `crypto::sign` are listed
///   too so a failure names the operation rather than only the key.
/// * `rsa` — every private-key operation needs an `RsaPrivateKey`, or one of the
///   typed wrappers built from it (`pkcs1v15::SigningKey`, `pss::SigningKey`,
///   `pss::BlindedSigningKey`, `oaep::DecryptingKey`, `pkcs1v15::DecryptingKey`).
///   The private-key *encodings* (PKCS#1, PKCS#8) are listed because those are the
///   deserializers that ingest private material; the public counterparts
///   (`SubjectPublicKeyInfo`, `from_public_key_*`) are untouched and stay legal.
///
/// The operations themselves (`.decrypt(`, `.sign(`, `.sign_with_rng(`,
/// `.sign_prehash`) are listed independently of the key types, so a private-key
/// operation reached through a re-export, a trait, or a future dependency is still
/// caught without naming that dependency.
const BANNED: &[(&str, &str)] = &[
    (
        "EncodingKey",
        "jsonwebtoken's signing key. It is the one type every jsonwebtoken signing call needs, \
         and the target of every from_rsa_pem / from_ec_pem / from_ed_pem / from_*_der / \
         from_secret constructor",
    ),
    (
        "jsonwebtoken::encode",
        "jsonwebtoken token issuance: this server validates tokens an identity provider issued, \
         and issues none",
    ),
    (
        "crypto::sign",
        "jsonwebtoken raw signature computation. The production path may only reach \
         jsonwebtoken::crypto::verify",
    ),
    (
        "RsaPrivateKey",
        "the rsa crate's private key. Every RSA private-key operation, including the \
         non-constant-time one RUSTSEC-2023-0071 describes, needs one",
    ),
    (
        "SigningKey",
        "a typed signer built from a private key (rsa::pkcs1v15::SigningKey, \
         rsa::pss::SigningKey, rsa::pss::BlindedSigningKey, and the ecdsa/ed25519 equivalents)",
    ),
    (
        "DecryptingKey",
        "a typed RSA decryptor built from a private key (rsa::oaep::DecryptingKey, \
         rsa::pkcs1v15::DecryptingKey)",
    ),
    (
        "from_pkcs1",
        "PKCS#1 is a private-key encoding. Deserializing one means the process holds a private \
         key; the public shape is SubjectPublicKeyInfo / from_public_key_*",
    ),
    (
        "from_pkcs8",
        "PKCS#8 is a private-key encoding. Deserializing one means the process holds a private \
         key; the public shape is SubjectPublicKeyInfo / from_public_key_*",
    ),
    (
        ".decrypt(",
        "an RSA decryption is exactly the private-key operation RUSTSEC-2023-0071 makes \
         observable",
    ),
    (
        ".sign(",
        "a signing operation. A resource server verifies signatures; it produces none",
    ),
    (
        ".sign_with_rng(",
        "an rsa signing operation with an explicit RNG",
    ),
    (".sign_prehash", "an rsa prehashed signing operation"),
    (
        "private_pem",
        "the committed fixture private key. Only the gated test-key module may read it; a \
         production read would compile private material into the shipped binary",
    ),
    (
        "PRIVATE KEY",
        "a private-key PEM label in production source. If the intent is to *detect* the marker \
         rather than ingest a key, store it reversed the way \
         crates/godot-mcp-cli/src/package/mod.rs stores its credential markers, so the pattern \
         never appears in the scanner's own image",
    ),
];

/// One banned-surface match, located precisely enough to act on.
#[derive(Debug)]
struct Finding {
    file: String,
    line: usize,
    token: &'static str,
    surface: &'static str,
}

/// Two aligned views of one Rust source file, produced in a single pass.
struct SourceViews {
    /// The original bytes with every comment byte replaced by a space and every
    /// newline preserved, so byte offsets and line numbers still match the file on
    /// disk. String and character literals survive untouched.
    text: String,
    /// True at byte positions that are code: outside comments, and outside string
    /// and character literals. Used for brace matching only, so a `{` inside a
    /// format string cannot unbalance an item.
    code: Vec<bool>,
}

/// Blank comments and mark code positions, in one pass over the bytes.
///
/// Operating on bytes keeps offsets exact: a multi-byte character inside a comment
/// is replaced byte-for-byte with spaces, so the result is the same length, is
/// still valid UTF-8, and reports the same line numbers as the file.
fn views(source: &str) -> SourceViews {
    let bytes = source.as_bytes();
    let length = bytes.len();
    let mut text = bytes.to_vec();
    let mut code = vec![true; length];
    let mut index = 0;
    while index < length {
        match bytes[index] {
            // Line comment: blank to end of line, keeping the newline.
            b'/' if index + 1 < length && bytes[index + 1] == b'/' => {
                while index < length && bytes[index] != b'\n' {
                    text[index] = b' ';
                    code[index] = false;
                    index += 1;
                }
            }
            // Block comment, which Rust allows to nest.
            b'/' if index + 1 < length && bytes[index + 1] == b'*' => {
                let mut depth = 0_usize;
                while index < length {
                    if index + 1 < length && bytes[index] == b'/' && bytes[index + 1] == b'*' {
                        depth += 1;
                        text[index] = b' ';
                        text[index + 1] = b' ';
                        code[index] = false;
                        code[index + 1] = false;
                        index += 2;
                    } else if index + 1 < length && bytes[index] == b'*' && bytes[index + 1] == b'/'
                    {
                        depth -= 1;
                        text[index] = b' ';
                        text[index + 1] = b' ';
                        code[index] = false;
                        code[index + 1] = false;
                        index += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        if bytes[index] != b'\n' {
                            text[index] = b' ';
                        }
                        code[index] = false;
                        index += 1;
                    }
                }
            }
            // Raw string, possibly byte-prefixed: r"…", r#"…"#, br"…", br#"…"#.
            b'r' | b'b' if raw_string_hashes(bytes, index).is_some() => {
                let (start, hashes) = raw_string_hashes(bytes, index).expect("checked above");
                let mut cursor = start;
                while cursor < length {
                    code[cursor] = false;
                    if bytes[cursor] == b'"' {
                        let mut seen = 0;
                        while cursor + 1 + seen < length && bytes[cursor + 1 + seen] == b'#' {
                            seen += 1;
                        }
                        if seen >= hashes {
                            let closing = (cursor + hashes + 1).min(length);
                            code[cursor..closing].fill(false);
                            cursor = closing;
                            break;
                        }
                    }
                    cursor += 1;
                }
                code[index..start.min(length)].fill(false);
                index = cursor.max(index + 1);
            }
            // Ordinary string literal.
            b'"' => {
                code[index] = false;
                index += 1;
                while index < length {
                    code[index] = false;
                    if bytes[index] == b'\\' {
                        if index + 1 < length {
                            code[index + 1] = false;
                        }
                        index += 2;
                        continue;
                    }
                    if bytes[index] == b'"' {
                        index += 1;
                        break;
                    }
                    index += 1;
                }
            }
            // Character literal, or a lifetime. `'"'` must not open a string.
            b'\'' => {
                if index + 1 < length && bytes[index + 1] == b'\\' {
                    let mut cursor = index + 2;
                    while cursor < length && bytes[cursor] != b'\'' {
                        cursor += 1;
                    }
                    code[index..(cursor + 1).min(length)].fill(false);
                    index = cursor + 1;
                } else if index + 2 < length && bytes[index + 2] == b'\'' {
                    code[index] = false;
                    code[index + 1] = false;
                    code[index + 2] = false;
                    index += 3;
                } else {
                    // A lifetime is code.
                    index += 1;
                }
            }
            _ => index += 1,
        }
    }
    SourceViews {
        text: String::from_utf8(text).expect("blanking preserves UTF-8 boundaries"),
        code,
    }
}

/// If `index` begins a raw-string prefix, return where the body scan starts and how
/// many `#` the terminator needs.
fn raw_string_hashes(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    if index > 0 && (bytes[index - 1].is_ascii_alphanumeric() || bytes[index - 1] == b'_') {
        return None;
    }
    let mut cursor = index;
    if bytes[cursor] == b'b' {
        cursor += 1;
    }
    if cursor >= bytes.len() || bytes[cursor] != b'r' {
        return None;
    }
    cursor += 1;
    let mut hashes = 0;
    while cursor < bytes.len() && bytes[cursor] == b'#' {
        hashes += 1;
        cursor += 1;
    }
    if cursor < bytes.len() && bytes[cursor] == b'"' {
        Some((cursor + 1, hashes))
    } else {
        None
    }
}

/// Blank every `#[cfg(test)]` item, keeping newlines so line numbers still hold.
///
/// An attribute may gate an item with a body (`mod tests { … }`) or a bare item
/// (`use super::*;`), so whichever of `{` or `;` comes first in *code* decides how
/// far the blanking reaches.
fn blank_cfg_test_items(views: &SourceViews) -> String {
    const ATTRIBUTE: &str = "#[cfg(test)]";
    let mut bytes = views.text.clone().into_bytes();
    let mut search_from = 0;
    while let Some(relative) = views.text[search_from..].find(ATTRIBUTE) {
        let start = search_from + relative;
        if !views.code[start] {
            search_from = start + ATTRIBUTE.len();
            continue;
        }
        let mut cursor = start + ATTRIBUTE.len();
        let mut end = None;
        while cursor < bytes.len() {
            if views.code[cursor] {
                if bytes[cursor] == b';' {
                    end = Some(cursor);
                    break;
                }
                if bytes[cursor] == b'{' {
                    let mut depth = 0_usize;
                    let mut inner = cursor;
                    while inner < bytes.len() {
                        if views.code[inner] {
                            if bytes[inner] == b'{' {
                                depth += 1;
                            } else if bytes[inner] == b'}' {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                        }
                        inner += 1;
                    }
                    end = Some(inner.min(bytes.len() - 1));
                    break;
                }
            }
            cursor += 1;
        }
        // Blank whole characters only, so the result stays valid UTF-8 and every
        // later offset stays on a character boundary.
        let mut past_end = end.unwrap_or(bytes.len() - 1) + 1;
        while past_end < bytes.len() && !views.text.is_char_boundary(past_end) {
            past_end += 1;
        }
        for byte in &mut bytes[start..past_end] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
        search_from = past_end;
    }
    String::from_utf8(bytes).expect("blanking preserves UTF-8 boundaries")
}

/// The production text of one source file: comments blanked, `#[cfg(test)]` items
/// blanked, string literals kept, byte offsets and line numbers unchanged.
fn production_source(source: &str) -> String {
    blank_cfg_test_items(&views(source))
}

/// 1-based line number of `offset` in `text`.
fn line_of(text: &str, offset: usize) -> usize {
    1 + text.as_bytes()[..offset]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
}

/// Every banned-surface match in one file's production text.
fn scan(relative: &str, source: &str) -> Vec<Finding> {
    let text = production_source(source);
    let mut findings = Vec::new();
    for (token, surface) in BANNED {
        let mut search_from = 0;
        while let Some(relative_offset) = text[search_from..].find(token) {
            let offset = search_from + relative_offset;
            findings.push(Finding {
                file: relative.to_owned(),
                line: line_of(&text, offset),
                token,
                surface,
            });
            search_from = offset + token.len();
        }
    }
    findings.sort_by(|left, right| (left.line, left.token).cmp(&(right.line, right.token)));
    findings
}

/// Workspace root: `CARGO_MANIFEST_DIR` is `<root>/crates/godot-mcp-server`.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root above crates/godot-mcp-server")
        .to_path_buf()
}

fn read(root: &Path, relative: &str) -> String {
    let path = root.join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

fn collect_rust_sources(directory: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_rust_sources(&path, into);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            into.push(path);
        }
    }
}

/// Every `.rs` file under `crates/*/src`, in a deterministic order.
fn production_files(root: &Path) -> Vec<(String, PathBuf)> {
    let crates = root.join("crates");
    let entries = std::fs::read_dir(&crates).expect("crates/ is readable");
    let mut members: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    members.sort();
    let mut sources = Vec::new();
    for member in members {
        let source_root = member.join("src");
        if source_root.is_dir() {
            collect_rust_sources(&source_root, &mut sources);
        }
    }
    assert!(
        sources.len() > 100,
        "the scan found only {} source files, so it is not looking at the workspace",
        sources.len()
    );
    sources
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(root)
                .expect("a scanned path lives under the workspace root")
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            (relative, path)
        })
        .collect()
}

/// The failure text a contributor acts on. It names the location, the assumption
/// it breaks, and the only acceptable response.
fn invalidation_report(findings: &[Finding]) -> String {
    let mut report = String::new();
    writeln!(
        report,
        "ADR 0007 IS INVALIDATED. Production source now holds a private key, or performs a \
         signing or decryption operation, in {} place(s):\n",
        findings.len()
    )
    .expect("writing to a String cannot fail");
    for finding in findings {
        writeln!(report, "  {}:{}", finding.file, finding.line)
            .expect("writing to a String cannot fail");
        writeln!(report, "    matched: {}", finding.token)
            .expect("writing to a String cannot fail");
        writeln!(report, "    surface: {}", finding.surface)
            .expect("writing to a String cannot fail");
    }
    writeln!(
        report,
        "\n{ADR_PATH} established the public-key-only resource-server invariant while assessing \
         RUSTSEC-2023-0071 (the Marvin Attack), and {SUPERSEDING_ADR_PATH} retained that invariant \
         after removing rsa 0.9.10: the shipped server only verifies signatures with public keys, \
         owns no private key, and performs no signing or decryption. Each match above expands that \
         security boundary, so the ADR 0007 \"Invalidation condition\" and ADR 0009 review trigger \
         have fired."
    )
    .expect("writing to a String cannot fail");
    writeln!(
        report,
        "\nREQUIRED ACTION — do not relax this guard, and do not add the new call site to its \
         allowlist to make it green. Per the ADR's \"Invalidation condition\": stop the change \
         before release, re-run `cargo deny check advisories` and the release binary-content \
         checks, and obtain a fresh security assessment of RUSTSEC-2023-0071 against the new \
         private-key path. The ADR may not be cited as an exception once its public-key-only \
         assumption has failed; amend or supersede it first, and only then revisit \
         {GUARD_PATH}."
    )
    .expect("writing to a String cannot fail");
    report
}

/// Collapse every whitespace run to one space, so an attribute and the item it
/// gates match regardless of line breaks or a blanked comment between them.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse `[features]` into name -> members, tolerating multi-line arrays.
fn feature_table(manifest: &str) -> BTreeMap<String, Vec<String>> {
    let mut table = BTreeMap::new();
    let mut inside = false;
    let mut pending: Option<(String, String)> = None;
    for raw in manifest.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if pending.is_none() && line.starts_with('[') && line.ends_with(']') {
            inside = line == "[features]";
            continue;
        }
        if !inside {
            continue;
        }
        match pending.as_mut() {
            Some((_, value)) => {
                value.push(' ');
                value.push_str(line);
            }
            None => match line.split_once('=') {
                Some((name, value)) => {
                    pending = Some((
                        name.trim().trim_matches('"').to_owned(),
                        value.trim().to_owned(),
                    ));
                }
                None => continue,
            },
        }
        let balanced = pending
            .as_ref()
            .is_some_and(|(_, value)| value.matches('[').count() == value.matches(']').count());
        if balanced && let Some((name, value)) = pending.take() {
            let members = value
                .trim_matches(|character| character == '[' || character == ']')
                .split(',')
                .map(|member| member.trim().trim_matches('"').to_owned())
                .filter(|member| !member.is_empty())
                .collect();
            table.insert(name, members);
        }
    }
    table
}

/// Every feature name reachable from `default`, including the cross-crate
/// `crate/feature` entries.
fn default_feature_closure(table: &BTreeMap<String, Vec<String>>) -> BTreeSet<String> {
    let mut closure = BTreeSet::new();
    let mut frontier = vec!["default".to_owned()];
    while let Some(name) = frontier.pop() {
        if !closure.insert(name.clone()) {
            continue;
        }
        if let Some(members) = table.get(&name) {
            frontier.extend(members.iter().cloned());
        }
    }
    closure
}

// ---------------------------------------------------------------------------
// The two properties the ADR's invalidation condition demands.
// ---------------------------------------------------------------------------

/// Property 1: no production source holds an RSA private key, and none signs or
/// decrypts.
#[test]
fn production_source_holds_no_private_key_and_signs_or_decrypts_nothing() {
    let root = workspace_root();
    let files = production_files(&root);
    assert!(
        files.iter().any(|(relative, _)| relative == KEY_FIXTURE),
        "{KEY_FIXTURE} is not in the scanned set, so this guard's allowlist is dead and its \
         cfg-gate assertion no longer protects anything"
    );
    let mut findings = Vec::new();
    for (relative, path) in files {
        if relative == KEY_FIXTURE {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        findings.extend(scan(&relative, &source));
    }
    assert!(findings.is_empty(), "{}", invalidation_report(&findings));
}

/// Property 2: the one file that does sign stays test-only.
///
/// Both ADRs retain "enabling `testkeys`/`test-hooks` in a release" as a security
/// boundary expansion, so removing this gate must fail even though the file itself
/// is allowlisted from the scan above.
#[test]
fn the_test_key_fixture_stays_behind_its_cfg_gate() {
    let root = workspace_root();
    assert!(
        root.join(KEY_FIXTURE).is_file(),
        "{KEY_FIXTURE} is gone; if the fixture moved, move this guard's allowlist with it"
    );
    let parent = collapse_whitespace(&production_source(&read(&root, FIXTURE_PARENT)));
    let gated = format!("{FIXTURE_GATE} pub mod testkeys;");
    assert!(
        parent.contains(&gated),
        "{FIXTURE_PARENT} must declare the signing fixture as `{gated}`.\n\nWithout that gate the \
         module — and the four committed private keys it compiles in — enters a default-feature \
         release build. {ADR_PATH} and {SUPERSEDING_ADR_PATH} retain the shipped binary's \
         public-key-only boundary. Restore the gate, or stop the change and obtain a fresh \
         security assessment."
    );
    assert_eq!(
        parent.matches("mod testkeys").count(),
        1,
        "{FIXTURE_PARENT} must declare the signing fixture exactly once; a second declaration \
         could be un-gated"
    );

    // No production module anywhere may reach the fixture.
    let mut reachable = Vec::new();
    for (relative, path) in production_files(&root) {
        if relative == KEY_FIXTURE || relative == FIXTURE_PARENT {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        let text = production_source(&source);
        let mut search_from = 0;
        while let Some(offset) = text[search_from..].find("testkeys") {
            let absolute = search_from + offset;
            reachable.push(format!("{relative}:{}", line_of(&text, absolute)));
            search_from = absolute + "testkeys".len();
        }
    }
    assert!(
        reachable.is_empty(),
        "production source reaches the private-key fixture at {reachable:?}.\n\n{ADR_PATH} rests \
         on nothing in production reading this module. Remove the reference, or stop the change \
         and obtain a fresh security assessment of RUSTSEC-2023-0071."
    );
}

/// A release must not be able to switch the signing fixture on: no default-feature
/// closure may reach `test-hooks`.
#[test]
fn no_default_feature_closure_enables_test_hooks() {
    let root = workspace_root();
    for manifest in RELEASE_MANIFESTS {
        let table = feature_table(&read(&root, manifest));
        assert!(
            table.contains_key("default"),
            "{manifest} declares no `default` feature, so this check reads nothing"
        );
        assert!(
            table.contains_key("test-hooks"),
            "{manifest} no longer declares `test-hooks`; if the gate was renamed, rename it here \
             too rather than deleting the check"
        );
        let closure = default_feature_closure(&table);
        let leaks: Vec<&String> = closure
            .iter()
            .filter(|feature| feature.contains("test-hooks"))
            .collect();
        assert!(
            leaks.is_empty(),
            "{manifest}: the default-feature closure reaches {leaks:?}.\n\nThat compiles the \
             gated signing fixture and its four committed private keys into a default release \
             build, which {ADR_PATH} and {SUPERSEDING_ADR_PATH} prohibit. Keep `test-hooks` out \
             of `default`, or stop the change and obtain a fresh security assessment."
        );
    }
}

/// The parsed JWK shape admits public members only, so private JWK material cannot
/// be ingested by the verification path.
#[test]
fn the_parsed_jwk_shape_carries_no_private_member() {
    let root = workspace_root();
    let text = production_source(&read(&root, JWKS_MODULE));
    let body = text
        .split_once("pub struct Jwk {")
        .expect("the JWK shape is declared")
        .1;
    let body = body.split_once('}').expect("the JWK shape closes").0;
    let members: BTreeSet<String> = body
        .lines()
        .filter_map(|line| line.trim().split_once(':'))
        .map(|(name, _)| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect();
    let expected: BTreeSet<String> = PUBLIC_JWK_MEMBERS
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    for private in PRIVATE_JWK_MEMBERS {
        assert!(
            !members.contains(*private),
            "{JWKS_MODULE} declares the private JWK member `{private}`.\n\n{ADR_PATH} names \
             \"accepting private PEM/DER/JWK material\" as a security-boundary expansion retained \
             by {SUPERSEDING_ADR_PATH}: a private JWK member means the server ingests private key \
             material. Remove the member, or stop the change and obtain a fresh security assessment."
        );
    }
    assert_eq!(
        members, expected,
        "{JWKS_MODULE}'s JWK shape changed. Every member must be a public JWK member; extend \
         PUBLIC_JWK_MEMBERS in {GUARD_PATH} only after confirming the new member carries no \
         private key material, per {ADR_PATH}."
    );
}

// ---------------------------------------------------------------------------
// The guard's own non-vacuity, asserted in the suite rather than assumed.
// ---------------------------------------------------------------------------

/// The scanner really does see the one module that signs.
///
/// Without this, a broken scanner (one that blanked everything, or lost its token
/// table) would leave the guard above passing forever while proving nothing.
#[test]
fn the_scanner_flags_the_known_signing_module() {
    let root = workspace_root();
    let findings = scan(KEY_FIXTURE, &read(&root, KEY_FIXTURE));
    let matched: BTreeSet<&str> = findings.iter().map(|finding| finding.token).collect();
    for expected in [
        "EncodingKey",
        "jsonwebtoken::encode",
        "crypto::sign",
        "private_pem",
    ] {
        assert!(
            matched.contains(expected),
            "the scanner missed `{expected}` in {KEY_FIXTURE}, so it cannot be trusted to find \
             the same surface in production source. Matched: {matched:?}"
        );
    }
    assert!(
        findings
            .iter()
            .any(|finding| finding.token == "EncodingKey" && finding.line == 90),
        "the scanner must report the exact line of a match; expected EncodingKey at \
         {KEY_FIXTURE}:90, got {findings:?}"
    );
}

/// The scanner reads code and ignores prose.
///
/// Comments that *name* a banned API — including this repository's own ADR text and
/// this guard's own failure messages — must not trip it, while the same names in
/// code must.
#[test]
fn the_scanner_separates_code_from_prose() {
    const PROSE: &str = "\
//! The shipped path must never construct an EncodingKey or an RsaPrivateKey.
/// See `from_pkcs8_pem`, `.decrypt(` and `SigningKey` for what is refused.
/* A nested /* block */ naming crypto::sign, private_pem and PRIVATE KEY. */
fn verify(token: &str) -> bool {
    let quote = '\"'; // EncodingKey named after a char literal holding a quote
    let raw = r#\"{}\"#;
    !token.is_empty() && quote == '\"' && !raw.is_empty()
}
#[cfg(test)]
mod tests {
    fn helper() {
        let _ = jsonwebtoken::EncodingKey::from_rsa_pem(b\"\");
    }
}
";
    let prose = scan("prose.rs", PROSE);
    assert!(
        prose.is_empty(),
        "prose, a char literal holding a quote, a raw string and a #[cfg(test)] item must not be \
         read as production code, got {prose:?}"
    );

    const CODE: &str = "\
fn issue(pem: &[u8]) -> String {
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(pem).expect(\"loads\");
    jsonwebtoken::encode(&Default::default(), &(), &key).expect(\"signs\")
}
fn ingest(pem: &str) -> bool {
    pem.contains(\"PRIVATE KEY\")
}
";
    let code = scan("code.rs", CODE);
    let located: BTreeSet<(&str, usize)> = code
        .iter()
        .map(|finding| (finding.token, finding.line))
        .collect();
    assert!(
        located.contains(&("EncodingKey", 2)),
        "a real EncodingKey construction must be found on its own line, got {located:?}"
    );
    assert!(
        located.contains(&("jsonwebtoken::encode", 3)),
        "a real token issuance must be found on its own line, got {located:?}"
    );
    assert!(
        located.contains(&("PRIVATE KEY", 6)),
        "a PEM private-key label inside a string literal must stay visible, got {located:?}"
    );
}

/// The failure text tells a reader what broke and what to do, rather than only
/// that an assertion failed.
#[test]
fn the_failure_report_is_actionable() {
    let report = invalidation_report(&[Finding {
        file: "crates/godot-mcp-server/src/auth/pipeline.rs".to_owned(),
        line: 412,
        token: "EncodingKey",
        surface: BANNED[0].1,
    }]);
    for required in [
        "ADR 0007 IS INVALIDATED",
        "crates/godot-mcp-server/src/auth/pipeline.rs:412",
        "RUSTSEC-2023-0071",
        "Invalidation condition",
        "REQUIRED ACTION",
        "do not relax this guard",
        "fresh security assessment",
        ADR_PATH,
        GUARD_PATH,
    ] {
        assert!(
            report.contains(required),
            "the failure report must mention {required:?}:\n{report}"
        );
    }
}

/// The ADR points back at this guard, so a reader learns the invalidation condition
/// is enforced rather than merely promised.
#[test]
fn the_adr_names_this_guard() {
    let root = workspace_root();
    let adr = read(&root, ADR_PATH);
    let superseding_adr = read(&root, SUPERSEDING_ADR_PATH);
    assert!(
        adr.contains("## Invalidation condition"),
        "{ADR_PATH} no longer states an invalidation condition; this guard enforces that section"
    );
    assert!(
        adr.contains(GUARD_PATH),
        "{ADR_PATH} must name {GUARD_PATH} so a reader can see the invalidation condition is \
         mechanically enforced. A prose-only invariant is not an invariant."
    );
    assert!(
        superseding_adr.contains(GUARD_PATH),
        "{SUPERSEDING_ADR_PATH} must name {GUARD_PATH} so superseding the advisory decision cannot \
         silently discard the public-key-only resource-server invariant"
    );
}
