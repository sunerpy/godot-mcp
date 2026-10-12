use godot_mcp_domain::ErrorKind;
use godot_mcp_domain::contracts::OutcomeError;
use godot_mcp_engine::canonical::request::{CanonicalOperation, CanonicalRequest};
use godot_mcp_engine::validation::framework::{selection_separators, validate_selection_element};
use godot_mcp_engine::validation::{
    PROJECT_RELATIVE_PATH_PATTERN, is_valid_project_relative_path, project_relative_path,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

pub(crate) use godot_mcp_domain::catalog::EngineOperation;

use super::authored::{self, AuthorKind, AuthorRequest};
use super::outcome_error;
use crate::arguments::deserialize_non_negative_whole_u64;

/// What each engine operation takes and runs, beside the catalog descriptor
/// its operation serves.
pub(super) trait EngineCall: Copy {
    fn input_schema(self) -> Map<String, Value>;
    /// The semantic authoring tool this operation is, if it is one.
    fn author_kind(self) -> Option<AuthorKind>;
}

impl EngineCall for EngineOperation {
    fn input_schema(self) -> Map<String, Value> {
        match self {
            Self::ImportProject => ImportArguments::input_schema(),
            Self::SceneCreate | Self::SceneBatch | Self::ResourceCreate | Self::ResourceBatch => {
                authored::input_schema(self.author_kind().expect("an authored operation"))
            }
            Self::UpdateUid | Self::UpdateResource | Self::SaveScene => {
                CanonicalArguments::input_schema()
            }
            Self::RunProject => RunArguments::input_schema(),
            Self::RunTests => TestArguments::input_schema(),
            Self::CheckScript => ScriptArguments::input_schema(),
        }
    }

    fn author_kind(self) -> Option<AuthorKind> {
        match self {
            Self::SceneCreate => Some(AuthorKind::SceneCreate),
            Self::SceneBatch => Some(AuthorKind::SceneBatch),
            Self::ResourceCreate => Some(AuthorKind::ResourceCreate),
            Self::ResourceBatch => Some(AuthorKind::ResourceBatch),
            _ => None,
        }
    }
}

/// The strict input schema of one engine tool.
pub(crate) fn input_schema(operation: EngineOperation) -> Map<String, Value> {
    operation.input_schema()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum EngineRequest {
    ImportProject {
        expected_artifacts: Vec<String>,
        assets: Vec<String>,
        deadline_ms: u64,
    },
    Author(AuthorRequest),
    UpdateUid(CanonicalArguments),
    UpdateResource(CanonicalArguments),
    SaveScene(CanonicalArguments),
    RunProject {
        main_scene: Option<String>,
        headless: bool,
        deadline_ms: u64,
    },
    RunTests {
        selection: Vec<String>,
        deadline_ms: u64,
    },
    CheckScript {
        script: String,
        deadline_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CanonicalArguments {
    target: String,
    #[serde(default)]
    resource_name: Option<String>,
    #[serde(default = "default_false")]
    dry_run: bool,
}

impl CanonicalArguments {
    fn input_schema() -> Map<String, Value> {
        object_schema(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "target": project_relative_path_schema(
                    "Project-relative target resource path, optionally res://-prefixed."
                ),
                "resource_name": {
                    "type": "string",
                    "description": "Resource name to assign during canonicalization."
                },
                "dry_run": {
                    "type": "boolean",
                    "default": default_false(),
                    "description": "Whether to validate without committing the update."
                }
            },
            "required": ["target"],
        }))
    }

    pub(super) fn into_request(self, operation: CanonicalOperation) -> CanonicalRequest {
        let target = project_relative_path(&self.target).to_owned();
        CanonicalRequest {
            operation,
            target,
            resource_name: self.resource_name,
            root_type: None,
            root_name: None,
            query: None,
            resource_class: None,
            batch: None,
            handles: None,
            open_resources: Vec::new(),
            dry_run: self.dry_run,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportArguments {
    #[serde(default = "default_expected_artifacts")]
    expected_artifacts: Vec<String>,
    #[serde(default = "default_import_assets")]
    assets: Vec<String>,
    #[serde(
        default = "default_import_deadline_ms",
        deserialize_with = "deserialize_non_negative_whole_u64"
    )]
    deadline_ms: u64,
}

impl ImportArguments {
    fn input_schema() -> Map<String, Value> {
        object_schema(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "expected_artifacts": {
                    "type": "array",
                    "items": project_relative_path_schema(
                        "Project-relative artifact path expected after import, optionally res://-prefixed."
                    ),
                    "default": default_expected_artifacts(),
                    "description": "Project-relative artifact paths expected after import, optionally res://-prefixed."
                },
                "assets": {
                    "type": "array",
                    "maxItems": MAX_IMPORT_ASSETS,
                    "items": project_relative_path_schema(
                        "Project-relative asset path whose import state is reported."
                    ),
                    "default": default_import_assets(),
                    "description": "Assets whose import state and imported artifact hashes are reported; each one's .import sidecar is an expected artifact."
                },
                "deadline_ms": deadline_schema(default_import_deadline_ms())
            },
            "required": [],
        }))
    }
}

