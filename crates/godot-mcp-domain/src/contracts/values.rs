//! Deterministic typed representations of Godot values.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{OpaqueObjectId, ProjectResource};

macro_rules! vector {
    ($name:ident { $($field:ident: $type:ty),+ $(,)? }) => {
        #[doc = concat!("Typed Godot `", stringify!($name), "` value.")]
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            $(pub $field: $type),+
        }
    };
}

vector!(Vector2 { x: f64, y: f64 });
vector!(Vector2i { x: i64, y: i64 });
vector!(Vector3 {
    x: f64,
    y: f64,
    z: f64
});
vector!(Vector3i {
    x: i64,
    y: i64,
    z: i64
});
vector!(Vector4 {
    x: f64,
    y: f64,
    z: f64,
    w: f64
});
vector!(Vector4i {
    x: i64,
    y: i64,
    z: i64,
    w: i64
});

/// Typed Godot `Rect2`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rect2 {
    pub position: Vector2,
    pub size: Vector2,
}

/// Typed Godot `Rect2i`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rect2i {
    pub position: Vector2i,
    pub size: Vector2i,
}

/// Typed Godot 2D transform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Transform2D {
    pub x: Vector2,
    pub y: Vector2,
    pub origin: Vector2,
}

/// Typed Godot plane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Plane {
    pub normal: Vector3,
    pub distance: f64,
}

/// Typed Godot quaternion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Quaternion {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub w: f64,
}

/// Typed Godot axis-aligned bounding box.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Aabb {
    pub position: Vector3,
    pub size: Vector3,
}

/// Typed Godot basis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Basis {
    pub x: Vector3,
    pub y: Vector3,
    pub z: Vector3,
}

/// Typed Godot 3D transform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Transform3D {
    pub basis: Basis,
    pub origin: Vector3,
}

/// Four-by-four matrix used by a Godot projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Matrix4 {
    pub rows: [[f64; 4]; 4],
}

/// Typed Godot projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Projection {
    pub matrix: Matrix4,
}

/// Typed Godot color with linear component values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Color {
    pub red: f64,
    pub green: f64,
    pub blue: f64,
    pub alpha: f64,
}

/// A Godot project resource reference without a local filesystem path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceReference {
    pub resource: ProjectResource,
    pub resource_type: String,
    pub uid: Option<String>,
}

/// An opaque live-object reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObjectReference {
    pub id: OpaqueObjectId,
    pub class_name: String,
}

/// Complete first-release typed Godot value algebra.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum GodotValue {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
    StringName(String),
    NodePath(String),
    Vector2(Vector2),
    Vector2i(Vector2i),
    Rect2(Rect2),
    Rect2i(Rect2i),
    Vector3(Vector3),
    Vector3i(Vector3i),
    Transform2D(Transform2D),
    Vector4(Vector4),
    Vector4i(Vector4i),
    Plane(Plane),
    Quaternion(Quaternion),
    Aabb(Aabb),
    Basis(Basis),
    Transform3D(Transform3D),
    Projection(Projection),
    Color(Color),
    Array(Vec<GodotValue>),
    Dictionary(BTreeMap<String, GodotValue>),
    PackedByteArray(Vec<u8>),
    PackedInt32Array(Vec<i32>),
    PackedInt64Array(Vec<i64>),
    PackedFloat32Array(Vec<f32>),
    PackedFloat64Array(Vec<f64>),
    PackedStringArray(Vec<String>),
    PackedVector2Array(Vec<Vector2>),
    PackedVector3Array(Vec<Vector3>),
    PackedColorArray(Vec<Color>),
    Resource(ResourceReference),
    Object(ObjectReference),
}
