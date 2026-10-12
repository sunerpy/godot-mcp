//! The one guarded path from a launch journal to a live adapter session.
//!
//! # The order is the safety property
//!
//! [`EndpointRegistry::connect`] performs its checks in this exact order, and the
//! order is what makes a refusal cheap and effect-free:
//!
//! | step | why it is where it is |
//! | --- | --- |
//! | 1. ownership | an endpoint no launch of ours names is refused before anything else happens |
//! | 2. tuple verification | PID, start time, binary, root URI, endpoint, generation — re-run on every reconnect |
//! | 3. client admission | the engine's 8-client ceiling, refused before any permit is spent |
//! | 4. effect permit | Todo 6's gateway authorises the exchange |
//! | 5. dial | only now is a socket opened |
//!
//! Steps 1–3 consult **no** gateway, so a spoofed endpoint, a stale receipt or a
//! ninth client cannot consume an effect permit. Steps 4–5 cannot be reached without
//! having passed 1–3, because the only way to obtain the
//! [`OwnedEndpoint`](crate::adapters::receipt::OwnedEndpoint) that
//! [`LspAdapter::open`]/[`DapAdapter::open`] require is step 2.
//!
//! # Reconnects re-verify, always
//!
//! [`EndpointRegistry::reconnect`] is [`EndpointRegistry::connect`] with an
//! incremented reconnect counter, and nothing else. There is deliberately no cheaper
//! path: reconnecting to the same port is exactly when PID reuse becomes
//! indistinguishable from continuity, so the check that catches it must run again.

use std::net::SocketAddr;
use std::time::Duration;

use crate::adapters::admission::{ClientAdmission, ClientTicket};
use crate::adapters::channel::TcpChannel;
use crate::adapters::dap::{DapAdapter, DapSession};
use crate::adapters::error::AdapterError;
use crate::adapters::lsp::{LspAdapter, LspSession};
use crate::adapters::receipt::{
    EditorEndpoint, EndpointKind, EndpointReceipt, ObservedEndpoint, OwnedEndpoint, receipt_for,
};
use crate::adapters::tools::{EffectBoundary, EffectGate, EffectRequest, ToolPermit, permit_for};
use crate::supervision::journal::{LaunchJournal, OperationFamily};

/// The canonical tool name an LSP/DAP exchange is journaled under.
#[must_use]
pub const fn exchange_tool_name(kind: EndpointKind) -> &'static str {
    match kind {
        EndpointKind::LanguageServer => "adapters.lsp_exchange",
        EndpointKind::DebugAdapter => "adapters.dap_exchange",
    }
}

/// The effect request one adapter exchange needs authority for.
#[must_use]
pub fn exchange_request(
    kind: EndpointKind,
    project_object_identity: &str,
    project_generation: u64,
    detail: &str,
) -> EffectRequest {
    EffectRequest {
        tool_name: exchange_tool_name(kind).to_owned(),
        boundary: EffectBoundary::LspDap,
        // An adapter exchange drives a live editor session, not a project file.
        operation_family: OperationFamily::EditorMutation,
        project_object_identity: project_object_identity.to_owned(),
        project_generation,
        detail: detail.to_owned(),
    }
}

/// One verified, admitted, permitted and dialled endpoint.
///
/// The `Debug` rendering deliberately shows the verified tuple and the permit, never
/// the live socket: a diagnostic that printed channel state could leak payload bytes.
pub struct ConnectedEndpoint {
    owned: OwnedEndpoint,
    permit: ToolPermit,
    ticket: ClientTicket,
    channel: TcpChannel,
}

impl std::fmt::Debug for ConnectedEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectedEndpoint")
            .field("endpoint", &self.owned.endpoint().authority())
            .field("kind", &self.owned.kind().as_str())
            .field("launch_transaction_id", &self.owned.launch_transaction_id())
            .field("permit_transaction_id", &self.permit.transaction_id())
            .field("client_slot", &self.ticket.kind().as_str())
            .finish_non_exhaustive()
    }
}

impl ConnectedEndpoint {
    /// The verified endpoint token.
    #[must_use]
    pub const fn owned(&self) -> &OwnedEndpoint {
        &self.owned
    }

    /// The permit this exchange runs under.
    #[must_use]
    pub const fn permit(&self) -> &ToolPermit {
        &self.permit
    }

    /// The admitted client slot.
    #[must_use]
    pub const fn ticket(&self) -> &ClientTicket {
        &self.ticket
    }

    /// Open a language-server session on this connection.
    ///
    /// # Errors
    /// Returns [`AdapterError::Unsupported`] when this endpoint is the debug adapter.
    pub fn lsp(&mut self, deadline: Duration) -> Result<LspSession<'_>, AdapterError> {
        LspAdapter::open(&self.owned, &mut self.channel, deadline)
    }

    /// Open a debug-adapter session on this connection.
    ///
    /// # Errors
    /// Returns [`AdapterError::Unsupported`] when this endpoint is the language server.
    pub fn dap(&mut self, deadline: Duration) -> Result<DapSession<'_>, AdapterError> {
        DapAdapter::open(&self.owned, &mut self.channel, deadline)
    }
}

