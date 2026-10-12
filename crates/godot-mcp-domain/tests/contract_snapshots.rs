use std::collections::{BTreeMap, BTreeSet};

use godot_mcp_domain::contracts::{
    Aabb, ArtifactContract, ArtifactId, ArtifactKind, ArtifactMetadata, ArtifactOwner,
    ArtifactResourceUri, Basis, ByteRange, CapabilitiesReport, Capability, Color, ComponentHealth,
    ComponentName, Confidence, EffectDisposition, ErrorKind, GodotValue, HealthReport,
    HealthStatus, Matrix4, ObjectReference, OpaqueObjectId, OperationClass, OperationEffect,
    OperationPlane, OutcomeError, Plane, PolicyDecision, PolicyReason, PolicyVerdict, PrincipalId,
    ProjectIdentity, ProjectResource, Projection, Provenance, ProvenanceSource, Quaternion,
    RangeSemantics, Rect2, Rect2i, ResourceReference, ResourceUri, SchedulerMode, SchedulerState,
    SessionId, StaticResult, Transform2D, Transform3D, Vector2, Vector2i, Vector3, Vector3i,
    Vector4, Vector4i, canonical_contract_schema,
};
use godot_mcp_domain::manifests::{ProfileManifest, canonical_manifest, historical_manifest_v0_1};
use godot_mcp_domain::objid::{LeaseKey, StableObjectId};
use godot_mcp_domain::{CONTRACT_VERSION, CapabilityProfile};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const CONTRACT_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schemas/domain/contract-v1.schema.json"
));
const CORE_MANIFEST: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schemas/domain/profiles/core.json"
));
const EDITOR_MANIFEST: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schemas/domain/profiles/editor.json"
));
const RUNTIME_MANIFEST: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schemas/domain/profiles/runtime.json"
));
const FULL_MANIFEST: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schemas/domain/profiles/full.json"
));
const CORE_MANIFEST_V0_2: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schemas/domain/profiles/v0.2/core.json"
));
const EDITOR_MANIFEST_V0_2: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schemas/domain/profiles/v0.2/editor.json"
));
const RUNTIME_MANIFEST_V0_2: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schemas/domain/profiles/v0.2/runtime.json"
));
const FULL_MANIFEST_V0_2: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schemas/domain/profiles/v0.2/full.json"
));

fn roundtrip<T>(value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let bytes = serde_json::to_vec(value).expect("contract serializes");
    let decoded = serde_json::from_slice::<T>(&bytes).expect("contract deserializes");
    assert_eq!(&decoded, value);
}

fn project() -> ProjectIdentity {
    ProjectIdentity::from_lease_key(
        LeaseKey {
            root: StableObjectId::Unix { dev: 7, ino: 11 },
            project_file: StableObjectId::Unix { dev: 7, ino: 12 },
        },
        4,
    )
}

fn provenance() -> Provenance {
    Provenance::new(
        ProvenanceSource::StaticSyntax,
        Confidence::Exact,
        "res://player.gd:Player",
        4,
    )
    .expect("static syntax may be exact")
}

fn identity_matrix() -> Matrix4 {
    Matrix4 {
        rows: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
    }
}

