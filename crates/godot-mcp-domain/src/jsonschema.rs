//! A deterministic, fail-closed validator for the restrictive JSON Schema
//! subset used by this workspace's canonical schemas.
//!
//! The workspace deliberately does not depend on a general JSON Schema engine:
//! every canonical schema under `schemas/` is authored inside one small,
//! explicitly enumerated keyword subset so that validation is deterministic,
//! dependency-free and auditable. An unknown keyword, an unsupported pattern or
//! an unsupported construct is a **validator error**, never a silent pass, so a
//! schema can never accidentally become permissive.
//!
//! Supported keywords: `type`, `enum`, `const`, `properties`, `required`,
//! `additionalProperties` (boolean or schema), `items`, `minItems`, `maxItems`,
//! `minLength`, `maxLength`, `minimum`, `pattern`, `oneOf`. The annotation
//! keywords `$schema`, `$id`, `title`, `description`, `examples` and `default`
//! are ignored.
//!
//! Supported `pattern` syntax: a fully anchored `^…$` expression built from
//! literals, escapes, character classes and the quantifiers `*`, `+`, `?`,
//! `{n}` and `{n,m}`, optionally wrapped in a single top-level alternation of
//! such sequences (`^(a|b)$` or `^a|b$`). Nested groups, back-references and the
//! `.` wildcard are rejected as unsupported rather than approximated.

use std::collections::BTreeSet;
use std::fmt;

use serde_json::Value;

/// A rejected instance, or a schema this subset cannot express.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaError {
    /// JSON pointer of the offending instance location.
    pub pointer: String,
    /// Stable, payload-free reason.
    pub reason: String,
}

impl SchemaError {
    fn new(pointer: &str, reason: impl Into<String>) -> Self {
        Self {
            pointer: if pointer.is_empty() {
                "/".to_owned()
            } else {
                pointer.to_owned()
            },
            reason: reason.into(),
        }
    }
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.pointer, self.reason)
    }
}

impl std::error::Error for SchemaError {}

const ANNOTATIONS: &[&str] = &[
    "$schema",
    "$id",
    "title",
    "description",
    "examples",
    "default",
    "$comment",
];

const SUPPORTED: &[&str] = &[
    "type",
    "enum",
    "const",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "minItems",
    "maxItems",
    "minLength",
    "maxLength",
    "minimum",
    "pattern",
    "oneOf",
];

/// Validate `instance` against `schema`.
///
/// # Errors
/// Returns the first [`SchemaError`] describing either a rejected instance or a
/// schema construct outside the supported subset.
pub fn validate(schema: &Value, instance: &Value) -> Result<(), SchemaError> {
    validate_at("", schema, instance)
}

fn validate_at(pointer: &str, schema: &Value, instance: &Value) -> Result<(), SchemaError> {
    let object = match schema {
        Value::Bool(true) => return Ok(()),
        Value::Bool(false) => return Err(SchemaError::new(pointer, "schema forbids any value")),
        Value::Object(object) => object,
        _ => {
            return Err(SchemaError::new(
                pointer,
                "schema node must be an object or boolean",
            ));
        }
    };

    for key in object.keys() {
        if !ANNOTATIONS.contains(&key.as_str()) && !SUPPORTED.contains(&key.as_str()) {
            return Err(SchemaError::new(
                pointer,
                format!("unsupported schema keyword `{key}`"),
            ));
        }
    }

    if let Some(expected) = object.get("type") {
        check_type(pointer, expected, instance)?;
    }
    if let Some(Value::Array(allowed)) = object.get("enum") {
        if !allowed.contains(instance) {
            return Err(SchemaError::new(pointer, "value is not an allowed enum"));
        }
    } else if object.contains_key("enum") {
        return Err(SchemaError::new(pointer, "`enum` must be an array"));
    }
    if let Some(expected) = object.get("const")
        && instance != expected
    {
        return Err(SchemaError::new(pointer, "value is not the required const"));
    }

    match instance {
        Value::String(text) => check_string(pointer, object, text)?,
        Value::Array(items) => check_array(pointer, object, items)?,
        Value::Object(_) => check_object(pointer, object, instance)?,
        Value::Number(number) => {
            if let Some(minimum) = object.get("minimum") {
                let minimum = minimum
                    .as_f64()
                    .ok_or_else(|| SchemaError::new(pointer, "`minimum` must be numeric"))?;
                let actual = number
                    .as_f64()
                    .ok_or_else(|| SchemaError::new(pointer, "number is not representable"))?;
                if actual < minimum {
                    return Err(SchemaError::new(pointer, "number is below the minimum"));
                }
            }
        }
        Value::Bool(_) | Value::Null => {}
    }

    if let Some(branches) = object.get("oneOf") {
        let Value::Array(branches) = branches else {
            return Err(SchemaError::new(pointer, "`oneOf` must be an array"));
        };
        let mut matched = 0_usize;
        for branch in branches {
            if validate_at(pointer, branch, instance).is_ok() {
                matched += 1;
            }
        }
        if matched != 1 {
            return Err(SchemaError::new(
                pointer,
                format!("value matched {matched} oneOf branches, expected exactly 1"),
            ));
        }
    }

    Ok(())
}

