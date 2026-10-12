//! The live-editor plane: the bridge to the `godot_mcp` editor plugin and the
//! editor tools that use it.

pub(crate) mod boundary;
pub(crate) mod bridge;
pub(crate) mod tools;

pub(crate) use tools::input_schema;
