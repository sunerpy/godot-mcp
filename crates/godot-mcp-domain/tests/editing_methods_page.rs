//! The editing-methods table the operator page documents is the domain's
//! table, class for class and method for method, so the page cannot promise a
//! call the server refuses or hide one it accepts.

use godot_mcp_domain::editing_methods::{
    EDITING_METHODS, MAX_CALL_ARGUMENTS, MAX_REPEATED_CALLS, NEW_RESOURCE_FORBIDDEN, Restore,
};

fn page() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/operator/editor-bridge.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// The rows of the `| Class | Methods |` table, in page order.
fn documented() -> Vec<(String, Vec<String>)> {
    let page = page();
    let table: Vec<&str> = page
        .lines()
        .skip_while(|line| *line != "| Class | Methods |")
        .skip(2)
        .take_while(|line| line.starts_with('|'))
        .collect();
    assert!(
        !table.is_empty(),
        "the page carries the editing-methods table"
    );
    table
        .iter()
        .map(|row| {
            let cells: Vec<&str> = row.trim_matches('|').split('|').map(str::trim).collect();
            assert_eq!(cells.len(), 2, "a row names a class and its methods: {row}");
            (
                cells[0].to_owned(),
                cells[1]
                    .split(',')
                    .map(|method| method.trim().to_owned())
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn the_operator_page_documents_exactly_the_editing_methods() {
    let expected: Vec<(String, Vec<String>)> = EDITING_METHODS
        .iter()
        .map(|class| {
            (
                class.class.to_owned(),
                class
                    .methods
                    .iter()
                    .map(|method| (*method).to_owned())
                    .collect(),
            )
        })
        .collect();
    assert_eq!(documented(), expected);
}

/// The paragraph of the page that starts with `opening`.
fn paragraph(opening: &str) -> String {
    page()
        .split("\n\n")
        .find(|paragraph| paragraph.starts_with(opening))
        .unwrap_or_else(|| panic!("the page has a paragraph starting {opening}"))
        .to_owned()
}

#[test]
fn the_operator_page_names_the_forbidden_new_resource_classes_and_the_limits() {
    let arguments = paragraph("**Arguments**");
    for class in NEW_RESOURCE_FORBIDDEN {
        assert!(
            arguments.contains(class),
            "the arguments paragraph names {class}: {arguments}"
        );
    }
    assert!(
        arguments.contains(&format!("at most {MAX_CALL_ARGUMENTS}")),
        "{arguments}"
    );
    let repeat = paragraph("**Repeat.**");
    assert!(
        repeat.contains(&format!("at most {MAX_REPEATED_CALLS} calls")),
        "{repeat}"
    );
}

/// Whether `text` names `class` as a whole word, not as part of a longer name.
fn names(text: &str, class: &str) -> bool {
    let word = |character: char| character.is_ascii_alphanumeric() || character == '_';
    text.match_indices(class).any(|(start, _)| {
        !text[..start].chars().next_back().is_some_and(word)
            && !text[start + class.len()..].chars().next().is_some_and(word)
    })
}

#[test]
fn the_operator_page_names_every_replacing_class() {
    let page = page();
    let replacing = page
        .split("\n\n")
        .find(|paragraph| paragraph.starts_with("The graph resources"))
        .expect("the page explains how graph resources are undone");
    for class in EDITING_METHODS {
        let named = names(replacing, class.class)
            || (class.class.starts_with("AnimationNodeBlendSpace")
                && replacing.contains("the two blend spaces"));
        assert_eq!(
            named,
            class.restore == Restore::Replace,
            "the replacing paragraph names {} exactly when the table replaces it: {replacing}",
            class.class
        );
    }
}

#[test]
fn the_operator_page_names_every_baking_class() {
    let baking = paragraph("**Baking.**");
    for class in EDITING_METHODS {
        let bakes = matches!(class.restore, Restore::Bake(_));
        assert_eq!(
            names(&baking, class.class),
            bakes,
            "the baking paragraph names {} exactly when the table bakes with it: {baking}",
            class.class
        );
        if bakes {
            for method in class.methods {
                assert!(
                    baking.contains(&format!("`{method}`")),
                    "{method}: {baking}"
                );
            }
        }
    }
}