fn type_name(instance: &Value) -> &'static str {
    match instance {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) => {
            if number.is_i64() || number.is_u64() {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn type_matches(expected: &str, instance: &Value) -> bool {
    match expected {
        "number" => instance.is_number(),
        "integer" => matches!(type_name(instance), "integer"),
        other => other == type_name(instance),
    }
}

fn check_type(pointer: &str, expected: &Value, instance: &Value) -> Result<(), SchemaError> {
    let accepted: Vec<&str> = match expected {
        Value::String(single) => vec![single.as_str()],
        Value::Array(values) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| SchemaError::new(pointer, "`type` entries must be strings"))
            })
            .collect::<Result<_, _>>()?,
        _ => {
            return Err(SchemaError::new(
                pointer,
                "`type` must be a string or array",
            ));
        }
    };
    if accepted
        .iter()
        .any(|expected| type_matches(expected, instance))
    {
        Ok(())
    } else {
        Err(SchemaError::new(
            pointer,
            format!("expected type {accepted:?}, found {}", type_name(instance)),
        ))
    }
}

fn check_string(
    pointer: &str,
    object: &serde_json::Map<String, Value>,
    text: &str,
) -> Result<(), SchemaError> {
    if let Some(minimum) = object.get("minLength") {
        let minimum = minimum
            .as_u64()
            .ok_or_else(|| SchemaError::new(pointer, "`minLength` must be an integer"))?;
        if (text.chars().count() as u64) < minimum {
            return Err(SchemaError::new(
                pointer,
                "string is shorter than minLength",
            ));
        }
    }
    if let Some(maximum) = object.get("maxLength") {
        let maximum = maximum
            .as_u64()
            .ok_or_else(|| SchemaError::new(pointer, "`maxLength` must be an integer"))?;
        if (text.chars().count() as u64) > maximum {
            return Err(SchemaError::new(pointer, "string is longer than maxLength"));
        }
    }
    if let Some(pattern) = object.get("pattern") {
        let pattern = pattern
            .as_str()
            .ok_or_else(|| SchemaError::new(pointer, "`pattern` must be a string"))?;
        let matcher = Pattern::parse(pattern).map_err(|reason| {
            SchemaError::new(pointer, format!("unsupported pattern: {reason}"))
        })?;
        if !matcher.matches(text) {
            return Err(SchemaError::new(pointer, "string does not match pattern"));
        }
    }
    Ok(())
}

fn check_array(
    pointer: &str,
    object: &serde_json::Map<String, Value>,
    items: &[Value],
) -> Result<(), SchemaError> {
    if let Some(minimum) = object.get("minItems") {
        let minimum = minimum
            .as_u64()
            .ok_or_else(|| SchemaError::new(pointer, "`minItems` must be an integer"))?;
        if (items.len() as u64) < minimum {
            return Err(SchemaError::new(pointer, "array has fewer than minItems"));
        }
    }
    if let Some(maximum) = object.get("maxItems") {
        let maximum = maximum
            .as_u64()
            .ok_or_else(|| SchemaError::new(pointer, "`maxItems` must be an integer"))?;
        if (items.len() as u64) > maximum {
            return Err(SchemaError::new(pointer, "array has more than maxItems"));
        }
    }
    if let Some(item_schema) = object.get("items") {
        for (index, item) in items.iter().enumerate() {
            validate_at(&format!("{pointer}/{index}"), item_schema, item)?;
        }
    }
    Ok(())
}