fn typed_values() -> Vec<GodotValue> {
    let vector2 = Vector2 { x: 1.0, y: 2.0 };
    let vector3 = Vector3 {
        x: 1.0,
        y: 2.0,
        z: 3.0,
    };
    let color = Color {
        red: 0.1,
        green: 0.2,
        blue: 0.3,
        alpha: 1.0,
    };
    let basis = Basis {
        x: Vector3 {
            x: 1.0,
            y: 0.0,
            z: 0.0,
        },
        y: Vector3 {
            x: 0.0,
            y: 1.0,
            z: 0.0,
        },
        z: Vector3 {
            x: 0.0,
            y: 0.0,
            z: 1.0,
        },
    };
    let mut dictionary = BTreeMap::new();
    dictionary.insert("enabled".to_owned(), GodotValue::Bool(true));

    vec![
        GodotValue::Null,
        GodotValue::Bool(true),
        GodotValue::Integer(42),
        GodotValue::Float(1.5),
        GodotValue::String("player".to_owned()),
        GodotValue::StringName("ready".to_owned()),
        GodotValue::NodePath("Root/Player".to_owned()),
        GodotValue::Vector2(vector2.clone()),
        GodotValue::Vector2i(Vector2i { x: 1, y: 2 }),
        GodotValue::Rect2(Rect2 {
            position: vector2.clone(),
            size: Vector2 { x: 3.0, y: 4.0 },
        }),
        GodotValue::Rect2i(Rect2i {
            position: Vector2i { x: 1, y: 2 },
            size: Vector2i { x: 3, y: 4 },
        }),
        GodotValue::Vector3(vector3.clone()),
        GodotValue::Vector3i(Vector3i { x: 1, y: 2, z: 3 }),
        GodotValue::Transform2D(Transform2D {
            x: Vector2 { x: 1.0, y: 0.0 },
            y: Vector2 { x: 0.0, y: 1.0 },
            origin: vector2.clone(),
        }),
        GodotValue::Vector4(Vector4 {
            x: 1.0,
            y: 2.0,
            z: 3.0,
            w: 4.0,
        }),
        GodotValue::Vector4i(Vector4i {
            x: 1,
            y: 2,
            z: 3,
            w: 4,
        }),
        GodotValue::Plane(Plane {
            normal: vector3.clone(),
            distance: 4.0,
        }),
        GodotValue::Quaternion(Quaternion {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w: 1.0,
        }),
        GodotValue::Aabb(Aabb {
            position: vector3.clone(),
            size: Vector3 {
                x: 4.0,
                y: 5.0,
                z: 6.0,
            },
        }),
        GodotValue::Basis(basis.clone()),
        GodotValue::Transform3D(Transform3D {
            basis: basis.clone(),
            origin: vector3.clone(),
        }),
        GodotValue::Projection(Projection {
            matrix: identity_matrix(),
        }),
        GodotValue::Color(color.clone()),
        GodotValue::Array(vec![GodotValue::Integer(1), GodotValue::Integer(2)]),
        GodotValue::Dictionary(dictionary),
        GodotValue::PackedByteArray(vec![0, 1, 2]),
        GodotValue::PackedInt32Array(vec![1, 2]),
        GodotValue::PackedInt64Array(vec![3, 4]),
        GodotValue::PackedFloat32Array(vec![1.0, 2.0]),
        GodotValue::PackedFloat64Array(vec![3.0, 4.0]),
        GodotValue::PackedStringArray(vec!["a".to_owned(), "b".to_owned()]),
        GodotValue::PackedVector2Array(vec![vector2]),
        GodotValue::PackedVector3Array(vec![vector3]),
        GodotValue::PackedColorArray(vec![color]),
        GodotValue::Resource(ResourceReference {
            resource: ProjectResource::parse("res://materials/player.tres")
                .expect("project resource"),
            resource_type: "StandardMaterial3D".to_owned(),
            uid: Some("uid://player-material".to_owned()),
        }),
        GodotValue::Object(ObjectReference {
            id: OpaqueObjectId::parse("object_0123456789abcdef").expect("object id"),
            class_name: "CharacterBody3D".to_owned(),
        }),
    ]
}

fn committed_manifest(text: &str) -> ProfileManifest {
    serde_json::from_str(text).expect("committed profile manifest is valid")
}

fn union<T: Ord + Clone>(sets: impl IntoIterator<Item = BTreeSet<T>>) -> BTreeSet<T> {
    sets.into_iter().flatten().collect()
}

