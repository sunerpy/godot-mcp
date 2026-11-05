//! Godot's text spelling of a value — what `project.godot`, `.tscn` and
//! `.tres` files and `var_to_str` write — for the typed value contract.
//!
//! [`write`] spells a [`GodotValue`] as Godot writes it, so a value written
//! here reads back as the same value in every locked Godot build; [`parse`]
//! reads the spellings of the same values back. Both follow Godot's own
//! layout: a `Basis` and the basis of a `Transform3D` list their rows, a
//! `Transform2D` and a `Projection` their columns, and a dictionary its
//! entries one per line. A live object and a resource have no spelling here:
//! a file names a resource through its own reference syntax, which this
//! module does not read or write.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::contracts::{
    Aabb, Basis, Color, GodotValue, Matrix4, Plane, Projection, Quaternion, Rect2, Rect2i,
    Transform2D, Transform3D, Vector2, Vector2i, Vector3, Vector3i, Vector4, Vector4i,
};

/// The deepest nesting of arrays and dictionaries [`parse`] reads.
pub const MAX_DEPTH: usize = 64;

/// Why a value has no text spelling here, or a text is not one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VariantTextError {
    /// The value is a live object or a resource, or a float is not finite.
    #[error("{0}")]
    Unwritable(String),
    /// The text is not the spelling of a value this module reads.
    #[error("not a value literal at byte {at}: {reason}")]
    NotParsed { at: usize, reason: String },
}

/// A Godot string literal: quotes, backslashes, newlines and tabs escaped.
#[must_use]
pub fn string_literal(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// A float as Godot writes one on its own: a whole number keeps `.0`.
fn float(value: f64) -> Result<String, VariantTextError> {
    if !value.is_finite() {
        return Err(VariantTextError::Unwritable(format!(
            "{value} has no text spelling"
        )));
    }
    let mut text = component(value)?;
    if !text.contains(['.', 'e', 'E']) {
        text.push_str(".0");
    }
    Ok(text)
}

/// A float as Godot writes one inside a constructor: a whole number without
/// a fraction, a very large or very small one with an exponent.
fn component(value: f64) -> Result<String, VariantTextError> {
    if !value.is_finite() {
        return Err(VariantTextError::Unwritable(format!(
            "{value} has no text spelling"
        )));
    }
    let magnitude = value.abs();
    Ok(if value == 0.0 {
        "0".to_owned()
    } else if !(1e-5..1e16).contains(&magnitude) {
        format!("{value:e}")
    } else if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    })
}

fn components(values: &[f64]) -> Result<String, VariantTextError> {
    Ok(values
        .iter()
        .map(|value| component(*value))
        .collect::<Result<Vec<_>, _>>()?
        .join(", "))
}

