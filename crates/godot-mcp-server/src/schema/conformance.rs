//! A partial JSON Schema checker, for tests that hold an advertised schema and
//! the handler behind it to the same verdict on the same cases.
//!
//! It knows the keywords the tool schemas use and a regular-expression subset
//! large enough for their patterns. Anything else panics, so a schema the
//! checker cannot read fails its test instead of passing it.

use serde_json::{Map, Value};

/// Whether `instance` conforms to `schema`. A `$ref` resolves against `schema`
/// itself, which must therefore carry the `$defs` it refers to.
pub(crate) fn conforms(schema: &Value, instance: &Value) -> bool {
    Checker { root: schema }.check(schema, instance)
}

/// Whether `pattern` matches somewhere in `text`, as the `pattern` keyword
/// applies it.
pub(crate) fn pattern_matches(pattern: &str, text: &str) -> bool {
    Pattern::parse(pattern).search(text)
}

/// Keywords that never change a verdict.
const ANNOTATIONS: &[&str] = &["description", "default", "title", "format", "$defs"];

struct Checker<'a> {
    root: &'a Value,
}

impl Checker<'_> {
    fn check(&self, schema: &Value, instance: &Value) -> bool {
        match schema {
            Value::Bool(verdict) => *verdict,
            Value::Object(keywords) => keywords
                .iter()
                .all(|(keyword, argument)| self.keyword(keywords, keyword, argument, instance)),
            other => panic!("not a schema: {other}"),
        }
    }

    fn keyword(
        &self,
        schema: &Map<String, Value>,
        keyword: &str,
        argument: &Value,
        instance: &Value,
    ) -> bool {
        match keyword {
            _ if ANNOTATIONS.contains(&keyword) => true,
            "$ref" => self.check(self.resolve(argument), instance),
            "type" => match argument {
                Value::String(kind) => is_type(kind, instance),
                Value::Array(kinds) => kinds
                    .iter()
                    .any(|kind| is_type(kind.as_str().expect("a type name"), instance)),
                other => panic!("type must name types: {other}"),
            },
            "const" => same(argument, instance),
            "enum" => list(argument).iter().any(|option| same(option, instance)),
            "oneOf" => {
                list(argument)
                    .iter()
                    .filter(|option| self.check(option, instance))
                    .count()
                    == 1
            }
            "anyOf" => list(argument)
                .iter()
                .any(|option| self.check(option, instance)),
            "allOf" => list(argument).iter().all(|part| self.check(part, instance)),
            "not" => !self.check(argument, instance),
            "required" => instance.as_object().is_none_or(|object| {
                list(argument)
                    .iter()
                    .all(|name| object.contains_key(name.as_str().expect("a property name")))
            }),
            "properties" => instance.as_object().is_none_or(|object| {
                argument
                    .as_object()
                    .expect("properties are named schemas")
                    .iter()
                    .all(|(name, property)| {
                        object
                            .get(name)
                            .is_none_or(|value| self.check(property, value))
                    })
            }),
            "additionalProperties" => instance.as_object().is_none_or(|object| {
                let declared = schema.get("properties").and_then(Value::as_object);
                object
                    .iter()
                    .filter(|(name, _)| {
                        declared.is_none_or(|declared| !declared.contains_key(*name))
                    })
                    .all(|(_, value)| self.check(argument, value))
            }),
            "items" => instance
                .as_array()
                .is_none_or(|items| items.iter().all(|item| self.check(argument, item))),
            "minItems" => instance
                .as_array()
                .is_none_or(|items| items.len() >= count(argument)),
            "maxItems" => instance
                .as_array()
                .is_none_or(|items| items.len() <= count(argument)),
            "minProperties" => instance
                .as_object()
                .is_none_or(|object| object.len() >= count(argument)),
            "maxProperties" => instance
                .as_object()
                .is_none_or(|object| object.len() <= count(argument)),
            "minLength" => instance
                .as_str()
                .is_none_or(|text| text.chars().count() >= count(argument)),
            "maxLength" => instance
                .as_str()
                .is_none_or(|text| text.chars().count() <= count(argument)),
            "pattern" => instance.as_str().is_none_or(|text| {
                pattern_matches(argument.as_str().expect("a pattern string"), text)
            }),
            "minimum" => number(instance).is_none_or(|value| value >= bound(argument)),
            "maximum" => number(instance).is_none_or(|value| value <= bound(argument)),
            "exclusiveMinimum" => number(instance).is_none_or(|value| value > bound(argument)),
            "exclusiveMaximum" => number(instance).is_none_or(|value| value < bound(argument)),
            other => panic!("the conformance checker does not know the keyword `{other}`"),
        }
    }

    fn resolve(&self, reference: &Value) -> &Value {
        let name = reference
            .as_str()
            .and_then(|reference| reference.strip_prefix("#/$defs/"))
            .unwrap_or_else(|| panic!("only local #/$defs references resolve: {reference}"));
        self.root
            .get("$defs")
            .and_then(|definitions| definitions.get(name))
            .unwrap_or_else(|| panic!("{reference} names no definition"))
    }
}

