//! Phase 2D-D candidate deduplication. Pure logic — no RPC.
//!
//! The Phase 2D-C artifact's `route_id` is `"{profile}:{structural_cycle_key}"`
//! (see `route_artifact.rs`): the *same* physical route can appear twice,
//! once discovered under the `base` token-universe campaign and once under
//! `liquid`. Phase 2D-D must fork-execute the physical route once per
//! (anchor_block, size) — not once per profile duplicate — so this module
//! collapses `route_id` back down to `structural_cycle_key` and fails loudly
//! if two "duplicates" of the same key turn out to structurally disagree
//! (different pools/venues/token path), which would mean they are not
//! actually the same physical route and must not be silently merged.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

/// The subset of a Phase 2D-C `EvaluationRow` this module needs. `#[serde(rename_all... )]`
/// is not required — field names already match the 2D-C JSONL schema.
#[derive(Debug, Clone, Deserialize)]
pub struct SourceEvaluationRow {
    pub round_id: u64,
    pub route_id: String,
    pub venues: Vec<String>,
    pub pool_ids: Vec<String>,
    pub token_path: Vec<String>,
    pub size_human: f64,
    pub gross_positive: bool,
    pub net_positive: Option<bool>,
    #[serde(default)]
    pub net_pnl_atomic: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DedupError {
    #[error("PHASE2D_C_ROW_MISSING_STRUCTURAL_KEY: route_id={0}")]
    MissingStructuralKey(String),
    #[error("PHYSICAL_ROUTE_PROFILE_DIVERGENCE: structural_cycle_key={key} detail={detail}")]
    ProfileDivergence { key: String, detail: String },
    #[error("MULTIPLE_PHYSICAL_ROUTES_WITHOUT_EXPLANATION: found={0}")]
    UnexplainedMultiplePhysicalRoutes(usize),
}

#[derive(Debug, Clone)]
pub struct PhysicalRouteCandidate {
    pub structural_cycle_key: String,
    pub source_route_ids: Vec<String>,
    pub source_profiles: Vec<String>,
    pub venues: Vec<String>,
    pub pool_ids: Vec<String>,
    pub token_path: Vec<String>,
    /// Sizes that were net-positive in every round for at least one source
    /// profile's evidence.
    pub positive_sizes: Vec<f64>,
    /// Sizes that were gross-positive but net-negative in every round for
    /// every source profile — kept as an explicit negative control, not
    /// dropped.
    pub negative_control_sizes: Vec<f64>,
}

fn split_route_id(route_id: &str) -> Result<(String, String), DedupError> {
    route_id
        .split_once(':')
        .map(|(profile, key)| (profile.to_string(), key.to_string()))
        .ok_or_else(|| DedupError::MissingStructuralKey(route_id.to_string()))
}

/// Returns `true` if every round for this (route_id, size) group was net
/// positive. `rounds_seen` guards against silently treating a 1-round
/// dataset as "3/3 stable".
fn all_rounds_net_positive(rows: &[&SourceEvaluationRow], expected_rounds: u64) -> bool {
    let rounds_seen: BTreeSet<u64> = rows.iter().map(|r| r.round_id).collect();
    rounds_seen.len() as u64 >= expected_rounds && rows.iter().all(|r| r.net_positive == Some(true))
}

fn all_rounds_gross_positive_net_negative(
    rows: &[&SourceEvaluationRow],
    expected_rounds: u64,
) -> bool {
    let rounds_seen: BTreeSet<u64> = rows.iter().map(|r| r.round_id).collect();
    rounds_seen.len() as u64 >= expected_rounds
        && rows
            .iter()
            .all(|r| r.gross_positive && r.net_positive == Some(false))
}

/// Groups Phase 2D-C evaluation rows into deduplicated physical-route
/// candidates. `expected_rounds` is the number of independent rounds a
/// (route_id, size) needs to have been observed in before it counts as
/// stable in either direction (3 for the real campaign; parameterized so
/// tests can use smaller fixtures).
pub fn dedup_candidates(
    rows: &[SourceEvaluationRow],
    expected_rounds: u64,
) -> Result<Vec<PhysicalRouteCandidate>, DedupError> {
    // (venues, pool_ids, token_path) — used to check profile duplicates agree structurally.
    type RouteShape = (Vec<String>, Vec<String>, Vec<String>);
    // structural_cycle_key -> profile -> size -> rows
    let mut by_key: BTreeMap<String, BTreeMap<String, BTreeMap<u64, Vec<&SourceEvaluationRow>>>> =
        BTreeMap::new();
    // structural_cycle_key -> profile -> shape, for consistency check
    let mut shape_by_key: BTreeMap<String, BTreeMap<String, RouteShape>> = BTreeMap::new();

    for row in rows {
        let (profile, key) = split_route_id(&row.route_id)?;
        shape_by_key
            .entry(key.clone())
            .or_default()
            .entry(profile.clone())
            .or_insert_with(|| {
                (
                    row.venues.clone(),
                    row.pool_ids.clone(),
                    row.token_path.clone(),
                )
            });
        by_key
            .entry(key)
            .or_default()
            .entry(profile)
            .or_default()
            .entry(row.size_human.to_bits())
            .or_default()
            .push(row);
    }

    let mut candidates = Vec::new();
    for (key, by_profile) in &by_key {
        // Structural consistency: every profile sharing this key must agree
        // on venues/pools/token path, or this is not really "the same
        // physical route" and must not be silently merged.
        let shapes = &shape_by_key[key];
        let mut shapes_iter = shapes.values();
        if let Some(first) = shapes_iter.next() {
            for other in shapes_iter {
                if other != first {
                    return Err(DedupError::ProfileDivergence {
                        key: key.clone(),
                        detail: "venues/pool_ids/token_path differ across profiles".to_string(),
                    });
                }
            }
        }
        let (venues, pool_ids, token_path) = shapes.values().next().cloned().unwrap_or_default();

        let mut positive_sizes: BTreeSet<u64> = BTreeSet::new();
        let mut negative_control_sizes: BTreeSet<u64> = BTreeSet::new();
        let mut source_route_ids = BTreeSet::new();
        let mut source_profiles = BTreeSet::new();

        for (profile, by_size) in by_profile {
            source_profiles.insert(profile.clone());
            source_route_ids.insert(format!("{profile}:{key}"));
            for (size_bits, size_rows) in by_size {
                if all_rounds_net_positive(size_rows, expected_rounds) {
                    positive_sizes.insert(*size_bits);
                } else if all_rounds_gross_positive_net_negative(size_rows, expected_rounds) {
                    negative_control_sizes.insert(*size_bits);
                }
            }
        }

        candidates.push(PhysicalRouteCandidate {
            structural_cycle_key: key.clone(),
            source_route_ids: source_route_ids.into_iter().collect(),
            source_profiles: source_profiles.into_iter().collect(),
            venues,
            pool_ids,
            token_path,
            positive_sizes: positive_sizes.into_iter().map(f64::from_bits).collect(),
            negative_control_sizes: negative_control_sizes
                .into_iter()
                .map(f64::from_bits)
                .collect(),
        });
    }

    Ok(candidates)
}

/// Convenience for the campaign entrypoint: dedup, then require exactly one
/// physical route with at least one positive size — the campaign has
/// nothing to fork-execute otherwise, and more than one physical route
/// means the "single candidate" premise from the 2D-C report no longer
/// holds and needs a human decision, not a silent pick of "the first one".
pub fn single_physical_candidate(
    rows: &[SourceEvaluationRow],
    expected_rounds: u64,
) -> Result<PhysicalRouteCandidate, DedupError> {
    let mut candidates = dedup_candidates(rows, expected_rounds)?;
    candidates.retain(|c| !c.positive_sizes.is_empty());
    if candidates.len() != 1 {
        return Err(DedupError::UnexplainedMultiplePhysicalRoutes(
            candidates.len(),
        ));
    }
    Ok(candidates.into_iter().next().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        round_id: u64,
        route_id: &str,
        size_human: f64,
        gross_positive: bool,
        net_positive: Option<bool>,
    ) -> SourceEvaluationRow {
        SourceEvaluationRow {
            round_id,
            route_id: route_id.to_string(),
            venues: vec!["Curve".into(), "UniswapV3".into()],
            pool_ids: vec!["0xpool".into()],
            token_path: vec!["0xa".into(), "0xb".into(), "0xa".into()],
            size_human,
            gross_positive,
            net_positive,
            net_pnl_atomic: None,
        }
    }

    fn three_rounds_positive(route_id: &str, size: f64) -> Vec<SourceEvaluationRow> {
        (1..=3)
            .map(|r| row(r, route_id, size, true, Some(true)))
            .collect()
    }

    fn three_rounds_negative_control(route_id: &str, size: f64) -> Vec<SourceEvaluationRow> {
        (1..=3)
            .map(|r| row(r, route_id, size, true, Some(false)))
            .collect()
    }

    #[test]
    fn dedups_base_and_liquid_profile_duplicates_into_one_physical_route() {
        let mut rows = three_rounds_positive("base:key1", 100.0);
        rows.extend(three_rounds_positive("liquid:key1", 100.0));
        let candidates = dedup_candidates(&rows, 3).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].positive_sizes, vec![100.0]);
        assert_eq!(candidates[0].source_profiles, vec!["base", "liquid"]);
    }

    #[test]
    fn preserves_provenance_of_both_profiles() {
        let mut rows = three_rounds_positive("base:key1", 100.0);
        rows.extend(three_rounds_positive("liquid:key1", 100.0));
        let candidates = dedup_candidates(&rows, 3).unwrap();
        let mut ids = candidates[0].source_route_ids.clone();
        ids.sort();
        assert_eq!(
            ids,
            vec!["base:key1".to_string(), "liquid:key1".to_string()]
        );
    }

    #[test]
    fn negative_control_size_is_preserved_separately() {
        let mut rows = three_rounds_positive("base:key1", 100.0);
        rows.extend(three_rounds_negative_control("base:key1", 10.0));
        let candidates = dedup_candidates(&rows, 3).unwrap();
        assert_eq!(candidates[0].positive_sizes, vec![100.0]);
        assert_eq!(candidates[0].negative_control_sizes, vec![10.0]);
    }

    #[test]
    fn missing_structural_key_is_rejected() {
        let rows = vec![row(1, "no-colon-here", 100.0, true, Some(true))];
        assert!(matches!(
            dedup_candidates(&rows, 3),
            Err(DedupError::MissingStructuralKey(_))
        ));
    }

    #[test]
    fn profile_divergence_in_pools_is_rejected_not_merged() {
        let mut rows = three_rounds_positive("base:key1", 100.0);
        let mut divergent = three_rounds_positive("liquid:key1", 100.0);
        for r in &mut divergent {
            r.pool_ids = vec!["0xDIFFERENT".into()];
        }
        rows.extend(divergent);
        assert!(matches!(
            dedup_candidates(&rows, 3),
            Err(DedupError::ProfileDivergence { .. })
        ));
    }

    #[test]
    fn fewer_than_expected_rounds_is_not_counted_as_stable() {
        // Only 2 of 3 rounds present — must not count as 3/3 stable.
        let rows: Vec<_> = (1..=2)
            .map(|r| row(r, "base:key1", 100.0, true, Some(true)))
            .collect();
        let candidates = dedup_candidates(&rows, 3).unwrap();
        assert!(candidates[0].positive_sizes.is_empty());
    }

    #[test]
    fn one_negative_round_breaks_positive_classification() {
        let mut rows = three_rounds_positive("base:key1", 100.0);
        rows[2].net_positive = Some(false);
        let candidates = dedup_candidates(&rows, 3).unwrap();
        assert!(candidates[0].positive_sizes.is_empty());
    }

    #[test]
    fn single_physical_candidate_succeeds_with_exactly_one_positive_route() {
        let mut rows = three_rounds_positive("base:key1", 100.0);
        rows.extend(three_rounds_positive("liquid:key1", 100.0));
        let candidate = single_physical_candidate(&rows, 3).unwrap();
        assert_eq!(candidate.structural_cycle_key, "key1");
    }

    #[test]
    fn single_physical_candidate_rejects_two_distinct_positive_routes() {
        let mut rows = three_rounds_positive("base:key1", 100.0);
        rows.extend(three_rounds_positive("base:key2", 100.0));
        assert!(matches!(
            single_physical_candidate(&rows, 3),
            Err(DedupError::UnexplainedMultiplePhysicalRoutes(2))
        ));
    }

    #[test]
    fn single_physical_candidate_rejects_zero_positive_routes() {
        let rows = three_rounds_negative_control("base:key1", 10.0);
        assert!(matches!(
            single_physical_candidate(&rows, 3),
            Err(DedupError::UnexplainedMultiplePhysicalRoutes(0))
        ));
    }

    #[test]
    fn real_phase2d_c_artifact_dedups_to_expected_counts() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("diagnostics/phase2d_c_sequential_simulation_20260730T164545Z.jsonl");
        let content = std::fs::read_to_string(&path).unwrap();
        let rows: Vec<SourceEvaluationRow> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let candidates = dedup_candidates(&rows, 3).unwrap();
        let positive: Vec<_> = candidates
            .iter()
            .filter(|c| !c.positive_sizes.is_empty())
            .collect();
        assert_eq!(
            positive.len(),
            1,
            "expected exactly 1 physical route with positive sizes"
        );
        let mut sizes = positive[0].positive_sizes.clone();
        sizes.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(
            sizes,
            vec![25.0, 50.0, 100.0, 200.0, 300.0, 500.0, 750.0, 1000.0]
        );
        assert_eq!(positive[0].negative_control_sizes, vec![10.0]);
    }
}
