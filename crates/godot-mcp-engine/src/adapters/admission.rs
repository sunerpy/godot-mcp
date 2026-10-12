//! The 8-client ceiling, refused without dispatching an effect.
//!
//! # The engine's number, and the engine's failure mode
//!
//! Both Godot protocol servers cap concurrent clients at eight:
//! `#define LSP_MAX_CLIENTS 8` in `gdscript_language_protocol.h` and
//! `#define DAP_MAX_CLIENTS 8` in `debug_adapter_protocol.h`. The engine's own
//! enforcement is late — `on_client_connected` runs
//! `ERR_FAIL_COND_V_MSG(clients.size() >= LSP_MAX_CLIENTS, …)` *after*
//! `take_connection()` has already accepted the socket, so a ninth client is
//! accepted and then dropped. From the client's side that is indistinguishable from
//! a connection that succeeded and then failed for an unknown reason.
//!
//! This module refuses the ninth **before** anything is dialled and before any
//! effect permit is consumed, so the caller gets `unavailable` with a count instead
//! of a mysterious disconnect. That ordering is the point: a refusal that happens
//! after an effect is dispatched has already spent the effect.
//!
//! # Why a ticket type instead of a counter
//!
//! [`ClientTicket`] releases its slot on drop, so a slot cannot leak when an adapter
//! session returns early through `?`. The registry is per-endpoint-kind because the
//! engine's two servers count separately.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::adapters::receipt::EndpointKind;

/// Hard maximum concurrent clients per protocol.
///
/// Exactly the engine's own `LSP_MAX_CLIENTS` / `DAP_MAX_CLIENTS`.
pub const MAX_CLIENTS: usize = 8;

/// Why a client could not be admitted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionError {
    /// The protocol already has [`MAX_CLIENTS`] live clients.
    ///
    /// Refused before dialling and before any effect is dispatched. The engine would
    /// accept-then-drop the connection, which is worse than an honest refusal.
    #[error(
        "{kind} already has {live} of {MAX_CLIENTS} clients, which is the engine's own \
         {kind}_MAX_CLIENTS ceiling; the connection was refused before it was dialled"
    )]
    AtCapacity {
        /// Which protocol.
        kind: &'static str,
        /// How many clients are live.
        live: usize,
    },

    /// The registry's lock was poisoned by a panic in another holder.
    #[error("the client registry is poisoned and can no longer bound concurrency")]
    Poisoned,
}

impl AdmissionError {
    /// The stable domain error kind.
    #[must_use]
    pub const fn error_kind(&self) -> godot_mcp_domain::ErrorKind {
        use godot_mcp_domain::ErrorKind;
        match self {
            Self::AtCapacity { .. } => ErrorKind::Unavailable,
            Self::Poisoned => ErrorKind::Internal,
        }
    }
}

/// Shared per-protocol live-client counts.
#[derive(Debug, Default)]
struct Counts {
    live: BTreeMap<&'static str, usize>,
}

/// Bounds concurrent adapter clients per protocol.
#[derive(Debug, Clone, Default)]
pub struct ClientAdmission {
    counts: Arc<Mutex<Counts>>,
}

impl ClientAdmission {
    /// A fresh registry with no live clients.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many clients of `kind` are live.
    ///
    /// # Errors
    /// Returns [`AdmissionError::Poisoned`] when the registry lock is poisoned.
    pub fn live(&self, kind: EndpointKind) -> Result<usize, AdmissionError> {
        let counts = self
            .counts
            .lock()
            .map_err(|_poisoned| AdmissionError::Poisoned)?;
        Ok(counts.live.get(kind.as_str()).copied().unwrap_or(0))
    }

    /// Admit one client of `kind`, or refuse at the ceiling.
    ///
    /// # Errors
    /// Returns [`AdmissionError::AtCapacity`] when [`MAX_CLIENTS`] are already live —
    /// **before** anything is dialled and before any effect permit is consumed — or
    /// [`AdmissionError::Poisoned`] when the registry lock is poisoned.
    pub fn admit(&self, kind: EndpointKind) -> Result<ClientTicket, AdmissionError> {
        let mut counts = self
            .counts
            .lock()
            .map_err(|_poisoned| AdmissionError::Poisoned)?;
        let slot = counts.live.entry(kind.as_str()).or_insert(0);
        if *slot >= MAX_CLIENTS {
            return Err(AdmissionError::AtCapacity {
                kind: kind.as_str(),
                live: *slot,
            });
        }
        *slot += 1;
        Ok(ClientTicket {
            counts: Arc::clone(&self.counts),
            kind,
            released: false,
        })
    }
}

