//! The protected editor settings the operator page lists are the domain's
//! lists, setting for setting, so the page cannot promise a change the server
//! refuses or hide one it refuses.

use std::collections::BTreeSet;

use godot_mcp_domain::editor_preferences::{
    FIXED_PREFIXES, FIXED_SETTINGS, MAX_CHANGES, MAX_PAGE, WITHHELD_PREFIXES,
};

fn page() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/operator/editor-bridge.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// The backticked names of each row of the `| Settings | Read | Why |` table,
/// with the row's Read column, in page order.
fn documented() -> Vec<(Vec<String>, String)> {
    let page = page();
    let table: Vec<&str> = page
        .lines()
        .skip_while(|line| *line != "| Settings | Read | Why |")
        .skip(2)
        .take_while(|line| line.starts_with('|'))
        .collect();
    assert!(
        !table.is_empty(),
        "the page carries the protected-settings table"
    );
    table
        .iter()
        .map(|row| {
            let cells: Vec<&str> = row.trim_matches('|').split('|').map(str::trim).collect();
            assert_eq!(
                cells.len(),
                3,
                "a row names settings, a read and why: {row}"
            );
            let names = cells[0]
                .split('`')
                .skip(1)
                .step_by(2)
                .map(str::to_owned)
                .collect();
            (names, cells[1].to_owned())
        })
        .collect()
}

fn strings(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn the_operator_page_documents_exactly_the_protected_settings() {
    assert_eq!(
        documented(),
        vec![
            (strings(WITHHELD_PREFIXES), "withheld".to_owned()),
            (strings(FIXED_PREFIXES), "yes".to_owned()),
            (strings(FIXED_SETTINGS), "yes".to_owned()),
        ]
    );
}

#[test]
fn the_operator_page_names_the_hint_rule_and_the_limits() {
    let page = page();
    let section = page
        .split("\n## ")
        .find(|section| section.starts_with("The editor's settings"))
        .expect("the page explains the editor's settings");
    assert!(
        section.contains("hint is a file, a directory or a password"),
        "the section names the hints the add-on refuses: {section}"
    );
    for limit in [
        format!("at most {MAX_PAGE}"),
        format!("up to {MAX_CHANGES} settings"),
    ] {
        assert!(section.contains(&limit), "the section says {limit}");
    }
    let listed: BTreeSet<&str> = WITHHELD_PREFIXES
        .iter()
        .chain(FIXED_PREFIXES)
        .chain(FIXED_SETTINGS)
        .copied()
        .collect();
    assert_eq!(
        listed.len(),
        WITHHELD_PREFIXES.len() + FIXED_PREFIXES.len() + FIXED_SETTINGS.len(),
        "no setting is listed twice"
    );
}
