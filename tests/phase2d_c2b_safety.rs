use flashloan_bot::core::fresh_discovery_gate::{
    distinct_fresh_anchors, may_promote, RoundEvidence,
};

const HISTORICAL: &[u64] = &[91149850, 91149883, 91149916];

fn pass(block: u64) -> RoundEvidence {
    RoundEvidence {
        anchor_block: block,
        quote_only: false,
        economically_positive: true,
        read_only_pass: true,
        preflight_pass: true,
        preflight_reverted: false,
    }
}

#[test]
fn fresh_campaign_requires_three_distinct_anchor_blocks() {
    assert!(distinct_fresh_anchors(
        &[pass(100), pass(101), pass(102)],
        HISTORICAL
    ));
    assert!(!distinct_fresh_anchors(
        &[pass(100), pass(100), pass(102)],
        HISTORICAL
    ));
    assert!(!distinct_fresh_anchors(
        &[pass(91149850), pass(101), pass(102)],
        HISTORICAL
    ));
}

#[test]
fn rejected_route_is_never_preflighted_or_promoted() {
    assert!(!may_promote(
        &[pass(100), pass(101), pass(102)],
        true,
        false
    ));
}

#[test]
fn quote_only_route_is_not_phase2d_d_candidate() {
    let mut quote_only = pass(100);
    quote_only.quote_only = true;
    assert!(!may_promote(
        &[quote_only, pass(101), pass(102)],
        false,
        false
    ));
}

#[test]
fn candidate_requires_preflight_pass_in_all_rounds() {
    let mut partial = pass(101);
    partial.preflight_pass = false;
    assert!(!may_promote(&[pass(100), partial, pass(102)], false, false));
    assert!(may_promote(
        &[pass(100), pass(101), pass(102)],
        false,
        false
    ));
}

#[test]
fn zero_executable_candidates_is_valid_result() {
    assert!(!may_promote(&[], false, false));
}
