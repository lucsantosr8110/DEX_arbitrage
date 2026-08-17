//! Pure Phase 2D-C2B promotion rules.  Kept independent of RPC/fork IO so
//! the safety contract is regression-testable.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundEvidence {
    pub anchor_block: u64,
    pub quote_only: bool,
    pub economically_positive: bool,
    pub read_only_pass: bool,
    pub preflight_pass: bool,
    pub preflight_reverted: bool,
}

pub fn distinct_fresh_anchors(rounds: &[RoundEvidence], historical: &[u64]) -> bool {
    rounds.len() == 3
        && rounds
            .iter()
            .all(|round| !historical.contains(&round.anchor_block))
        && rounds
            .iter()
            .map(|round| round.anchor_block)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == 3
}

pub fn may_promote(rounds: &[RoundEvidence], rejected_registry_hit: bool, sign_flip: bool) -> bool {
    !rejected_registry_hit
        && !sign_flip
        && rounds.len() == 3
        && rounds.iter().all(|round| {
            !round.quote_only
                && round.economically_positive
                && round.read_only_pass
                && round.preflight_pass
                && !round.preflight_reverted
        })
}
