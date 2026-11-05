//! The reading-methods table the operator page documents is the domain's
//! table, class for class and method for method, so the page cannot promise a
//! read the server refuses or hide one it makes.

use godot_mcp_domain::reading_methods::{MAX_READ_CALLS, MAX_READ_VALUE_BYTES, READING_METHODS};

fn page() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/operator/editor-bridge.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// The rows of the `| Class | Reading methods |` table, in page order.
fn documented() -> Vec<(String, Vec<String>)> {
    let page = page();
    let table: Vec<&str> = page
        .lines()
        .skip_while(|line| *line != "| Class | Reading methods |")
        .skip(2)
        .take_while(|line| line.starts_with('|'))
        .collect();
    assert!(
        !table.is_empty(),
        "the page carries the reading-methods table"
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
fn the_operator_page_documents_exactly_the_reading_methods() {
    let expected: Vec<(String, Vec<String>)> = READING_METHODS
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
fn the_operator_page_states_the_read_limits() {
    let page = page();
    let section = page
        .split("## Reading calls")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .expect("the page has a reading calls section");
    // Lines wrap anywhere, so words are compared with single spaces.
    let section = section.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        section.contains(&format!("at most {MAX_READ_CALLS} calls")),
        "{section}"
    );
    let bytes = MAX_READ_VALUE_BYTES.to_string();
    let grouped = format!(
        "{},{}",
        &bytes[..bytes.len() - 3],
        &bytes[bytes.len() - 3..]
    );
    assert!(
        section.contains(&format!("more than {grouped} bytes")),
        "the page states the value limit, {grouped} bytes: {section}"
    );
}