/// The bounded gateway from a launch journal to an adapter session.
pub struct EndpointRegistry<'j> {
    journal: &'j dyn LaunchJournal,
    admission: ClientAdmission,
    project_object_identity: String,
    project_root_uri: String,
}

impl<'j> EndpointRegistry<'j> {
    /// Bind a registry to one journal, one client-admission registry and one held
    /// project.
    #[must_use]
    pub fn new(
        journal: &'j dyn LaunchJournal,
        admission: ClientAdmission,
        project_object_identity: &str,
        project_root_uri: &str,
    ) -> Self {
        Self {
            journal,
            admission,
            project_object_identity: project_object_identity.to_owned(),
            project_root_uri: project_root_uri.to_owned(),
        }
    }

    /// The client-admission registry, so a caller can observe live counts.
    #[must_use]
    pub const fn admission(&self) -> &ClientAdmission {
        &self.admission
    }

    /// The ownership record for `endpoint`, or the refusal that applies.
    ///
    /// # Errors
    /// Returns [`AdapterError::Ownership`] when no committed launch of ours asked an
    /// editor to host `kind` on that authority.
    pub fn owned(
        &self,
        kind: EndpointKind,
        endpoint: &EditorEndpoint,
    ) -> Result<EndpointReceipt, AdapterError> {
        let committed = self.journal.committed_launches()?;
        Ok(receipt_for(
            &committed,
            kind,
            &self.project_root_uri,
            endpoint,
        )?)
    }

    /// Verify, admit, obtain a permit and dial — in that order.
    ///
    /// # Errors
    /// Returns [`AdapterError::Ownership`] for an unowned or stale endpoint,
    /// [`AdapterError::Admission`] at the client ceiling,
    /// [`AdapterError::PermitRefused`] when the gateway refuses, and
    /// [`AdapterError::Unavailable`] when nothing is listening. The first three are
    /// reached **before** any socket is opened, and the first two before the gateway
    /// is consulted at all.
    pub fn connect(
        &self,
        gate: &dyn EffectGate,
        kind: EndpointKind,
        endpoint: &EditorEndpoint,
        observed: &ObservedEndpoint,
        reconnect: u32,
        timeout: Duration,
    ) -> Result<ConnectedEndpoint, AdapterError> {
        let receipt = self.owned(kind, endpoint)?;
        let owned = receipt.verify(observed, reconnect)?;
        let ticket = self.admission.admit(kind)?;
        let permit = permit_for(
            gate,
            &exchange_request(
                kind,
                &self.project_object_identity,
                owned.project_generation(),
                &format!(
                    "{} exchange on {} owned by launch {}",
                    kind.as_str(),
                    owned.endpoint().authority(),
                    owned.launch_transaction_id()
                ),
            ),
        )?;
        let address: SocketAddr = format!("{}:{}", endpoint.host, endpoint.port)
            .parse()
            .map_err(|error| {
                AdapterError::unavailable(&format!(
                    "{} is not a dialable address: {error}",
                    endpoint.authority()
                ))
            })?;
        let channel = TcpChannel::connect(address, timeout).map_err(|error| {
            AdapterError::unavailable(&format!(
                "no editor is hosting {} on {}: {error}",
                kind.as_str(),
                endpoint.authority()
            ))
        })?;
        Ok(ConnectedEndpoint {
            owned,
            permit,
            ticket,
            channel,
        })
    }

    /// Reconnect, re-running the full ownership tuple check.
    ///
    /// # Errors
    /// Identical to [`EndpointRegistry::connect`]; there is no cheaper reconnect path,
    /// because a reconnect is exactly when PID reuse looks like continuity.
    pub fn reconnect(
        &self,
        gate: &dyn EffectGate,
        kind: EndpointKind,
        endpoint: &EditorEndpoint,
        observed: &ObservedEndpoint,
        previous: u32,
        timeout: Duration,
    ) -> Result<ConnectedEndpoint, AdapterError> {
        self.connect(
            gate,
            kind,
            endpoint,
            observed,
            previous.saturating_add(1),
            timeout,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exchange_is_an_lsp_dap_effect_on_the_live_editor() {
        let request = exchange_request(EndpointKind::DebugAdapter, "project_abc", 18, "stackTrace");
        assert_eq!(request.boundary, EffectBoundary::LspDap);
        assert_eq!(request.operation_family, OperationFamily::EditorMutation);
        assert_eq!(request.tool_name, "adapters.dap_exchange");
    }

    #[test]
    fn the_two_protocols_have_distinct_tool_names() {
        assert_ne!(
            exchange_tool_name(EndpointKind::LanguageServer),
            exchange_tool_name(EndpointKind::DebugAdapter)
        );
    }
}
