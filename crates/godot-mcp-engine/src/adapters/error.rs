//! The closed failure set for the LSP/DAP adapters and the engine tools.
//!
//! Two mappings carry most of the weight, and both are deliberate:
//!
//! * [`AdapterError::Unsupported`] is `unsupported`, never a fabricated success.
//!   Godot's DAP dispatcher prefixes `"req_"` onto the incoming command and checks
//!   `parser->has_method(command)`; an unknown command falls straight through the
//!   `if` and **no response is written at all**. A client that "tried anyway" would
//!   therefore hang until its own timeout and then report a transport failure for
//!   what is really a missing feature. So an unimplemented method is refused here,
//!   before the frame is sent, with the exact reason.
//! * [`AdapterError::Unavailable`] is `unavailable` — the protocol is real and
//!   implemented, but nothing is hosting it right now (no editor running, connection
//!   refused, client ceiling reached). That is a retryable condition and is kept
//!   distinct from `unsupported`, which is not.

use godot_mcp_domain::ErrorKind;

use crate::adapters::admission::AdmissionError;
use crate::adapters::codec::CodecError;
use crate::adapters::receipt::OwnershipFailure;
use crate::supervision::journal::JournalError;
use crate::validation::runner::RunError;

/// An adapter or engine-tool failure.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    /// The endpoint is not owned by this server, or no longer matches its receipt.
    #[error(transparent)]
    Ownership(#[from] OwnershipFailure),

    /// The client ceiling was reached, or the registry is unusable.
    #[error(transparent)]
    Admission(#[from] AdmissionError),

    /// A frame could not be read or written.
    #[error(transparent)]
    Codec(#[from] CodecError),

    /// The method is not implemented by the engine at all.
    ///
    /// Never presented as a success and never sent on the wire: Godot's DAP drops an
    /// unknown command silently, so sending it would produce a hang rather than an
    /// error.
    #[error("{method} is not implemented by this Godot build: {detail}")]
    Unsupported {
        /// The refused method or request name.
        method: String,
        /// Exactly what the engine does (or fails to do) instead.
        detail: String,
    },

    /// The protocol exists but nothing is hosting it right now.
    #[error("{detail}")]
    Unavailable {
        /// What was attempted and what came back.
        detail: String,
    },

    /// The peer answered with a JSON-RPC / DAP error.
    #[error("{method} failed on the engine side: {message}")]
    PeerError {
        /// The method that was called.
        method: String,
        /// The engine's own code, when it sent one.
        code: Option<i64>,
        /// The engine's own message.
        message: String,
    },

    /// The peer's answer was not the shape the protocol requires.
    #[error("{method} returned an unusable payload: {detail}")]
    MalformedResponse {
        /// The method that was called.
        method: String,
        /// What was wrong with the payload.
        detail: String,
    },

    /// The exchange exceeded its deadline.
    ///
    /// Godot's DAP additionally enforces its own per-message `request_timeout`
    /// (5000 ms by default) and answers `DAP::ErrorType::TIMEOUT` before dispatch, so
    /// a deadline longer than that cannot be relied on.
    #[error("{method} exceeded its {millis} ms deadline")]
    Timeout {
        /// The method that was called.
        method: String,
        /// The deadline in milliseconds.
        millis: u64,
    },

    /// The caller cancelled the exchange.
    #[error("{method} was cancelled by the caller before it completed")]
    Cancelled {
        /// The method that was cancelled.
        method: String,
    },

    /// An effect permit was absent or refused, so nothing was dispatched.
    #[error("no effect permit authorises this operation: {detail}")]
    PermitRefused {
        /// Why the gateway refused.
        detail: String,
    },

    /// The engine could not be launched or contained.
    #[error(transparent)]
    Launch(#[from] RunError),

    /// The durable journal failed.
    #[error(transparent)]
    Journal(#[from] JournalError),
}

impl AdapterError {
    /// Map to the stable domain error taxonomy frozen by Todo 3.
    #[must_use]
    pub fn error_kind(&self) -> ErrorKind {
        match self {
            Self::Ownership(failure) => failure.error_kind(),
            Self::Admission(error) => error.error_kind(),
            // A framing failure — oversize frame, bad header, mid-frame close —
            // makes the *channel* unusable rather than the request wrong. The
            // channel is abandoned and the caller may reconnect, which is exactly
            // what `unavailable` means.
            Self::Codec(_) => ErrorKind::Unavailable,
            Self::Unsupported { .. } => ErrorKind::Unsupported,
            Self::Unavailable { .. } => ErrorKind::Unavailable,
            Self::PeerError { .. } => ErrorKind::InvalidInput,
            Self::MalformedResponse { .. } => ErrorKind::Uncertain,
            Self::Timeout { .. } => ErrorKind::Timeout,
            Self::Cancelled { .. } => ErrorKind::Cancelled,
            Self::PermitRefused { .. } => ErrorKind::PermissionDenied,
            Self::Launch(error) => error.error_kind(),
            Self::Journal(_) => ErrorKind::Internal,
        }
    }

    /// An `unsupported` refusal naming the method and the engine's actual behaviour.
    #[must_use]
    pub fn unsupported(method: &str, detail: &str) -> Self {
        Self::Unsupported {
            method: method.to_owned(),
            detail: detail.to_owned(),
        }
    }

    /// An `unavailable` refusal.
    #[must_use]
    pub fn unavailable(detail: &str) -> Self {
        Self::Unavailable {
            detail: detail.to_owned(),
        }
    }

    /// Whether this failure means the requested feature does not exist, as opposed
    /// to not being reachable right now.
    #[must_use]
    pub const fn is_unsupported(&self) -> bool {
        matches!(self, Self::Unsupported { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unimplemented_method_is_unsupported_not_internal_and_not_a_success() {
        let error = AdapterError::unsupported(
            "stepOut",
            "there is no req_stepOut handler in debug_adapter_parser.cpp, and Godot's dispatcher \
             writes no response for an unknown command",
        );
        assert_eq!(error.error_kind(), ErrorKind::Unsupported);
        assert!(error.is_unsupported());
        assert!(error.to_string().contains("req_stepOut"));
    }

    #[test]
    fn a_missing_host_is_unavailable_and_stays_distinct_from_unsupported() {
        let error = AdapterError::unavailable(
            "connection to 127.0.0.1:6005 refused; no editor is hosting the language server",
        );
        assert_eq!(error.error_kind(), ErrorKind::Unavailable);
        assert!(!error.is_unsupported());
    }

    #[test]
    fn a_refused_permit_is_permission_denied() {
        let error = AdapterError::PermitRefused {
            detail: "the permit's authority epoch is stale".to_owned(),
        };
        assert_eq!(error.error_kind(), ErrorKind::PermissionDenied);
    }

    #[test]
    fn a_deadline_and_a_cancellation_are_their_own_kinds() {
        assert_eq!(
            AdapterError::Timeout {
                method: "stackTrace".to_owned(),
                millis: 5000,
            }
            .error_kind(),
            ErrorKind::Timeout
        );
        assert_eq!(
            AdapterError::Cancelled {
                method: "variables".to_owned(),
            }
            .error_kind(),
            ErrorKind::Cancelled
        );
    }

    #[test]
    fn an_unusable_payload_is_uncertain_rather_than_a_failure_verdict() {
        let error = AdapterError::MalformedResponse {
            method: "textDocument/documentSymbol".to_owned(),
            detail: "result is neither an array nor null".to_owned(),
        };
        assert_eq!(error.error_kind(), ErrorKind::Uncertain);
    }
}