fn is_type(kind: &str, instance: &Value) -> bool {
    match kind {
        "null" => instance.is_null(),
        "boolean" => instance.is_boolean(),
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "number" => instance.is_number(),
        "integer" => instance.as_number().is_some_and(|number| {
            number.is_i64()
                || number.is_u64()
                || number.as_f64().is_some_and(|value| value.fract() == 0.0)
        }),
        other => panic!("unknown type `{other}`"),
    }
}

/// JSON equality, numbers compared by value.
fn same(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len() && left.iter().zip(right).all(|(a, b)| same(a, b))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .all(|(key, value)| right.get(key).is_some_and(|other| same(value, other)))
        }
        _ => left == right,
    }
}

fn list(argument: &Value) -> &[Value] {
    argument
        .as_array()
        .unwrap_or_else(|| panic!("expected a list: {argument}"))
}

fn count(argument: &Value) -> usize {
    argument
        .as_u64()
        .and_then(|count| usize::try_from(count).ok())
        .unwrap_or_else(|| panic!("expected a count: {argument}"))
}

fn bound(argument: &Value) -> f64 {
    argument
        .as_f64()
        .unwrap_or_else(|| panic!("expected a number: {argument}"))
}

fn number(instance: &Value) -> Option<f64> {
    instance.as_number().and_then(serde_json::Number::as_f64)
}

// ---------------------------------------------------------------------------
// The regular-expression subset
// ---------------------------------------------------------------------------

/// A parsed pattern: alternatives of sequences of quantified atoms, with
/// classes, groups, lookaheads and the `^`/`$` assertions.
struct Pattern {
    alternatives: Vec<Vec<Item>>,
}

struct Item {
    atom: Atom,
    min: usize,
    max: Option<usize>,
}

enum Atom {
    Literal(char),
    Any,
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
    Start,
    End,
    Group(Vec<Vec<Item>>),
    Lookahead {
        negated: bool,
        alternatives: Vec<Vec<Item>>,
    },
}

type Rest<'r> = &'r mut dyn FnMut(usize) -> bool;

impl Pattern {
    fn parse(source: &str) -> Self {
        let characters: Vec<char> = source.chars().collect();
        let mut parser = Parser {
            source,
            characters: &characters,
            at: 0,
        };
        let alternatives = parser.alternatives();
        assert!(
            parser.at == characters.len(),
            "unbalanced `)` in pattern {source:?}"
        );
        Self { alternatives }
    }

    fn search(&self, text: &str) -> bool {
        let text: Vec<char> = text.chars().collect();
        (0..=text.len()).any(|start| {
            self.alternatives
                .iter()
                .any(|sequence| sequence_matches(sequence, &text, start, &mut |_| true))
        })
    }
}

fn sequence_matches(items: &[Item], text: &[char], at: usize, rest: Rest<'_>) -> bool {
    match items.split_first() {
        None => rest(at),
        Some((first, tail)) => repeat_matches(first, 0, text, at, &mut |next| {
            sequence_matches(tail, text, next, &mut *rest)
        }),
    }
}

/// Match `item` greedily, `done` repetitions already behind `at`.
fn repeat_matches(item: &Item, done: usize, text: &[char], at: usize, rest: Rest<'_>) -> bool {
    let more = item.max.is_none_or(|max| done < max);
    // A repetition that consumed nothing is only retried while the minimum
    // still needs it, so an empty match cannot loop.
    if more
        && atom_matches(&item.atom, text, at, &mut |next| {
            (next > at || done < item.min) && repeat_matches(item, done + 1, text, next, &mut *rest)
        })
    {
        return true;
    }
    done >= item.min && rest(at)
}