/// Most assets one import reports on.
const MAX_IMPORT_ASSETS: usize = 64;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArguments {
    #[serde(default)]
    main_scene: Option<String>,
    #[serde(default = "default_true")]
    headless: bool,
    #[serde(
        default = "default_run_deadline_ms",
        deserialize_with = "deserialize_non_negative_whole_u64"
    )]
    deadline_ms: u64,
}

impl RunArguments {
    fn input_schema() -> Map<String, Value> {
        object_schema(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "main_scene": project_relative_path_schema(
                    "Project-relative main scene path to run, optionally res://-prefixed."
                ),
                "headless": {
                    "type": "boolean",
                    "default": default_true(),
                    "description": "Whether to run Godot without a display."
                },
                "deadline_ms": deadline_schema(default_run_deadline_ms())
            },
            "required": [],
        }))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TestArguments {
    #[serde(default = "default_selection")]
    selection: Vec<String>,
    #[serde(
        default = "default_test_deadline_ms",
        deserialize_with = "deserialize_non_negative_whole_u64"
    )]
    deadline_ms: u64,
}

impl TestArguments {
    fn input_schema() -> Map<String, Value> {
        object_schema(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "selection": {
                    "type": "array",
                    "items": test_selection_path_schema(
                        "Test suite or directory path, optionally res://-prefixed; framework list separators are not allowed."
                    ),
                    "default": default_selection(),
                    "description": "Test paths passed to the detected test framework."
                },
                "deadline_ms": deadline_schema(default_test_deadline_ms())
            },
            "required": [],
        }))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptArguments {
    script: String,
    #[serde(
        default = "default_script_deadline_ms",
        deserialize_with = "deserialize_non_negative_whole_u64"
    )]
    deadline_ms: u64,
}

impl ScriptArguments {
    fn input_schema() -> Map<String, Value> {
        object_schema(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "script": project_relative_path_schema(
                    "Project-relative GDScript path to check with Godot, optionally res://-prefixed."
                ),
                "deadline_ms": deadline_schema(default_script_deadline_ms())
            },
            "required": ["script"],
        }))
    }
}

impl EngineRequest {
    pub(super) fn parse(
        operation: EngineOperation,
        arguments: Map<String, Value>,
    ) -> Result<Self, OutcomeError> {
        match operation {
            EngineOperation::ImportProject => {
                let parsed: ImportArguments = parse_arguments(arguments)?;
                validate_deadline(parsed.deadline_ms)?;
                for artifact in &parsed.expected_artifacts {
                    validate_project_relative_path(artifact)?;
                }
                if parsed.assets.len() > MAX_IMPORT_ASSETS {
                    return Err(outcome_error(
                        ErrorKind::InvalidInput,
                        format!("`assets` names at most {MAX_IMPORT_ASSETS} assets"),
                    ));
                }
                let mut assets = Vec::with_capacity(parsed.assets.len());
                for asset in &parsed.assets {
                    validate_project_relative_path(asset)?;
                    assets.push(project_relative_path(asset).to_owned());
                }
                Ok(Self::ImportProject {
                    expected_artifacts: parsed.expected_artifacts,
                    assets,
                    deadline_ms: parsed.deadline_ms,
                })
            }
            EngineOperation::SceneCreate
            | EngineOperation::SceneBatch
            | EngineOperation::ResourceCreate
            | EngineOperation::ResourceBatch => authored::parse(
                operation.author_kind().expect("an authored operation"),
                arguments,
            )
            .map(Self::Author),
            EngineOperation::UpdateUid => parse_canonical(arguments).map(Self::UpdateUid),
            EngineOperation::UpdateResource => parse_canonical(arguments).map(Self::UpdateResource),
            EngineOperation::SaveScene => parse_canonical(arguments).map(Self::SaveScene),
            EngineOperation::RunProject => {
                let parsed: RunArguments = parse_arguments(arguments)?;
                validate_deadline(parsed.deadline_ms)?;
                if let Some(main_scene) = &parsed.main_scene {
                    validate_project_relative_path(main_scene)?;
                }
                Ok(Self::RunProject {
                    main_scene: parsed.main_scene,
                    headless: parsed.headless,
                    deadline_ms: parsed.deadline_ms,
                })
            }
            EngineOperation::RunTests => {
                let parsed: TestArguments = parse_arguments(arguments)?;
                validate_deadline(parsed.deadline_ms)?;
                for target in &parsed.selection {
                    validate_project_relative_path(target)?;
                    validate_selection_element(target).map_err(|error| {
                        outcome_error(ErrorKind::InvalidInput, error.to_string())
                    })?;
                }
                Ok(Self::RunTests {
                    selection: parsed.selection,
                    deadline_ms: parsed.deadline_ms,
                })
            }
            EngineOperation::CheckScript => {
                let parsed: ScriptArguments = parse_arguments(arguments)?;
                validate_deadline(parsed.deadline_ms)?;
                validate_project_relative_path(&parsed.script)?;
                Ok(Self::CheckScript {
                    script: parsed.script,
                    deadline_ms: parsed.deadline_ms,
                })
            }
        }
    }
}