fn check_object(
    pointer: &str,
    object: &serde_json::Map<String, Value>,
    instance: &Value,
) -> Result<(), SchemaError> {
    let members = instance
        .as_object()
        .expect("check_object is only called for object instances");

    if let Some(required) = object.get("required") {
        let Value::Array(required) = required else {
            return Err(SchemaError::new(pointer, "`required` must be an array"));
        };
        for name in required {
            let name = name
                .as_str()
                .ok_or_else(|| SchemaError::new(pointer, "`required` entries must be strings"))?;
            if !members.contains_key(name) {
                return Err(SchemaError::new(
                    pointer,
                    format!("missing required property `{name}`"),
                ));
            }
        }
    }

    let declared: BTreeSet<&str> = match object.get("properties") {
        None => BTreeSet::new(),
        Some(Value::Object(properties)) => properties.keys().map(String::as_str).collect(),
        Some(_) => return Err(SchemaError::new(pointer, "`properties` must be an object")),
    };

    if let Some(Value::Object(properties)) = object.get("properties") {
        for (name, child_schema) in properties {
            if let Some(child) = members.get(name) {
                validate_at(&format!("{pointer}/{name}"), child_schema, child)?;
            }
        }
    }

    match object.get("additionalProperties") {
        None | Some(Value::Bool(true)) => {}
        Some(Value::Bool(false)) => {
            for name in members.keys() {
                if !declared.contains(name.as_str()) {
                    return Err(SchemaError::new(
                        pointer,
                        format!("additional property `{name}` is not allowed"),
                    ));
                }
            }
        }
        Some(child_schema) => {
            for (name, child) in members {
                if !declared.contains(name.as_str()) {
                    validate_at(&format!("{pointer}/{name}"), child_schema, child)?;
                }
            }
        }
    }

    Ok(())
}

/// One anchored pattern element.
#[derive(Debug, Clone)]
enum Element {
    Literal(char),
    Class {
        ranges: Vec<(char, char)>,
        negated: bool,
    },
}

#[derive(Debug, Clone, Copy)]
enum Quantifier {
    One,
    Exactly(usize),
    Between(usize, usize),
    ZeroOrMore,
    OneOrMore,
    ZeroOrOne,
}

/// A fully anchored `^…$` pattern: one or more alternative sequences of literals
/// and character classes with simple quantifiers. Nested groups and
/// back-references are rejected.
#[derive(Debug, Clone)]
struct Pattern {
    alternatives: Vec<Vec<(Element, Quantifier)>>,
}

impl Pattern {
    fn parse(pattern: &str) -> Result<Self, String> {
        let mut body = pattern
            .strip_prefix('^')
            .and_then(|rest| rest.strip_suffix('$'))
            .ok_or_else(|| "pattern must be fully anchored with ^ and $".to_owned())?;
        // Exactly one optional outermost group is allowed, so `^(a|b)$` and
        // `^a|b$` are both expressible while a nested group stays unsupported.
        if body.starts_with('(') && body.ends_with(')') && is_single_group(body) {
            body = &body[1..body.len() - 1];
        }
        let alternatives = split_top_level(body)
            .into_iter()
            .map(Self::parse_sequence)
            .collect::<Result<Vec<_>, _>>()?;
        if alternatives.is_empty() {
            return Err("pattern has no alternative".to_owned());
        }
        Ok(Self { alternatives })
    }