#[test]
fn profile_algebra_and_dto_roundtrip() {
    assert_eq!(CONTRACT_VERSION, "0.2.0");

    let project = project();
    let provenance = provenance();
    let values = typed_values();
    let static_result = StaticResult::new(project.clone(), values.clone(), provenance.clone());
    let artifact_id = ArtifactId::parse(
        "artifact_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .expect("artifact id");
    let artifact_uri = ArtifactResourceUri::for_artifact(&artifact_id);
    let resource_uri = ResourceUri::parse("godot-mcp://resources/0123456789abcdef0123456789abcdef")
        .expect("resource URI");
    let principal = PrincipalId::parse("principal_0123456789abcdef").expect("principal id");
    let session = SessionId::parse("session_0123456789abcdef").expect("session id");

    let effect = OperationEffect::new(
        "scene.node.update",
        OperationPlane::Editor,
        OperationClass::EditorMutation,
        EffectDisposition::Committed,
        BTreeSet::from([resource_uri]),
        BTreeSet::from([artifact_id.clone()]),
    );
    let health = HealthReport::new(
        project.clone(),
        HealthStatus::Degraded,
        vec![
            ComponentHealth::new(ComponentName::Project, HealthStatus::Ready, None),
            ComponentHealth::new(
                ComponentName::Editor,
                HealthStatus::Unavailable,
                Some("editor is optional".to_owned()),
            ),
        ],
        1_722_000_000_000,
    );
    let capabilities = CapabilitiesReport::new(
        project.clone(),
        canonical_manifest(CapabilityProfile::Full),
        BTreeSet::from([
            Capability::Read,
            Capability::ProjectWrite,
            Capability::EditorMutate,
            Capability::RuntimeExecute,
            Capability::ExportDestructive,
        ]),
        provenance.clone(),
    );
    let scheduler = SchedulerState::new(
        project.clone(),
        SchedulerMode::Accepting,
        2,
        1,
        8,
        32,
        1_722_000_000_001,
    )
    .expect("valid scheduler counters");
    let policy = PolicyDecision::new(
        "decision_0123456789abcdef",
        project.clone(),
        Some(principal.clone()),
        Some(session.clone()),
        Capability::EditorMutate,
        PolicyVerdict::Allow,
        PolicyReason::Granted,
    )
    .expect("valid policy decision");
    let ranges = RangeSemantics::new(1_048_576, 65_536, true).expect("valid byte ranges");
    let requested_range = ByteRange::new(1_024, 2_047).expect("valid inclusive byte range");
    assert_eq!(
        ranges
            .resolve(Some(requested_range.clone()))
            .expect("bounded byte range"),
        requested_range
    );
    roundtrip(&ranges);
    roundtrip(&requested_range);
    let artifact = ArtifactContract::new(
        artifact_id.clone(),
        artifact_uri.clone(),
        project.clone(),
        Some(ArtifactOwner {
            principal: Some(principal),
            session: Some(session),
        }),
        ArtifactMetadata {
            kind: ArtifactKind::Screenshot,
            media_type: "image/png".to_owned(),
            byte_length: 1_048_576,
            sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            created_at_unix_ms: 1_722_000_000_000,
        },
        1_722_003_600_000,
        ranges,
    )
    .expect("valid artifact contract");
    let outcome_error = OutcomeError::new(
        ErrorKind::Conflict,
        "project generation changed",
        false,
        BTreeMap::from([("observed_generation".to_owned(), GodotValue::Integer(5))]),
    );

    roundtrip(&project);
    roundtrip(&provenance);
    roundtrip(&static_result);
    roundtrip(&effect);
    roundtrip(&health);
    roundtrip(&capabilities);
    roundtrip(&scheduler);
    roundtrip(&policy);
    roundtrip(&artifact);
    roundtrip(&outcome_error);
    for value in &values {
        roundtrip(value);
    }

    let error_codes = [
        ErrorKind::InvalidInput,
        ErrorKind::NotFound,
        ErrorKind::Conflict,
        ErrorKind::Unsupported,
        ErrorKind::PermissionDenied,
        ErrorKind::Timeout,
        ErrorKind::Cancelled,
        ErrorKind::Unavailable,
        ErrorKind::Uncertain,
        ErrorKind::Internal,
    ]
    .map(ErrorKind::code);
    assert_eq!(
        error_codes,
        [
            "invalid_input",
            "not_found",
            "conflict",
            "unsupported",
            "permission_denied",
            "timeout",
            "cancelled",
            "unavailable",
            "uncertain",
            "internal",
        ]
    );

    let generated_schema = serde_json::to_value(canonical_contract_schema())
        .expect("generated contract schema serializes");
    let committed_schema: Value =
        serde_json::from_str(CONTRACT_SCHEMA).expect("committed contract schema is valid");
    assert_eq!(generated_schema, committed_schema, "contract schema drift");
    assert_eq!(
        generated_schema["$schema"],
        json!("https://json-schema.org/draft/2020-12/schema")
    );

    // The frozen version 0.1 artifacts stay byte-verifiable against the frozen
    // first-release manifests; contract 0.2 discovery is pinned to its own
    // committed artifacts, derived from the implemented catalog.
    for (profile, committed) in [
        (CapabilityProfile::Core, CORE_MANIFEST),
        (CapabilityProfile::Editor, EDITOR_MANIFEST),
        (CapabilityProfile::Runtime, RUNTIME_MANIFEST),
        (CapabilityProfile::Full, FULL_MANIFEST),
    ] {
        assert_eq!(
            historical_manifest_v0_1(profile),
            committed_manifest(committed)
        );
    }
    let core = canonical_manifest(CapabilityProfile::Core);
    let editor = canonical_manifest(CapabilityProfile::Editor);
    let runtime = canonical_manifest(CapabilityProfile::Runtime);
    let full = canonical_manifest(CapabilityProfile::Full);
    assert_eq!(core, committed_manifest(CORE_MANIFEST_V0_2));
    assert_eq!(editor, committed_manifest(EDITOR_MANIFEST_V0_2));
    assert_eq!(runtime, committed_manifest(RUNTIME_MANIFEST_V0_2));
    assert_eq!(full, committed_manifest(FULL_MANIFEST_V0_2));
    assert_eq!(
        full.capabilities,
        union([
            core.capabilities.clone(),
            editor.capabilities.clone(),
            runtime.capabilities.clone(),
        ])
    );
    assert_eq!(
        full.tools,
        union([
            core.tools.clone(),
            editor.tools.clone(),
            runtime.tools.clone()
        ])
    );
    assert_eq!(
        full.resources,
        union([
            core.resources.clone(),
            editor.resources.clone(),
            runtime.resources.clone(),
        ])
    );

    for document in [
        serde_json::to_string(&static_result).expect("static result JSON"),
        serde_json::to_string(&artifact).expect("artifact JSON"),
        serde_json::to_string(&full).expect("manifest JSON"),
        CONTRACT_SCHEMA.to_owned(),
        CORE_MANIFEST.to_owned(),
        EDITOR_MANIFEST.to_owned(),
        RUNTIME_MANIFEST.to_owned(),
        FULL_MANIFEST.to_owned(),
        CORE_MANIFEST_V0_2.to_owned(),
        EDITOR_MANIFEST_V0_2.to_owned(),
        RUNTIME_MANIFEST_V0_2.to_owned(),
        FULL_MANIFEST_V0_2.to_owned(),
    ] {
        assert!(!document.contains("unsafe_script"));
        assert!(!document.contains("storage_path"));
        assert!(!document.contains("filesystem_path"));
        assert!(!document.contains("plugin"));
        assert!(!document.contains("transport"));
    }
    assert!(!serde_json::to_string(&artifact).unwrap().contains("/tmp/"));
    let mut artifact_with_path = serde_json::to_value(&artifact).expect("artifact JSON value");
    artifact_with_path
        .as_object_mut()
        .expect("artifact object")
        .insert("storage_path".to_owned(), json!("/tmp/private.png"));
    assert!(serde_json::from_value::<ArtifactContract>(artifact_with_path).is_err());
    assert_eq!(artifact.resource_uri(), &artifact_uri);
    assert_eq!(artifact.artifact_id(), &artifact_id);
}

#[test]
fn provenance_invariant_rejects_false_exact() {
    let error = Provenance::new(
        ProvenanceSource::DynamicReference,
        Confidence::Exact,
        "dynamic get_node(reference_name)",
        9,
    )
    .expect_err("an inferred dynamic reference cannot be labeled exact");
    assert_eq!(error.kind(), ErrorKind::Uncertain);

    let fixture = json!({
        "schema_version": CONTRACT_VERSION,
        "source": "dynamic_reference",
        "confidence": "exact",
        "subject": "dynamic get_node(reference_name)",
        "project_generation": 9
    });
    assert!(
        serde_json::from_value::<Provenance>(fixture).is_err(),
        "deserialization must enforce the same production invariant"
    );

    let honest = Provenance::new(
        ProvenanceSource::DynamicReference,
        Confidence::Inferred,
        "dynamic get_node(reference_name)",
        9,
    )
    .expect("dynamic provenance may be explicitly inferred");
    roundtrip(&honest);
}
