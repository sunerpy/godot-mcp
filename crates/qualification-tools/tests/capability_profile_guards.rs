#[path = "support/capability_profile.rs"]
mod capability_profile;
#[allow(dead_code)]
mod support;

use capability_profile::{
    GuardHarness, SAFE_DESCRIPTIONS, VARIANTS, finding_summaries, line_of, set_descriptions,
};

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn guards_reject_unhedged_claims_on_both_surfaces_and_inspect_every_variant() {
    let harness = GuardHarness::build();
    for (index, (rust_name, schema_name)) in VARIANTS.iter().enumerate() {
        let mirror = harness.mirror(&format!("unhedged-{schema_name}"));
        let mut descriptions = SAFE_DESCRIPTIONS;
        descriptions[index] = "Screenshot capture is delivered.";
        set_descriptions(&mirror, descriptions);

        let rust_line = line_of(
            &mirror.join("crates/godot-mcp-domain/src/lib.rs"),
            "Screenshot capture is delivered.",
        );
        let schema_line = line_of(
            &mirror.join("schemas/domain/contract-v1.schema.json"),
            "Screenshot capture is delivered.",
        );
        let (f2, f2_receipt) = harness.run_f2(&mirror, schema_name);
        assert!(f2.failed(), "F2 must reject CapabilityProfile::{rust_name}");
        let findings = finding_summaries(&f2_receipt);
        for expected in [
            format!("lib.rs:{rust_line} CapabilityProfile::{rust_name}"),
            format!("contract-v1.schema.json:{schema_line} CapabilityProfile::{schema_name}"),
            "claim `screenshot`".to_owned(),
        ] {
            assert!(
                findings.contains(&expected),
                "missing {expected:?} in {findings}"
            );
        }

        let f4 = harness.run_f4(&mirror, schema_name);
        assert!(
            f4.failed(),
            "F4 must reject CapabilityProfile::{schema_name}"
        );
        for expected in [
            format!("contract-v1.schema.json:{schema_line}"),
            format!("CapabilityProfile::{schema_name}"),
            "claim `screenshot`".to_owned(),
        ] {
            assert!(
                f4.stdout.contains(&expected),
                "missing {expected:?} in {}",
                f4.stdout
            );
        }
    }
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn guards_accept_the_current_real_profile_wording() {
    let harness = GuardHarness::build();
    let mirror = harness.mirror("current-wording");
    let (f2, _) = harness.run_f2(&mirror, "current");
    let f4 = harness.run_f4(&mirror, "current");
    assert!(
        f2.succeeded(),
        "current wording must pass F2: {}",
        f2.stdout
    );
    assert!(
        f4.succeeded(),
        "current wording must pass F4: {}",
        f4.stdout
    );
    println!(
        "CURRENT_F2_STDOUT:\n{}\nCURRENT_F4_STDOUT:\n{}",
        f2.stdout, f4.stdout
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn guards_reject_a_mixed_description_with_an_unhedged_sentence() {
    let harness = GuardHarness::build();
    let mirror = harness.mirror("mixed-hedge");
    let mut descriptions = SAFE_DESCRIPTIONS;
    descriptions[1] = "Undoable scene/resource mutations. Screenshot capture is unavailable.";
    set_descriptions(&mirror, descriptions);

    let (f2, f2_receipt) = harness.run_f2(&mirror, "mixed");
    assert!(f2.failed(), "the second sentence must not hedge the first");
    let findings = finding_summaries(&f2_receipt);
    assert!(
        findings.contains("claim `undoable scene/resource mutation`"),
        "{findings}"
    );
    assert!(!findings.contains("claim `screenshot`"), "{findings}");

    let f4 = harness.run_f4(&mirror, "mixed");
    assert!(f4.failed(), "the second sentence must not hedge the first");
    assert!(
        f4.stdout
            .contains("claim `undoable scene/resource mutation`"),
        "{}",
        f4.stdout
    );
    assert!(!f4.stdout.contains("claim `screenshot`"), "{}", f4.stdout);
    println!(
        "MIXED_F2_FINDINGS:\n{findings}\nMIXED_F4_STDOUT:\n{}",
        f4.stdout
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn guards_reject_the_original_unhedged_profile_wording() {
    let harness = GuardHarness::build();
    let mirror = harness.mirror("original-wording");
    set_descriptions(
        &mirror,
        [
            "Adds static analysis tools.",
            "Undoable scene/resource mutations.",
            "Run / debug / screenshot / input.",
            "Combines every profile.",
        ],
    );

    let (f2, f2_receipt) = harness.run_f2(&mirror, "original");
    assert!(f2.failed(), "F2 must reject the original unhedged wording");
    let findings = finding_summaries(&f2_receipt);
    for expected in [
        "claim `undoable scene/resource mutation`",
        "claim `run / debug`",
        "claim `screenshot`",
        "claim `screenshot / input`",
    ] {
        assert!(
            findings.contains(expected),
            "missing {expected:?} in {findings}"
        );
    }

    let f4 = harness.run_f4(&mirror, "original");
    assert!(f4.failed(), "F4 must reject the original unhedged wording");
    println!(
        "ORIGINAL_F2_FINDINGS:\n{findings}\nORIGINAL_F4_STDOUT:\n{}",
        f4.stdout
    );
}

#[test]
#[cfg_attr(
    not(maintainer_workspace),
    ignore = "needs the maintainer's .omo workspace"
)]
fn guards_accept_ordinary_english_near_misses() {
    let harness = GuardHarness::build();
    let mirror = harness.mirror("ordinary-english");
    set_descriptions(
        &mirror,
        [
            "Adds structured input validation for every request.",
            "Recording each payload file for the durable journal.",
            "Debug-level tracing of the scheduler.",
            "Combines every profile.",
        ],
    );

    let (f2, _) = harness.run_f2(&mirror, "ordinary");
    let f4 = harness.run_f4(&mirror, "ordinary");
    assert!(
        f2.succeeded(),
        "ordinary English must pass F2: {}",
        f2.stdout
    );
    assert!(
        f4.succeeded(),
        "ordinary English must pass F4: {}",
        f4.stdout
    );
    println!(
        "ORDINARY_F2_STDOUT:\n{}\nORDINARY_F4_STDOUT:\n{}",
        f2.stdout, f4.stdout
    );
}