fn parse_canonical(arguments: Map<String, Value>) -> Result<CanonicalArguments, OutcomeError> {
    let parsed: CanonicalArguments = parse_arguments(arguments)?;
    validate_project_relative_path(&parsed.target)?;
    Ok(parsed)
}

fn parse_arguments<T: for<'de> Deserialize<'de>>(
    arguments: Map<String, Value>,
) -> Result<T, OutcomeError> {
    if let Some(key) = arguments
        .iter()
        .find_map(|(key, value)| value.is_null().then_some(key))
    {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            format!("invalid engine request: `{key}` must not be null"),
        ));
    }
    serde_json::from_value(Value::Object(arguments)).map_err(|error| {
        outcome_error(
            ErrorKind::InvalidInput,
            format!("invalid engine request: {error}"),
        )
    })
}

fn validate_project_relative_path(path: &str) -> Result<(), OutcomeError> {
    if !is_valid_project_relative_path(path) {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            "path must be non-empty, project-relative without parent traversal, and not option-shaped; it must have at most one res:// prefix, use '/', and contain no residual scheme or control character",
        ));
    }
    Ok(())
}

fn validate_deadline(deadline_ms: u64) -> Result<(), OutcomeError> {
    if deadline_ms == 0 || deadline_ms > MAX_DEADLINE_MS {
        return Err(outcome_error(
            ErrorKind::InvalidInput,
            "deadline_ms must be between 1 and 600000",
        ));
    }
    Ok(())
}

const MAX_DEADLINE_MS: u64 = 600_000;

fn project_relative_path_schema(description: &str) -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "pattern": PROJECT_RELATIVE_PATH_PATTERN,
        "description": description,
    })
}

fn test_selection_path_schema(description: &str) -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "pattern": test_selection_path_pattern(),
        "description": description,
    })
}

fn test_selection_path_pattern() -> String {
    let mut separator_class = String::new();
    for separator in selection_separators() {
        if matches!(separator, '\\' | '[' | ']' | '-' | '^') {
            separator_class.push('\\');
        }
        separator_class.push(separator);
    }
    if separator_class.is_empty() {
        return PROJECT_RELATIVE_PATH_PATTERN.to_owned();
    }
    format!(
        "^(?!.*[{separator_class}]){}",
        PROJECT_RELATIVE_PATH_PATTERN
            .strip_prefix('^')
            .expect("the shared project-relative pattern is anchored")
    )
}

fn deadline_schema(default: u64) -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "maximum": MAX_DEADLINE_MS,
        "default": default,
        "description": "Execution deadline in milliseconds."
    })
}

fn object_schema(value: Value) -> Map<String, Value> {
    value
        .as_object()
        .expect("engine tool schemas are JSON objects")
        .clone()
}

const fn default_true() -> bool {
    true
}

const fn default_false() -> bool {
    false
}

fn default_expected_artifacts() -> Vec<String> {
    Vec::new()
}

fn default_import_assets() -> Vec<String> {
    Vec::new()
}

fn default_selection() -> Vec<String> {
    Vec::new()
}

const fn default_import_deadline_ms() -> u64 {
    60_000
}

const fn default_run_deadline_ms() -> u64 {
    120_000
}

const fn default_test_deadline_ms() -> u64 {
    120_000
}

const fn default_script_deadline_ms() -> u64 {
    30_000
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
