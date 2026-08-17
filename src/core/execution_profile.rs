//! Identity of the chain and operational profile that produced a route.
//!
//! This is deliberately distinct from the discovery token-universe profile
//! (`base`/`liquid`): it identifies the *operational context* the evidence
//! was produced under, so a diagnostic-binary fork audit and a live main-bot
//! dry-run round can never be silently aggregated into the same
//! three-anchor stability window even when they share a structural cycle.

use serde::{Deserialize, Serialize};

/// The diagnostic binary's authoritative 3-of-3 campaign: real Anvil fork
/// execution, receipts, and trace validation are required before a route
/// can count as stable under this profile.
pub const FORK_TRACE_AUDIT_PROFILE: &str = "fork-trace-audit";

/// The main bot's live canonical-primary loop: pinned-anchor discovery plus
/// pure integer economics and a pending `eth_call` dry run. No fork, no
/// receipt, no trace is ever produced or required under this profile.
pub const MAIN_PENDING_DRY_RUN_PROFILE: &str = "main-pending-dry-run";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ExecutionProfile {
    pub chain_id: u64,
    pub profile_label: String,
}

impl ExecutionProfile {
    /// Only the diagnostic binary's fork-audit profile requires real
    /// receipt/trace evidence before a route counts as stable/approvable.
    /// Every other profile (in particular `main-pending-dry-run`) must
    /// never be blocked on evidence it structurally cannot produce.
    pub fn requires_fork_evidence(&self) -> bool {
        self.profile_label == FORK_TRACE_AUDIT_PROFILE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_with_different_labels_are_not_equal() {
        let a = ExecutionProfile {
            chain_id: 137,
            profile_label: "base".into(),
        };
        let b = ExecutionProfile {
            chain_id: 137,
            profile_label: "liquid".into(),
        };
        assert_ne!(a, b);
    }

    #[test]
    fn profiles_with_same_fields_are_equal() {
        let a = ExecutionProfile {
            chain_id: 137,
            profile_label: "base".into(),
        };
        let b = ExecutionProfile {
            chain_id: 137,
            profile_label: "base".into(),
        };
        assert_eq!(a, b);
    }
}