fn integers(values: impl IntoIterator<Item = i64>) -> String {
    values
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn basis_rows(basis: &Basis) -> [f64; 9] {
    [
        basis.x.x, basis.y.x, basis.z.x, basis.x.y, basis.y.y, basis.z.y, basis.x.z, basis.y.z,
        basis.z.z,
    ]
}

fn rgba(color: &Color) -> [f64; 4] {
    [color.red, color.green, color.blue, color.alpha]
}

/// The text Godot writes for `value`.
///
/// # Errors
/// [`VariantTextError::Unwritable`] for a live object, a resource or a float
/// that is not finite, at any depth.
pub fn write(value: &GodotValue) -> Result<String, VariantTextError> {
    Ok(match value {
        GodotValue::Null => "null".to_owned(),
        GodotValue::Bool(flag) => flag.to_string(),
        GodotValue::Integer(number) => number.to_string(),
        GodotValue::Float(number) => float(*number)?,
        GodotValue::String(text) => string_literal(text),
        GodotValue::StringName(text) => format!("&{}", string_literal(text)),
        GodotValue::NodePath(path) => format!("NodePath({})", string_literal(path)),
        GodotValue::Vector2(v) => format!("Vector2({})", components(&[v.x, v.y])?),
        GodotValue::Vector2i(v) => format!("Vector2i({})", integers([v.x, v.y])),
        GodotValue::Rect2(r) => format!(
            "Rect2({})",
            components(&[r.position.x, r.position.y, r.size.x, r.size.y])?
        ),
        GodotValue::Rect2i(r) => format!(
            "Rect2i({})",
            integers([r.position.x, r.position.y, r.size.x, r.size.y])
        ),
        GodotValue::Vector3(v) => format!("Vector3({})", components(&[v.x, v.y, v.z])?),
        GodotValue::Vector3i(v) => format!("Vector3i({})", integers([v.x, v.y, v.z])),
        GodotValue::Transform2D(t) => format!(
            "Transform2D({})",
            components(&[t.x.x, t.x.y, t.y.x, t.y.y, t.origin.x, t.origin.y])?
        ),
        GodotValue::Vector4(v) => format!("Vector4({})", components(&[v.x, v.y, v.z, v.w])?),
        GodotValue::Vector4i(v) => format!("Vector4i({})", integers([v.x, v.y, v.z, v.w])),
        GodotValue::Plane(p) => format!(
            "Plane({})",
            components(&[p.normal.x, p.normal.y, p.normal.z, p.distance])?
        ),
        GodotValue::Quaternion(q) => {
            format!("Quaternion({})", components(&[q.x, q.y, q.z, q.w])?)
        }
        GodotValue::Aabb(a) => format!(
            "AABB({})",
            components(&[
                a.position.x,
                a.position.y,
                a.position.z,
                a.size.x,
                a.size.y,
                a.size.z
            ])?
        ),
        GodotValue::Basis(b) => format!("Basis({})", components(&basis_rows(b))?),
        GodotValue::Transform3D(t) => {
            let mut values = basis_rows(&t.basis).to_vec();
            values.extend([t.origin.x, t.origin.y, t.origin.z]);
            format!("Transform3D({})", components(&values)?)
        }
        GodotValue::Projection(p) => {
            let rows = &p.matrix.rows;
            let columns: Vec<f64> = (0..4)
                .flat_map(|column| (0..4).map(move |row| rows[row][column]))
                .collect();
            format!("Projection({})", components(&columns)?)
        }
        GodotValue::Color(c) => format!("Color({})", components(&rgba(c))?),
        GodotValue::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(write)
                .collect::<Result<Vec<_>, _>>()?
                .join(", ")
        ),
        GodotValue::Dictionary(entries) => {
            if entries.is_empty() {
                "{}".to_owned()
            } else {
                let mut text = String::from("{\n");
                let mut first = true;
                for (key, value) in entries {
                    if !first {
                        text.push_str(",\n");
                    }
                    first = false;
                    let _ = write!(text, "{}: {}", string_literal(key), write(value)?);
                }
                text.push_str("\n}");
                text
            }
        }
        GodotValue::PackedByteArray(bytes) => format!(
            "PackedByteArray({})",
            integers(bytes.iter().map(|byte| i64::from(*byte)))
        ),
        GodotValue::PackedInt32Array(values) => format!(
            "PackedInt32Array({})",
            integers(values.iter().map(|value| i64::from(*value)))
        ),
        GodotValue::PackedInt64Array(values) => {
            format!("PackedInt64Array({})", integers(values.iter().copied()))
        }
        GodotValue::PackedFloat32Array(values) => {
            let spelled = values
                .iter()
                .map(|value| {
                    if value.is_finite() && value.fract() != 0.0 {
                        // The shortest spelling that reads back as this f32.
                        Ok(format!("{value}"))
                    } else {
                        component(f64::from(*value))
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;
            format!("PackedFloat32Array({})", spelled.join(", "))
        }
        GodotValue::PackedFloat64Array(values) => {
            format!("PackedFloat64Array({})", components(values)?)
        }
        GodotValue::PackedStringArray(values) => format!(
            "PackedStringArray({})",
            values
                .iter()
                .map(|value| string_literal(value))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        GodotValue::PackedVector2Array(values) => {
            let flat: Vec<f64> = values.iter().flat_map(|v| [v.x, v.y]).collect();
            format!("PackedVector2Array({})", components(&flat)?)
        }
        GodotValue::PackedVector3Array(values) => {
            let flat: Vec<f64> = values.iter().flat_map(|v| [v.x, v.y, v.z]).collect();
            format!("PackedVector3Array({})", components(&flat)?)
        }
        GodotValue::PackedColorArray(values) => {
            let flat: Vec<f64> = values.iter().flat_map(rgba).collect();
            format!("PackedColorArray({})", components(&flat)?)
        }
        GodotValue::Resource(_) => {
            return Err(VariantTextError::Unwritable(
                "a resource has no value spelling; name it by its res:// path as a string"
                    .to_owned(),
            ));
        }
        GodotValue::Object(_) => {
            return Err(VariantTextError::Unwritable(
                "a live object has no text spelling".to_owned(),
            ));
        }
    })
}

/// The value `text` spells, as [`write`] or Godot writes it; surrounding
/// whitespace is ignored.
///
/// # Errors
/// [`VariantTextError::NotParsed`] for text that is not one complete value
/// literal, for a resource, object or other reference, for a dictionary key
/// that is not a string, for a non-finite float, or for nesting deeper than
/// [`MAX_DEPTH`].
pub fn parse(text: &str) -> Result<GodotValue, VariantTextError> {
    let mut parser = Parser {
        text: text.as_bytes(),
        at: 0,
    };
    let value = parser.value(0)?;
    parser.blank();
    if parser.at != parser.text.len() {
        return Err(parser.error("text follows the value"));
    }
    Ok(value)
}

/// An f32 element of a packed array, as Godot stores it.
#[allow(clippy::cast_possible_truncation)]
const fn narrow(value: f64) -> f32 {
    value as f32
}

struct Parser<'a> {
    text: &'a [u8],
    at: usize,
}

/// A number as written: an integer, or a float with a fraction or exponent.
#[derive(Clone, Copy)]
enum Number {
    Integer(i64),
    Float(f64),
}

impl Number {
    #[allow(clippy::cast_precision_loss)]
    const fn as_f64(self) -> f64 {
        match self {
            Self::Integer(number) => number as f64,
            Self::Float(number) => number,
        }
    }
}

/// One argument of a constructor.
enum Argument {
    Number(Number),
    Text(String),
}

impl Parser<'_> {
    fn error(&self, reason: &str) -> VariantTextError {
        VariantTextError::NotParsed {
            at: self.at,
            reason: reason.to_owned(),
        }
    }

    fn blank(&mut self) {
        while self
            .text
            .get(self.at)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            self.at += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.blank();
        self.text.get(self.at).copied()
    }

    fn expect(&mut self, byte: u8) -> Result<(), VariantTextError> {
        if self.peek() == Some(byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(self.error(&format!("expected `{}`", byte as char)))
        }
    }

    fn word(&mut self) -> String {
        self.blank();
        let start = self.at;
        while self
            .text
            .get(self.at)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            self.at += 1;
        }
        String::from_utf8_lossy(&self.text[start..self.at]).into_owned()
    }

    fn string(&mut self) -> Result<String, VariantTextError> {
        self.expect(b'"')?;
        let mut bytes = Vec::new();
        loop {
            let Some(&byte) = self.text.get(self.at) else {
                return Err(self.error("the string is not closed"));
            };
            self.at += 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    let Some(&escaped) = self.text.get(self.at) else {
                        return Err(self.error("the string is not closed"));
                    };
                    self.at += 1;
                    match escaped {
                        b'n' => bytes.push(b'\n'),
                        b't' => bytes.push(b'\t'),
                        b'r' => bytes.push(b'\r'),
                        b'b' => bytes.push(8),
                        b'f' => bytes.push(12),
                        b'u' | b'U' => {
                            let width = if escaped == b'u' { 4 } else { 6 };
                            let digits = self
                                .text
                                .get(self.at..self.at + width)
                                .and_then(|digits| std::str::from_utf8(digits).ok())
                                .and_then(|digits| u32::from_str_radix(digits, 16).ok())
                                .and_then(char::from_u32)
                                .ok_or_else(|| self.error("a malformed \\u escape"))?;
                            self.at += width;
                            let mut buffer = [0_u8; 4];
                            bytes.extend_from_slice(digits.encode_utf8(&mut buffer).as_bytes());
                        }
                        other => bytes.push(other),
                    }
                }
                other => bytes.push(other),
            }
        }
        String::from_utf8(bytes).map_err(|_| self.error("the string is not UTF-8"))
    }

    fn number(&mut self) -> Result<Number, VariantTextError> {
        self.blank();
        let start = self.at;
        while self
            .text
            .get(self.at)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
        {
            self.at += 1;
        }
        let spelled = std::str::from_utf8(&self.text[start..self.at]).unwrap_or_default();
        if !spelled.contains(['.', 'e', 'E', 'i', 'n'])
            && let Ok(number) = spelled.parse::<i64>()
        {
            return Ok(Number::Integer(number));
        }
        let number: f64 = spelled
            .parse()
            .map_err(|_| self.error(&format!("`{spelled}` is not a number")))?;
        if !number.is_finite() {
            return Err(self.error("a float that is not finite has no typed value"));
        }
        Ok(Number::Float(number))
    }

    fn arguments(&mut self) -> Result<Vec<Argument>, VariantTextError> {
        self.expect(b'(')?;
        let mut arguments = Vec::new();
        if self.peek() == Some(b')') {
            self.at += 1;
            return Ok(arguments);
        }
        loop {
            if self.peek() == Some(b'"') {
                arguments.push(Argument::Text(self.string()?));
            } else {
                arguments.push(Argument::Number(self.number()?));
            }
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b')') => {
                    self.at += 1;
                    return Ok(arguments);
                }
                _ => return Err(self.error("expected `,` or `)`")),
            }
        }
    }

    fn floats(
        &self,
        name: &str,
        arguments: &[Argument],
        count: Option<usize>,
    ) -> Result<Vec<f64>, VariantTextError> {
        let numbers: Vec<f64> = arguments
            .iter()
            .map(|argument| match argument {
                Argument::Number(number) => Ok(number.as_f64()),
                Argument::Text(_) => Err(self.error(&format!("{name} takes numbers"))),
            })
            .collect::<Result<_, _>>()?;
        if count.is_some_and(|count| numbers.len() != count) {
            return Err(self.error(&format!(
                "{name} takes {} numbers, not {}",
                count.unwrap_or_default(),
                numbers.len()
            )));
        }
        Ok(numbers)
    }

    fn whole(
        &self,
        name: &str,
        arguments: &[Argument],
        count: Option<usize>,
    ) -> Result<Vec<i64>, VariantTextError> {
        let numbers: Vec<i64> = arguments
            .iter()
            .map(|argument| match argument {
                Argument::Number(Number::Integer(number)) => Ok(*number),
                _ => Err(self.error(&format!("{name} takes integers"))),
            })
            .collect::<Result<_, _>>()?;
        if count.is_some_and(|count| numbers.len() != count) {
            return Err(self.error(&format!(
                "{name} takes {} integers, not {}",
                count.unwrap_or_default(),
                numbers.len()
            )));
        }
        Ok(numbers)
    }

    fn value(&mut self, depth: usize) -> Result<GodotValue, VariantTextError> {
        if depth > MAX_DEPTH {
            return Err(self.error("the value nests too deep"));
        }
        match self.peek() {
            None => Err(self.error("expected a value")),
            Some(b'"') => Ok(GodotValue::String(self.string()?)),
            Some(b'&') => {
                self.at += 1;
                Ok(GodotValue::StringName(self.string()?))
            }
            Some(b'^') => {
                self.at += 1;
                Ok(GodotValue::NodePath(self.string()?))
            }
            Some(b'[') => self.array(depth),
            Some(b'{') => self.dictionary(depth),
            Some(byte) if byte == b'-' || byte == b'+' || byte == b'.' || byte.is_ascii_digit() => {
                Ok(match self.number()? {
                    Number::Integer(number) => GodotValue::Integer(number),
                    Number::Float(number) => GodotValue::Float(number),
                })
            }
            Some(_) => self.named(depth),
        }
    }

    fn array(&mut self, depth: usize) -> Result<GodotValue, VariantTextError> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.at += 1;
            return Ok(GodotValue::Array(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            match self.peek() {
                Some(b',') => {
                    self.at += 1;
                    if self.peek() == Some(b']') {
                        self.at += 1;
                        return Ok(GodotValue::Array(items));
                    }
                }
                Some(b']') => {
                    self.at += 1;
                    return Ok(GodotValue::Array(items));
                }
                _ => return Err(self.error("expected `,` or `]`")),
            }
        }
    }

    fn dictionary(&mut self, depth: usize) -> Result<GodotValue, VariantTextError> {
        self.expect(b'{')?;
        let mut entries = BTreeMap::new();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(GodotValue::Dictionary(entries));
        }
        loop {
            let key = match self.value(depth + 1)? {
                GodotValue::String(key) | GodotValue::StringName(key) => key,
                _ => return Err(self.error("only a string dictionary key has a typed value")),
            };
            self.expect(b':')?;
            let value = self.value(depth + 1)?;
            entries.insert(key, value);
            match self.peek() {
                Some(b',') => {
                    self.at += 1;
                    if self.peek() == Some(b'}') {
                        self.at += 1;
                        return Ok(GodotValue::Dictionary(entries));
                    }
                }
                Some(b'}') => {
                    self.at += 1;
                    return Ok(GodotValue::Dictionary(entries));
                }
                _ => return Err(self.error("expected `,` or `}`")),
            }
        }
    }

    /// A keyword, a constructor, or a typed array or dictionary.
    #[allow(clippy::too_many_lines)]
    fn named(&mut self, depth: usize) -> Result<GodotValue, VariantTextError> {
        let start = self.at;
        let name = self.word();
        match name.as_str() {
            "null" => return Ok(GodotValue::Null),
            "true" => return Ok(GodotValue::Bool(true)),
            "false" => return Ok(GodotValue::Bool(false)),
            "Array" | "Dictionary" => {
                // `Array[int]([1, 2])` and `Dictionary[String, int]({...})`.
                if self.peek() == Some(b'[') {
                    let mut nesting = 0_usize;
                    while let Some(&byte) = self.text.get(self.at) {
                        self.at += 1;
                        match byte {
                            b'[' => nesting += 1,
                            b']' => {
                                nesting -= 1;
                                if nesting == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                self.expect(b'(')?;
                let value = if name == "Array" {
                    self.array(depth)?
                } else {
                    self.dictionary(depth)?
                };
                self.expect(b')')?;
                return Ok(value);
            }
            "NodePath" => {
                self.expect(b'(')?;
                let path = self.string()?;
                self.expect(b')')?;
                return Ok(GodotValue::NodePath(path));
            }
            "StringName" => {
                self.expect(b'(')?;
                let text = self.string()?;
                self.expect(b')')?;
                return Ok(GodotValue::StringName(text));
            }
            "" => {
                self.at = start;
                return Err(self.error("expected a value"));
            }
            _ => {}
        }
        if self.peek() != Some(b'(') {
            self.at = start;
            return Err(self.error(&format!("`{name}` is not a value")));
        }
        let arguments = self.arguments()?;
        let f = |count| self.floats(&name, &arguments, Some(count));
        let i = |count| self.whole(&name, &arguments, Some(count));
        let vector3 = |values: &[f64]| Vector3 {
            x: values[0],
            y: values[1],
            z: values[2],
        };
        let basis = |rows: &[f64]| Basis {
            x: Vector3 {
                x: rows[0],
                y: rows[3],
                z: rows[6],
            },
            y: Vector3 {
                x: rows[1],
                y: rows[4],
                z: rows[7],
            },
            z: Vector3 {
                x: rows[2],
                y: rows[5],
                z: rows[8],
            },
        };
        let color = |values: &[f64]| Color {
            red: values[0],
            green: values[1],
            blue: values[2],
            alpha: values[3],
        };
        Ok(match name.as_str() {
            "Vector2" => {
                let v = f(2)?;
                GodotValue::Vector2(Vector2 { x: v[0], y: v[1] })
            }
            "Vector2i" => {
                let v = i(2)?;
                GodotValue::Vector2i(Vector2i { x: v[0], y: v[1] })
            }
            "Rect2" => {
                let v = f(4)?;
                GodotValue::Rect2(Rect2 {
                    position: Vector2 { x: v[0], y: v[1] },
                    size: Vector2 { x: v[2], y: v[3] },
                })
            }
            "Rect2i" => {
                let v = i(4)?;
                GodotValue::Rect2i(Rect2i {
                    position: Vector2i { x: v[0], y: v[1] },
                    size: Vector2i { x: v[2], y: v[3] },
                })
            }
            "Vector3" => GodotValue::Vector3(vector3(&f(3)?)),
            "Vector3i" => {
                let v = i(3)?;
                GodotValue::Vector3i(Vector3i {
                    x: v[0],
                    y: v[1],
                    z: v[2],
                })
            }
            "Transform2D" => {
                let v = f(6)?;
                GodotValue::Transform2D(Transform2D {
                    x: Vector2 { x: v[0], y: v[1] },
                    y: Vector2 { x: v[2], y: v[3] },
                    origin: Vector2 { x: v[4], y: v[5] },
                })
            }
            "Vector4" => {
                let v = f(4)?;
                GodotValue::Vector4(Vector4 {
                    x: v[0],
                    y: v[1],
                    z: v[2],
                    w: v[3],
                })
            }
            "Vector4i" => {
                let v = i(4)?;
                GodotValue::Vector4i(Vector4i {
                    x: v[0],
                    y: v[1],
                    z: v[2],
                    w: v[3],
                })
            }
            "Plane" => {
                let v = f(4)?;
                GodotValue::Plane(Plane {
                    normal: vector3(&v),
                    distance: v[3],
                })
            }
            "Quaternion" | "Quat" => {
                let v = f(4)?;
                GodotValue::Quaternion(Quaternion {
                    x: v[0],
                    y: v[1],
                    z: v[2],
                    w: v[3],
                })
            }
            "AABB" => {
                let v = f(6)?;
                GodotValue::Aabb(Aabb {
                    position: vector3(&v[0..3]),
                    size: vector3(&v[3..6]),
                })
            }
            "Basis" => GodotValue::Basis(basis(&f(9)?)),
            "Transform3D" | "Transform" => {
                let v = f(12)?;
                GodotValue::Transform3D(Transform3D {
                    basis: basis(&v[0..9]),
                    origin: vector3(&v[9..12]),
                })
            }
            "Projection" => {
                let v = f(16)?;
                let mut rows = [[0.0; 4]; 4];
                for (column, values) in v.chunks(4).enumerate() {
                    for (row, value) in values.iter().enumerate() {
                        rows[row][column] = *value;
                    }
                }
                GodotValue::Projection(Projection {
                    matrix: Matrix4 { rows },
                })
            }
            "Color" => GodotValue::Color(color(&f(4)?)),
            "PackedByteArray" | "PoolByteArray" => GodotValue::PackedByteArray(
                self.whole(&name, &arguments, None)?
                    .into_iter()
                    .map(|value| u8::try_from(value).map_err(|_| self.error("a byte is 0 to 255")))
                    .collect::<Result<_, _>>()?,
            ),
            "PackedInt32Array" | "PoolIntArray" => GodotValue::PackedInt32Array(
                self.whole(&name, &arguments, None)?
                    .into_iter()
                    .map(|value| {
                        i32::try_from(value)
                            .map_err(|_| self.error("the value is not a 32-bit integer"))
                    })
                    .collect::<Result<_, _>>()?,
            ),
            "PackedInt64Array" => {
                GodotValue::PackedInt64Array(self.whole(&name, &arguments, None)?)
            }
            "PackedFloat32Array" | "PoolRealArray" => GodotValue::PackedFloat32Array(
                self.floats(&name, &arguments, None)?
                    .into_iter()
                    .map(narrow)
                    .collect(),
            ),
            "PackedFloat64Array" => {
                GodotValue::PackedFloat64Array(self.floats(&name, &arguments, None)?)
            }
            "PackedStringArray" | "PoolStringArray" => GodotValue::PackedStringArray(
                arguments
                    .into_iter()
                    .map(|argument| match argument {
                        Argument::Text(text) => Ok(text),
                        Argument::Number(..) => Err(self.error("PackedStringArray takes strings")),
                    })
                    .collect::<Result<_, _>>()?,
            ),
            "PackedVector2Array" | "PoolVector2Array" => {
                let v = self.floats(&name, &arguments, None)?;
                if v.len() % 2 != 0 {
                    return Err(self.error("PackedVector2Array takes pairs of numbers"));
                }
                GodotValue::PackedVector2Array(
                    v.chunks(2)
                        .map(|pair| Vector2 {
                            x: pair[0],
                            y: pair[1],
                        })
                        .collect(),
                )
            }
            "PackedVector3Array" | "PoolVector3Array" => {
                let v = self.floats(&name, &arguments, None)?;
                if v.len() % 3 != 0 {
                    return Err(self.error("PackedVector3Array takes triples of numbers"));
                }
                GodotValue::PackedVector3Array(v.chunks(3).map(vector3).collect())
            }
            "PackedColorArray" | "PoolColorArray" => {
                let v = self.floats(&name, &arguments, None)?;
                if v.len() % 4 != 0 {
                    return Err(self.error("PackedColorArray takes four numbers per color"));
                }
                GodotValue::PackedColorArray(v.chunks(4).map(color).collect())
            }
            _ => {
                self.at = start;
                return Err(self.error(&format!("`{name}(...)` has no typed value")));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What Godot 4.4.1 and 4.7.1 print with `var_to_str` for these values,
    /// captured from both builds; the two agree on every spelling but the
    /// precision of a float on its own.
    const GODOT_SPELLINGS: &[&str] = &[
        "null",
        "true",
        "false",
        "0",
        "-7",
        "9007199254740993",
        "0.5",
        "2.0",
        "1e+20",
        "1e-05",
        "3.14159265358979",
        "&\"name\"",
        "NodePath(\"A/B:c\")",
        "Vector2(1, 2.5)",
        "Vector2i(-3, 4)",
        "Rect2(1, 2, 3, 4)",
        "Rect2i(1, 2, 3, 4)",
        "Vector3(1, 2, 3)",
        "Vector3i(1, 2, 3)",
        "Transform2D(1, 2, 3, 4, 5, 6)",
        "Vector4(1, 2, 3, 4)",
        "Vector4i(1, 2, 3, 4)",
        "Plane(0, 1, 0, 5)",
        "Quaternion(0, 0, 0, 1)",
        "AABB(1, 2, 3, 4, 5, 6)",
        "Basis(1, 4, 7, 2, 5, 8, 3, 6, 9)",
        "Transform3D(1, 4, 7, 2, 5, 8, 3, 6, 9, 10, 11, 12)",
        "Projection(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16)",
        "Color(1, 0.5, 0, 1)",
        "[1, \"a\", [true]]",
        "{\n\"a\": [2],\n\"b\": 1\n}",
        "PackedByteArray(1, 2, 255)",
        "PackedInt32Array(1, -2)",
        "PackedInt64Array(3)",
        "PackedFloat32Array(0.5, 1)",
        "PackedFloat64Array(0.25)",
        "PackedStringArray(\"x\", \"y z\")",
        "PackedVector2Array(1, 2)",
        "PackedVector3Array(1, 2, 3)",
        "PackedColorArray(1, 0, 0, 1)",
        "[]",
        "{}",
        "PackedByteArray()",
        "Vector2(0.1, 1e-07)",
    ];

    #[test]
    fn every_godot_spelling_reads_back_and_writes_as_godot_does() {
        for spelled in GODOT_SPELLINGS {
            let value = parse(spelled).unwrap_or_else(|error| panic!("{spelled}: {error}"));
            let written = write(&value).unwrap_or_else(|error| panic!("{spelled}: {error}"));
            assert_eq!(
                parse(&written).expect("a written value reads back"),
                value,
                "{spelled} -> {written}"
            );
            // Godot's exponent spelling differs from this module's; every
            // other spelling is written exactly as Godot writes it.
            if !spelled.contains("e+") && !spelled.contains("e-") {
                assert_eq!(&written, spelled, "{spelled}");
            }
        }
    }

    #[test]
    fn the_layout_matches_godot_for_matrices() {
        let basis = parse("Basis(1, 4, 7, 2, 5, 8, 3, 6, 9)").expect("basis");
        let GodotValue::Basis(basis) = basis else {
            panic!("a basis")
        };
        // Godot's Basis(Vector3(1, 2, 3), Vector3(4, 5, 6), Vector3(7, 8, 9))
        // takes its axes, the columns, and writes its rows.
        assert_eq!((basis.x.x, basis.x.y, basis.x.z), (1.0, 2.0, 3.0));
        assert_eq!((basis.y.x, basis.y.y, basis.y.z), (4.0, 5.0, 6.0));
        let GodotValue::Projection(projection) =
            parse("Projection(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16)")
                .expect("projection")
        else {
            panic!("a projection")
        };
        // Its first column is Vector4(1, 2, 3, 4).
        assert_eq!(
            [0, 1, 2, 3].map(|row| projection.matrix.rows[row][0]),
            [1.0, 2.0, 3.0, 4.0]
        );
    }

    #[test]
    fn strings_escape_what_godot_reads() {
        let value = GodotValue::String("a \"q\" \\ \n\tb é".to_owned());
        let written = write(&value).expect("written");
        assert_eq!(written, "\"a \\\"q\\\" \\\\ \\n\\tb é\"");
        assert_eq!(parse(&written).expect("parsed"), value);
        assert_eq!(
            parse("\"\\u00e9\"").expect("unicode escape"),
            GodotValue::String("é".to_owned())
        );
        // Godot writes a newline inside a string as it is.
        assert_eq!(
            parse("\"two\nlines\"").expect("raw newline"),
            GodotValue::String("two\nlines".to_owned())
        );
    }

    #[test]
    fn typed_containers_and_old_names_read_as_their_values() {
        assert_eq!(
            parse("Array[int]([1, 2])").expect("typed array"),
            GodotValue::Array(vec![GodotValue::Integer(1), GodotValue::Integer(2)])
        );
        assert_eq!(
            parse("Dictionary[String, int]({\n\"a\": 1\n})").expect("typed dictionary"),
            GodotValue::Dictionary(BTreeMap::from([("a".to_owned(), GodotValue::Integer(1))]))
        );
        assert_eq!(
            parse("[1, 2,]").expect("trailing comma"),
            GodotValue::Array(vec![GodotValue::Integer(1), GodotValue::Integer(2)])
        );
        assert_eq!(
            parse("^\"A/B\"").expect("node path shorthand"),
            GodotValue::NodePath("A/B".to_owned())
        );
    }

    #[test]
    fn references_objects_and_malformed_text_are_refused() {
        for text in [
            "ExtResource(\"1\")",
            "SubResource(\"Animation_x\")",
            "Resource(\"res://a.tres\")",
            "Object(InputEventKey,\"keycode\":4194309)",
            "Callable()",
            "Vector2(1)",
            "Vector2i(1.5, 2)",
            "PackedByteArray(256)",
            "{1: 2}",
            "[1, 2",
            "\"open",
            "1 2",
            "inf",
            "nan",
            "",
        ] {
            assert!(parse(text).is_err(), "{text:?} is refused");
        }
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH + 2), "]".repeat(MAX_DEPTH + 2));
        assert!(parse(&deep).is_err());
        let resource = GodotValue::Resource(crate::contracts::ResourceReference {
            resource: crate::contracts::ProjectResource::parse("res://a.tres").expect("resource"),
            resource_type: "Resource".to_owned(),
            uid: None,
        });
        assert!(write(&resource).is_err());
        assert!(write(&GodotValue::Float(f64::INFINITY)).is_err());
    }

    #[test]
    fn floats_keep_a_fraction_on_their_own_and_round_trip() {
        for (value, spelled) in [
            (2.0, "2.0"),
            (-3.0, "-3.0"),
            (0.1, "0.1"),
            (1e20, "1e20"),
            (2.5e-7, "2.5e-7"),
            (0.0, "0.0"),
        ] {
            let written = write(&GodotValue::Float(value)).expect("written");
            assert_eq!(written, spelled);
            assert_eq!(parse(&written).expect("parsed"), GodotValue::Float(value));
        }
        let written = write(&GodotValue::PackedFloat32Array(vec![0.1, 2.0])).expect("written");
        assert_eq!(written, "PackedFloat32Array(0.1, 2)");
    }
}