    fn parse_sequence(body: &str) -> Result<Vec<(Element, Quantifier)>, String> {
        let chars: Vec<char> = body.chars().collect();
        let mut index = 0_usize;
        let mut parts: Vec<(Element, Quantifier)> = Vec::new();
        while index < chars.len() {
            let element = match chars[index] {
                '[' => {
                    let (element, next) = parse_class(&chars, index)?;
                    index = next;
                    element
                }
                '(' | ')' | '|' | '^' | '$' | '*' | '+' | '?' | '{' | '}' => {
                    return Err(format!("unsupported metacharacter `{}`", chars[index]));
                }
                '\\' => {
                    index += 1;
                    let escaped = *chars
                        .get(index)
                        .ok_or_else(|| "trailing escape".to_owned())?;
                    index += 1;
                    match escaped {
                        '.' | '\\' | '[' | ']' | '(' | ')' | '{' | '}' | '+' | '*' | '?' | '^'
                        | '$' | '|' | '-' | '/' => Element::Literal(escaped),
                        other => return Err(format!("unsupported escape `\\{other}`")),
                    }
                }
                '.' => return Err("unsupported wildcard `.`".to_owned()),
                literal => {
                    index += 1;
                    Element::Literal(literal)
                }
            };
            let (quantifier, next) = parse_quantifier(&chars, index)?;
            index = next;
            parts.push((element, quantifier));
        }
        Ok(parts)
    }

    fn matches(&self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        self.alternatives
            .iter()
            .any(|parts| match_parts(parts, &chars))
    }
}