/// One admitted client's slot. Releases on drop.
#[derive(Debug)]
pub struct ClientTicket {
    counts: Arc<Mutex<Counts>>,
    kind: EndpointKind,
    released: bool,
}

impl ClientTicket {
    /// Which protocol this slot belongs to.
    #[must_use]
    pub const fn kind(&self) -> EndpointKind {
        self.kind
    }

    /// Release the slot explicitly, so a caller can observe the release instead of
    /// leaving it to a destructor.
    pub fn release(mut self) {
        self.release_now();
    }

    fn release_now(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        if let Ok(mut counts) = self.counts.lock()
            && let Some(slot) = counts.live.get_mut(self.kind.as_str())
        {
            *slot = slot.saturating_sub(1);
        }
    }
}

impl Drop for ClientTicket {
    fn drop(&mut self) {
        self.release_now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ninth_client_is_refused_and_the_eight_survivors_are_untouched() {
        let admission = ClientAdmission::new();
        let mut tickets = Vec::new();
        for _ in 0..MAX_CLIENTS {
            tickets.push(
                admission
                    .admit(EndpointKind::LanguageServer)
                    .expect("within the ceiling"),
            );
        }
        assert_eq!(
            admission.live(EndpointKind::LanguageServer),
            Ok(MAX_CLIENTS)
        );
        let refused = admission
            .admit(EndpointKind::LanguageServer)
            .expect_err("the ninth");
        assert_eq!(
            refused,
            AdmissionError::AtCapacity {
                kind: "lsp",
                live: MAX_CLIENTS
            }
        );
        assert_eq!(
            refused.error_kind(),
            godot_mcp_domain::ErrorKind::Unavailable
        );
        assert_eq!(
            admission.live(EndpointKind::LanguageServer),
            Ok(MAX_CLIENTS),
            "a refusal never disturbs the admitted clients"
        );
        assert_eq!(tickets.len(), MAX_CLIENTS);
    }

    #[test]
    fn the_two_protocols_are_counted_separately_like_the_engines_two_servers() {
        let admission = ClientAdmission::new();
        let mut lsp = Vec::new();
        for _ in 0..MAX_CLIENTS {
            lsp.push(admission.admit(EndpointKind::LanguageServer).expect("lsp"));
        }
        assert!(admission.admit(EndpointKind::LanguageServer).is_err());
        let dap = admission
            .admit(EndpointKind::DebugAdapter)
            .expect("the DAP server has its own ceiling");
        assert_eq!(dap.kind(), EndpointKind::DebugAdapter);
        assert_eq!(admission.live(EndpointKind::DebugAdapter), Ok(1));
    }

    #[test]
    fn a_released_slot_is_reusable_and_release_is_idempotent_on_drop() {
        let admission = ClientAdmission::new();
        let mut tickets: Vec<ClientTicket> = (0..MAX_CLIENTS)
            .map(|_| {
                admission
                    .admit(EndpointKind::DebugAdapter)
                    .expect("within the ceiling")
            })
            .collect();
        assert!(admission.admit(EndpointKind::DebugAdapter).is_err());
        tickets.pop().expect("a ticket").release();
        assert_eq!(admission.live(EndpointKind::DebugAdapter), Ok(7));
        let replacement = admission
            .admit(EndpointKind::DebugAdapter)
            .expect("the freed slot");
        drop(replacement);
        assert_eq!(admission.live(EndpointKind::DebugAdapter), Ok(7));
    }

    #[test]
    fn an_early_return_cannot_leak_a_slot() {
        let admission = ClientAdmission::new();
        fn fails_early(admission: &ClientAdmission) -> Result<(), AdmissionError> {
            let _ticket = admission.admit(EndpointKind::LanguageServer)?;
            Err(AdmissionError::Poisoned)
        }
        for _ in 0..64 {
            assert!(fails_early(&admission).is_err());
        }
        assert_eq!(
            admission.live(EndpointKind::LanguageServer),
            Ok(0),
            "every early return released its slot on drop"
        );
    }
}