fn atom_matches(atom: &Atom, text: &[char], at: usize, rest: Rest<'_>) -> bool {
    match atom {
        Atom::Literal(expected) => text.get(at) == Some(expected) && rest(at + 1),
        Atom::Any => text.get(at).is_some_and(|found| *found != '\n') && rest(at + 1),
        Atom::Class { negated, ranges } => {
            text.get(at).is_some_and(|found| {
                ranges
                    .iter()
                    .any(|(low, high)| (*low..=*high).contains(found))
                    != *negated
            }) && rest(at + 1)
        }
        Atom::Start => at == 0 && rest(at),
        Atom::End => at == text.len() && rest(at),
        Atom::Group(alternatives) => alternatives
            .iter()
            .any(|sequence| sequence_matches(sequence, text, at, &mut *rest)),
        Atom::Lookahead {
            negated,
            alternatives,
        } => {
            let found = alternatives
                .iter()
                .any(|sequence| sequence_matches(sequence, text, at, &mut |_| true));
            found != *negated && rest(at)
        }
    }
}

struct Parser<'s> {
    source: &'s str,
    characters: &'s [char],
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.characters.get(self.at).copied()
    }

    fn next(&mut self) -> char {
        let character = self
            .peek()
            .unwrap_or_else(|| panic!("pattern {:?} ends early", self.source));
        self.at += 1;
        character
    }

    fn eat(&mut self, text: &str) -> bool {
        let wanted: Vec<char> = text.chars().collect();
        if self.characters[self.at..].starts_with(&wanted) {
            self.at += wanted.len();
            true
        } else {
            false
        }
    }

    fn alternatives(&mut self) -> Vec<Vec<Item>> {
        let mut alternatives = vec![self.sequence()];
        while self.peek() == Some('|') {
            self.at += 1;
            alternatives.push(self.sequence());
        }
        alternatives
    }

    fn sequence(&mut self) -> Vec<Item> {
        let mut items = Vec::new();
        while let Some(character) = self.peek() {
            if matches!(character, '|' | ')') {
                break;
            }
            let atom = self.atom();
            let (min, max) = self.quantifier();
            items.push(Item { atom, min, max });
        }
        items
    }

    fn atom(&mut self) -> Atom {
        match self.next() {
            '(' => {
                let atom = if self.eat("?:") {
                    Atom::Group(self.alternatives())
                } else if self.eat("?=") {
                    Atom::Lookahead {
                        negated: false,
                        alternatives: self.alternatives(),
                    }
                } else if self.eat("?!") {
                    Atom::Lookahead {
                        negated: true,
                        alternatives: self.alternatives(),
                    }
                } else if self.peek() == Some('?') {
                    panic!("unsupported group in pattern {:?}", self.source)
                } else {
                    Atom::Group(self.alternatives())
                };
                assert_eq!(self.next(), ')', "unclosed group in {:?}", self.source);
                atom
            }
            '[' => self.class(),
            '.' => Atom::Any,
            '^' => Atom::Start,
            '$' => Atom::End,
            '\\' => self.escape(),
            '*' | '+' | '?' | '{' => {
                panic!("a quantifier without an atom in {:?}", self.source)
            }
            literal => Atom::Literal(literal),
        }
    }

    fn escape(&mut self) -> Atom {
        match self.next() {
            'd' => Atom::Class {
                negated: false,
                ranges: vec![('0', '9')],
            },
            'x' => Atom::Literal(self.hex()),
            character if !character.is_ascii_alphanumeric() => Atom::Literal(character),
            other => panic!("unsupported escape \\{other} in {:?}", self.source),
        }
    }

    fn hex(&mut self) -> char {
        let digits: String = [self.next(), self.next()].iter().collect();
        u32::from_str_radix(&digits, 16)
            .ok()
            .and_then(char::from_u32)
            .unwrap_or_else(|| panic!("bad \\x escape in {:?}", self.source))
    }

    fn class(&mut self) -> Atom {
        let negated = self.eat("^");
        let mut ranges = Vec::new();
        let mut first = true;
        loop {
            let character = self.next();
            if character == ']' && !first {
                break;
            }
            first = false;
            let low = self.class_member(character);
            if self.peek() == Some('-') && self.characters.get(self.at + 1) != Some(&']') {
                self.at += 1;
                let next = self.next();
                let high = self.class_member(next);
                ranges.push((low, high));
            } else {
                ranges.push((low, low));
            }
        }
        Atom::Class { negated, ranges }
    }

    fn class_member(&mut self, character: char) -> char {
        if character != '\\' {
            return character;
        }
        match self.next() {
            'x' => self.hex(),
            escaped if !escaped.is_ascii_alphanumeric() => escaped,
            other => panic!("unsupported class escape \\{other} in {:?}", self.source),
        }
    }

    fn quantifier(&mut self) -> (usize, Option<usize>) {
        match self.peek() {
            Some('*') => {
                self.at += 1;
                (0, None)
            }
            Some('+') => {
                self.at += 1;
                (1, None)
            }
            Some('?') => {
                self.at += 1;
                (0, Some(1))
            }
            Some('{') => {
                self.at += 1;
                let min = self.number();
                let max = if self.eat(",") {
                    if self.peek() == Some('}') {
                        None
                    } else {
                        Some(self.number())
                    }
                } else {
                    Some(min)
                };
                assert_eq!(self.next(), '}', "unclosed quantifier in {:?}", self.source);
                (min, max)
            }
            _ => (1, Some(1)),
        }
    }

    fn number(&mut self) -> usize {
        let start = self.at;
        while self
            .peek()
            .is_some_and(|character| character.is_ascii_digit())
        {
            self.at += 1;
        }
        self.characters[start..self.at]
            .iter()
            .collect::<String>()
            .parse()
            .unwrap_or_else(|_| panic!("bad count in {:?}", self.source))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn patterns_search_unanchored_unless_they_anchor() {
        assert!(pattern_matches("^res://", "res://a.tscn"));
        assert!(!pattern_matches("^res://", "xres://a.tscn"));
        assert!(pattern_matches("b.d", "abcde"));
        assert!(pattern_matches("^[0-9a-f]{64}$", &"a".repeat(64)));
        assert!(!pattern_matches("^[0-9a-f]{64}$", &"a".repeat(63)));
        assert!(!pattern_matches("^[0-9a-f]{64}$", &"A".repeat(64)));
        assert!(pattern_matches(
            "^(?:[Ss][Pp]|[Ff](?:[1-9]|1[0-2]))$",
            "f12"
        ));
        assert!(!pattern_matches(
            "^(?:[Ss][Pp]|[Ff](?:[1-9]|1[0-2]))$",
            "F13"
        ));
        assert!(pattern_matches(r"^a(?!b)", "ac"));
        assert!(!pattern_matches(r"^a(?!b)", "ab"));
        assert!(pattern_matches(r"^[^/\\]+$", "name.tres"));
        assert!(!pattern_matches(r"^[^/\\]+$", "a\\b"));
        assert!(pattern_matches(r"^[\x00-\x1F]$", "\u{1}"));
    }

    /// The engine's own path pattern gives the verdict of the engine's path
    /// check, so this checker reads lookaheads the way a client does.
    #[test]
    fn the_project_path_pattern_agrees_with_the_path_check() {
        use godot_mcp_engine::validation::{
            PROJECT_RELATIVE_PATH_PATTERN, is_valid_project_relative_path,
        };
        for path in [
            "main.tscn",
            "res://scenes/main.tscn",
            "a/b/c.gd",
            "../escape.gd",
            "a/../b.gd",
            "/absolute.gd",
            "C:/drive.gd",
            "-flag.gd",
            "a\\b.gd",
            "http://host/x",
            "tab\there.gd",
            "",
        ] {
            assert_eq!(
                pattern_matches(PROJECT_RELATIVE_PATH_PATTERN, path),
                is_valid_project_relative_path(path),
                "{path:?}"
            );
        }
    }

    #[test]
    fn one_of_needs_exactly_one_match_and_references_resolve() {
        let schema = json!({
            "$defs": { "small": { "type": "integer", "maximum": 5 } },
            "oneOf": [
                { "$ref": "#/$defs/small" },
                { "type": "integer", "minimum": 3 }
            ]
        });
        assert!(conforms(&schema, &json!(1)));
        assert!(!conforms(&schema, &json!(4)), "both alternatives match");
        assert!(conforms(&schema, &json!(9)));
        assert!(!conforms(&schema, &json!("1")));
    }

    #[test]
    fn property_counts_bound_an_object() {
        let schema = json!({ "type": "object", "minProperties": 1, "maxProperties": 1 });
        assert!(conforms(&schema, &json!({ "a": 1 })));
        assert!(!conforms(&schema, &json!({})));
        assert!(!conforms(&schema, &json!({ "a": 1, "b": 2 })));
    }

    #[test]
    fn objects_honour_required_and_undeclared_fields() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "a": { "type": "string" } },
            "required": ["a"]
        });
        assert!(conforms(&schema, &json!({ "a": "x" })));
        assert!(!conforms(&schema, &json!({})));
        assert!(!conforms(&schema, &json!({ "a": "x", "b": 1 })));
        assert!(!conforms(&schema, &json!({ "a": 1 })));
        let map = json!({ "type": "object", "additionalProperties": { "type": "integer" } });
        assert!(conforms(&map, &json!({ "any": 1 })));
        assert!(!conforms(&map, &json!({ "any": 1.5 })));
    }
}