/// Whether the leading `(` closes only at the final `)`, i.e. the whole body is
/// one group rather than two adjacent ones.
fn is_single_group(body: &str) -> bool {
    let mut depth = 0_usize;
    let mut escaped = false;
    let mut in_class = false;
    for (index, character) in body.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '(' if !in_class => depth += 1,
            ')' if !in_class => {
                depth -= 1;
                if depth == 0 && index + character.len_utf8() != body.len() {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// Split on `|` characters that are not escaped, inside a class, or inside a
/// group. A group that survives here is rejected later as unsupported.
fn split_top_level(body: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0_usize;
    let mut depth = 0_usize;
    let mut escaped = false;
    let mut in_class = false;
    for (index, character) in body.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '(' if !in_class => depth += 1,
            ')' if !in_class => depth = depth.saturating_sub(1),
            '|' if !in_class && depth == 0 => {
                parts.push(&body[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&body[start..]);
    parts
}

fn parse_class(chars: &[char], start: usize) -> Result<(Element, usize), String> {
    let mut index = start + 1;
    let negated = chars.get(index) == Some(&'^');
    if negated {
        index += 1;
    }
    let mut ranges: Vec<(char, char)> = Vec::new();
    loop {
        let current = *chars
            .get(index)
            .ok_or_else(|| "unterminated character class".to_owned())?;
        if current == ']' {
            index += 1;
            break;
        }
        let low = if current == '\\' {
            index += 1;
            *chars
                .get(index)
                .ok_or_else(|| "trailing escape in class".to_owned())?
        } else {
            current
        };
        index += 1;
        if chars.get(index) == Some(&'-') && chars.get(index + 1).is_some_and(|next| *next != ']') {
            let high = chars[index + 1];
            index += 2;
            ranges.push((low, high));
        } else {
            ranges.push((low, low));
        }
    }
    if ranges.is_empty() {
        return Err("empty character class".to_owned());
    }
    Ok((Element::Class { ranges, negated }, index))
}

fn parse_quantifier(chars: &[char], start: usize) -> Result<(Quantifier, usize), String> {
    match chars.get(start) {
        Some('*') => Ok((Quantifier::ZeroOrMore, start + 1)),
        Some('+') => Ok((Quantifier::OneOrMore, start + 1)),
        Some('?') => Ok((Quantifier::ZeroOrOne, start + 1)),
        Some('{') => {
            let close = chars[start..]
                .iter()
                .position(|character| *character == '}')
                .ok_or_else(|| "unterminated repetition".to_owned())?
                + start;
            let body: String = chars[start + 1..close].iter().collect();
            let quantifier = match body.split_once(',') {
                None => Quantifier::Exactly(
                    body.parse::<usize>()
                        .map_err(|_| "invalid repetition count".to_owned())?,
                ),
                Some((low, high)) => {
                    let low = low
                        .parse::<usize>()
                        .map_err(|_| "invalid repetition lower bound".to_owned())?;
                    let high = high
                        .parse::<usize>()
                        .map_err(|_| "unbounded repetition is unsupported".to_owned())?;
                    Quantifier::Between(low, high)
                }
            };
            Ok((quantifier, close + 1))
        }
        _ => Ok((Quantifier::One, start)),
    }
}

fn element_matches(element: &Element, value: char) -> bool {
    match element {
        Element::Literal(expected) => *expected == value,
        Element::Class { ranges, negated } => {
            let inside = ranges
                .iter()
                .any(|(low, high)| (*low..=*high).contains(&value));
            inside != *negated
        }
    }
}

fn match_parts(parts: &[(Element, Quantifier)], text: &[char]) -> bool {
    let Some(((element, quantifier), rest)) = parts.split_first() else {
        return text.is_empty();
    };
    let (minimum, maximum) = match *quantifier {
        Quantifier::One => (1, Some(1)),
        Quantifier::Exactly(count) => (count, Some(count)),
        Quantifier::Between(low, high) => (low, Some(high)),
        Quantifier::ZeroOrMore => (0, None),
        Quantifier::OneOrMore => (1, None),
        Quantifier::ZeroOrOne => (0, Some(1)),
    };
    let mut available = 0_usize;
    while available < text.len() && element_matches(element, text[available]) {
        available += 1;
        if maximum.is_some_and(|maximum| available >= maximum) {
            break;
        }
    }
    if available < minimum {
        return false;
    }
    let mut taken = available;
    loop {
        if match_parts(rest, &text[taken..]) {
            return true;
        }
        if taken == minimum {
            return false;
        }
        taken -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lower_hex_pattern_is_exact() {
        let schema = json!({ "type": "string", "pattern": "^[0-9a-f]{64}$" });
        assert!(validate(&schema, &json!("a".repeat(64))).is_ok());
        assert!(validate(&schema, &json!("A".repeat(64))).is_err());
        assert!(validate(&schema, &json!("a".repeat(63))).is_err());
    }

    #[test]
    fn additional_properties_are_rejected() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["a"],
            "properties": { "a": { "type": "integer" } }
        });
        assert!(validate(&schema, &json!({ "a": 1 })).is_ok());
        assert!(validate(&schema, &json!({ "a": 1, "b": 2 })).is_err());
        assert!(validate(&schema, &json!({})).is_err());
    }

    #[test]
    fn one_of_requires_exactly_one_branch() {
        let schema = json!({
            "oneOf": [
                { "type": "object", "additionalProperties": false,
                  "properties": { "kind": { "const": "a" } }, "required": ["kind"] },
                { "type": "object", "additionalProperties": false,
                  "properties": { "kind": { "const": "b" } }, "required": ["kind"] }
            ]
        });
        assert!(validate(&schema, &json!({ "kind": "a" })).is_ok());
        assert!(validate(&schema, &json!({ "kind": "c" })).is_err());
    }

    #[test]
    fn unknown_keyword_fails_closed() {
        let schema = json!({ "type": "string", "format": "email" });
        assert!(validate(&schema, &json!("x")).is_err());
    }

    #[test]
    fn top_level_alternation_is_supported() {
        let schema = json!({
            "type": "string",
            "pattern": "^(unix:dev=[0-9]{1,20},ino=[0-9]{1,20}|windows:volume=[0-9]{1,20},file_id=[0-9a-f]{32})$"
        });
        assert!(validate(&schema, &json!("unix:dev=1,ino=2")).is_ok());
        assert!(
            validate(
                &schema,
                &json!(format!("windows:volume=7,file_id={}", "a".repeat(32)))
            )
            .is_ok()
        );
        assert!(validate(&schema, &json!("unix:dev=,ino=2")).is_err());
        assert!(validate(&schema, &json!("plan9:dev=1,ino=2")).is_err());
    }

    #[test]
    fn nested_groups_are_rejected_rather_than_approximated() {
        let schema = json!({ "type": "string", "pattern": "^(a(b|c))$" });
        assert!(validate(&schema, &json!("ab")).is_err());
    }

    #[test]
    fn uuid_pattern_matches_dashed_lowercase_hex() {
        let schema = json!({
            "type": "string",
            "pattern": "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
        });
        assert!(validate(&schema, &json!("00000000-0000-4000-8000-000000000101")).is_ok());
        assert!(validate(&schema, &json!("00000000000040008000000000000101")).is_err());
    }
}
