//! The operator page on project commands shows a declaration the server
//! accepts, a provider that answers every command it declares, and the
//! limits the domain enforces, so the page cannot teach a file the server
//! refuses or a bound it does not hold.

use godot_mcp_domain::project_commands::{
    DECLARATION_FILE, Declaration, MAX_ARGUMENT_DEPTH, MAX_ARGUMENT_ITEMS,
    MAX_ARGUMENT_STRING_CHARS, MAX_ARGUMENTS_BYTES, MAX_COMMANDS, MAX_DECLARATION_BYTES,
    MAX_DESCRIPTION_CHARS, MAX_INPUT_SCHEMA_BYTES, MAX_NAME_CHARS, MAX_PROVIDERS,
    MAX_SCRIPT_PATH_CHARS, MAX_TITLE_CHARS,
};

fn page() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/operator/project-commands.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// The body of the first fenced block whose info string is `info`.
fn first_block(page: &str, info: &str) -> String {
    let opening = format!("```{info}");
    let body: Vec<&str> = page
        .lines()
        .skip_while(|line| line.trim() != opening)
        .skip(1)
        .take_while(|line| line.trim() != "```")
        .collect();
    assert!(!body.is_empty(), "the page carries a ```{info} block");
    body.join("\n")
}

/// `value` written with a comma between each group of three digits, as the
/// pages write numbers of four digits or more.
fn grouped(value: usize) -> String {
    let digits = value.to_string();
    if digits.len() < 4 {
        return digits;
    }
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[test]
fn the_example_declaration_is_one_the_server_accepts_whole() {
    let page = page();
    assert!(
        page.contains(&format!("`{DECLARATION_FILE}`")),
        "the page names the file"
    );
    let declaration = Declaration::parse(first_block(&page, "json").as_bytes());
    assert!(
        declaration.is_valid(),
        "the example breaks no rule: {:?}",
        declaration.problems
    );
    assert!(
        declaration.command_count() > 0,
        "the example declares commands"
    );
    for provider in &declaration.providers {
        for command in &provider.commands {
            assert_eq!(
                command.unavailable, None,
                "{}/{} can be called",
                provider.id, command.name
            );
        }
    }
}

#[test]
fn the_example_provider_answers_every_command_the_example_declares() {
    let page = page();
    let declaration = Declaration::parse(first_block(&page, "json").as_bytes());
    let provider = first_block(&page, "gdscript");
    assert_eq!(
        provider.lines().find(|line| !line.trim().is_empty()),
        Some("@tool"),
        "the provider is a tool script"
    );
    assert!(
        provider.contains("func mcp_call(command: String, args: Dictionary)"),
        "the provider defines mcp_call(command, args)"
    );
    for command in declaration
        .providers
        .iter()
        .flat_map(|provider| &provider.commands)
    {
        assert!(
            provider.contains(&format!("\"{}\":", command.name)),
            "the provider answers `{}`",
            command.name
        );
    }
    for script in declaration
        .providers
        .iter()
        .map(|provider| &provider.script)
    {
        assert!(
            page.contains(&format!("`{script}`")),
            "the page names the provider's script {script}"
        );
    }
}

#[test]
fn the_page_states_the_limits_the_domain_enforces() {
    let page = page();
    for stated in [
        format!("at most {} bytes", grouped(MAX_DECLARATION_BYTES)),
        format!("at most {MAX_PROVIDERS} providers"),
        format!("at most {MAX_COMMANDS} commands"),
        format!("1 to {MAX_NAME_CHARS} characters"),
        format!("1 to {MAX_TITLE_CHARS} characters"),
        format!("1 to {} characters", grouped(MAX_DESCRIPTION_CHARS)),
        format!(
            "at most {} bytes as compact JSON",
            grouped(MAX_INPUT_SCHEMA_BYTES)
        ),
        format!("at most {} characters", grouped(MAX_SCRIPT_PATH_CHARS)),
        format!("{} bytes as compact JSON", grouped(MAX_ARGUMENTS_BYTES)),
        format!("{MAX_ARGUMENT_DEPTH} levels"),
        format!("{} items", grouped(MAX_ARGUMENT_ITEMS)),
        format!("{} characters", grouped(MAX_ARGUMENT_STRING_CHARS)),
    ] {
        assert!(page.contains(&stated), "the page states `{stated}`");
    }
}
