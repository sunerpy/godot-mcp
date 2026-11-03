//! The editing-methods table the operator page documents is the domain's
//! table, class for class and method for method, so the page cannot promise a
//! call the server refuses or hide one it accepts.

use godot_mcp_domain::editing_methods::{
    EDITING_METHODS, MAX_CALL_ARGUMENTS, NEW_RESOURCE_CLASSES,
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

#[test]
fn the_operator_page_names_every_new_resource_class_and_the_argument_limit() {
    let page = page();
    let paragraph = page
        .split("\n\n")
        .find(|paragraph| paragraph.starts_with("**Arguments**"))
        .expect("the page explains a call's arguments");
    for class in NEW_RESOURCE_CLASSES {
        assert!(
            paragraph.contains(class),
            "the arguments paragraph names {class}: {paragraph}"
        );
    }
    assert!(
        paragraph.contains(&format!("at most {MAX_CALL_ARGUMENTS}")),
        "{paragraph}"
    );
}
