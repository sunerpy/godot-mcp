//! Canonical aggregate JSON schema generation.

use schemars::Schema;
use serde::{Deserialize, Serialize};

use super::{
    ArtifactContract, CapabilitiesReport, GodotValue, HealthReport, OperationEffect, OutcomeError,
    PolicyDecision, ProjectIdentity, Provenance, SchedulerState, StaticResult,
};

/// Aggregate type used solely to generate the committed public contract schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ContractSchemaCatalog {
    project_identity: ProjectIdentity,
    provenance: Provenance,
    typed_value: GodotValue,
    static_result: StaticResult<Vec<GodotValue>>,
    operation_effect: OperationEffect,
    health: HealthReport,
    capabilities: CapabilitiesReport,
    scheduler: SchedulerState,
    policy: PolicyDecision,
    artifact: ArtifactContract,
    error: OutcomeError,
}

/// Generate the canonical Draft 2020-12 domain schema.
#[must_use]
pub fn canonical_contract_schema() -> Schema {
    let mut settings = schemars::generate::SchemaSettings::draft2020_12();
    settings.inline_subschemas = false;
    settings
        .into_generator()
        .into_root_schema_for::<ContractSchemaCatalog>()
}
