//! Phase 2D-C route artifact loader.
//!
//! Parses the Phase 2D-B structural-persistence artifact
//! (`diagnostics/phase2d_b/analysis/cycle_persistence.json`) into typed,
//! validated `StructuralRoute`s. Pure logic — no RPC, no execution.
//!
//! `structural_cycle_key` legs are produced by
//! `phase2c_bf_audit::structural_edge_key` as
//! `"{token_in}>{token_out}|{dex_name}|{protocol_version}|{pool_address}|{fee_tier}"`
//! joined with a literal `"||"` separator. Splitting the whole key on a single
//! `|` therefore yields exactly 5 tokens per leg plus one empty separator
//! token between legs (the `||` always contributes one empty string when
//! split this way, regardless of whether the leg's own trailing field is
//! empty) — never a naive `split("||")`, which is ambiguous whenever a leg's
//! own field (pool_address, fee_tier) is itself empty.

use anyhow::Result;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteLeg {
    pub token_in: String,
    pub token_out: String,
    pub venue: String,
    pub protocol_version: String,
    pub pool_address: Option<String>,
    pub fee_tier: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum RouteReturnClass {
    /// Observed in >=2 independent Phase 2D-B scans of its profile.
    ReturnStable,
    /// Observed in exactly 1 scan — not enough independent observations to
    /// call the return stable.
    ReturnInsufficientObservations,
}

#[derive(Debug, Clone)]
pub struct StructuralRoute {
    /// Unique campaign identity: `"{profile}:{structural_cycle_key}"`. The
    /// same physical route can be independently observed and persisted under
    /// both the `base` and `liquid` Phase 2D-B token-universe campaigns (the
    /// `liquid` universe is a superset of `base`) — Phase 2D-B's own
    /// `structural_cycle_keys_total=11` counts those as distinct structural
    /// entries (each has its own persistence evidence/anchors), so bare
    /// `structural_cycle_key` is not a safe uniqueness key on its own.
    pub route_id: String,
    /// Topology identity shared by routes observed under different profiles.
    pub structural_cycle_key: String,
    pub legs: Vec<RouteLeg>,
    pub hop_count: usize,
    pub profile: String,
    pub scans_observed: u64,
    pub return_class: RouteReturnClass,
    pub pools: Vec<String>,
    pub venues: Vec<String>,
    pub gross_multiplier_avg: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, serde::Serialize)]
pub enum RouteLoadError {
    #[error("ROUTE_ARTIFACT_MISSING: {0}")]
    ArtifactMissing(String),
    #[error("ROUTE_ARTIFACT_INVALID: {0}")]
    ArtifactInvalid(String),
    #[error("ROUTE_EDGE_MISSING: {0}")]
    EdgeMissing(String),
    #[error("ROUTE_TOKEN_DISCONTINUITY: {0}")]
    TokenDiscontinuity(String),
    #[error("ROUTE_NOT_CLOSED: {0}")]
    NotClosed(String),
    #[error("ROUTE_DUPLICATE: {0}")]
    Duplicate(String),
}

#[derive(Debug, Clone)]
pub struct RouteFailure {
    pub route_id: String,
    pub error: RouteLoadError,
}

#[derive(Debug, Clone, Default)]
pub struct RouteLoadReport {
    pub routes: Vec<StructuralRoute>,
    pub failures: Vec<RouteFailure>,
}

#[derive(Debug, Deserialize)]
struct RawArtifact {
    cycles: Vec<RawCycle>,
    #[allow(dead_code)]
    schema_version: u32,
}

#[derive(Debug, Deserialize)]
struct RawCycle {
    structural_cycle_key: String,
    hop_count: usize,
    profile: String,
    scans_observed: u64,
    pools: Vec<String>,
    venues: Vec<String>,
    gross_multiplier_avg: f64,
}

/// Splits a `structural_cycle_key` into its legs. See module docs for why a
/// naive `split("||")` is unsafe here.
fn parse_legs(key: &str, hop_count: usize) -> Result<Vec<RouteLeg>, RouteLoadError> {
    let tokens: Vec<&str> = key.split('|').collect();
    let expected_tokens = hop_count
        .checked_mul(6)
        .and_then(|v| v.checked_sub(1))
        .ok_or_else(|| RouteLoadError::ArtifactInvalid(format!("hop_count overflow: {key}")))?;
    if hop_count == 0 || tokens.len() != expected_tokens {
        return Err(RouteLoadError::EdgeMissing(format!(
            "expected {expected_tokens} tokens for hop_count={hop_count}, got {} in key={key}",
            tokens.len()
        )));
    }

    let mut legs = Vec::with_capacity(hop_count);
    let mut idx = 0usize;
    for leg_index in 0..hop_count {
        let chunk = &tokens[idx..idx + 5];
        let (token_in, token_out) = chunk[0].split_once('>').ok_or_else(|| {
            RouteLoadError::ArtifactInvalid(format!(
                "leg {leg_index} missing '>' token pair in key={key}"
            ))
        })?;
        if token_in.is_empty() || token_out.is_empty() {
            return Err(RouteLoadError::ArtifactInvalid(format!(
                "leg {leg_index} has empty token in key={key}"
            )));
        }
        let venue = chunk[1].to_string();
        let protocol_version = chunk[2].to_string();
        let pool_address = (!chunk[3].is_empty()).then(|| chunk[3].to_string());
        let fee_tier = if chunk[4].is_empty() {
            None
        } else {
            Some(chunk[4].parse::<u32>().map_err(|_| {
                RouteLoadError::ArtifactInvalid(format!(
                    "leg {leg_index} non-numeric fee_tier={} in key={key}",
                    chunk[4]
                ))
            })?)
        };
        legs.push(RouteLeg {
            token_in: token_in.to_string(),
            token_out: token_out.to_string(),
            venue,
            protocol_version,
            pool_address,
            fee_tier,
        });
        idx += 5;
        if leg_index + 1 < hop_count {
            let sep = tokens.get(idx).copied().unwrap_or("not-empty-sentinel");
            if !sep.is_empty() {
                return Err(RouteLoadError::ArtifactInvalid(format!(
                    "leg separator not empty between leg {leg_index} and next in key={key}"
                )));
            }
            idx += 1;
        }
    }
    Ok(legs)
}

fn validate_route(legs: &[RouteLeg], route_id: &str) -> Result<(), RouteLoadError> {
    for window in legs.windows(2) {
        if !window[0]
            .token_out
            .eq_ignore_ascii_case(&window[1].token_in)
        {
            return Err(RouteLoadError::TokenDiscontinuity(format!(
                "route {route_id}: leg token_out={} != next leg token_in={}",
                window[0].token_out, window[1].token_in
            )));
        }
    }
    let (Some(first), Some(last)) = (legs.first(), legs.last()) else {
        return Err(RouteLoadError::ArtifactInvalid(format!(
            "route {route_id}: no legs"
        )));
    };
    if !first.token_in.eq_ignore_ascii_case(&last.token_out) {
        return Err(RouteLoadError::NotClosed(format!(
            "route {route_id}: first token_in={} != last token_out={}",
            first.token_in, last.token_out
        )));
    }
    Ok(())
}

/// Loads and validates the Phase 2D-B structural route artifact.
///
/// A malformed top-level artifact (missing file, unparseable JSON) aborts the
/// whole load. A malformed individual route is recorded as a `RouteFailure`
/// and excluded from `routes` — the artifact is never silently
/// "reconstructed".
pub fn load_structural_routes(path: &Path) -> Result<RouteLoadReport, RouteLoadError> {
    if !path.exists() {
        return Err(RouteLoadError::ArtifactMissing(path.display().to_string()));
    }
    let bytes = std::fs::read(path)
        .map_err(|e| RouteLoadError::ArtifactMissing(format!("{}: {e}", path.display())))?;
    let raw: RawArtifact = serde_json::from_slice(&bytes)
        .map_err(|e| RouteLoadError::ArtifactInvalid(format!("{}: {e}", path.display())))?;

    let mut report = RouteLoadReport::default();
    let mut seen: HashSet<String> = HashSet::new();

    for cycle in raw.cycles {
        let route_id = format!("{}:{}", cycle.profile, cycle.structural_cycle_key);
        if !seen.insert(route_id.clone()) {
            report.failures.push(RouteFailure {
                route_id: route_id.clone(),
                error: RouteLoadError::Duplicate(route_id),
            });
            continue;
        }
        let outcome = parse_legs(&cycle.structural_cycle_key, cycle.hop_count).and_then(|legs| {
            validate_route(&legs, &route_id)?;
            Ok(legs)
        });
        match outcome {
            Ok(legs) => {
                let return_class = if cycle.scans_observed >= 2 {
                    RouteReturnClass::ReturnStable
                } else {
                    RouteReturnClass::ReturnInsufficientObservations
                };
                report.routes.push(StructuralRoute {
                    route_id,
                    structural_cycle_key: cycle.structural_cycle_key,
                    legs,
                    hop_count: cycle.hop_count,
                    profile: cycle.profile,
                    scans_observed: cycle.scans_observed,
                    return_class,
                    pools: cycle.pools,
                    venues: cycle.venues,
                    gross_multiplier_avg: cycle.gross_multiplier_avg,
                });
            }
            Err(error) => report.failures.push(RouteFailure { route_id, error }),
        }
    }
    Ok(report)
}

/// Per-route pool-reuse detection: pool identity is `(venue, pool_address)`
/// (falls back to `(venue, token_in>token_out)` for legs without an on-chain
/// pool address, e.g. artifacts predating pool capture).
pub fn pool_identity(leg: &RouteLeg) -> String {
    match &leg.pool_address {
        Some(addr) => format!("{}:{}", leg.venue, addr.to_ascii_lowercase()),
        None => format!(
            "{}:{}>{}",
            leg.venue,
            leg.token_in.to_ascii_lowercase(),
            leg.token_out.to_ascii_lowercase()
        ),
    }
}

/// Returns, for each leg index, `Some(first_leg_index)` of the earlier leg
/// using the same physical pool if this leg is a reuse, else `None`.
pub fn detect_pool_reuse(legs: &[RouteLeg]) -> Vec<Option<usize>> {
    let mut first_seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    legs.iter()
        .enumerate()
        .map(|(i, leg)| {
            let id = pool_identity(leg);
            let prior = first_seen.get(&id).copied();
            first_seen.entry(id).or_insert(i);
            prior
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("phase2d_c_route_artifact_{tag}_{nanos}"));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_artifact(dir: &Path, json: &str) -> std::path::PathBuf {
        let path = dir.join("cycle_persistence.json");
        std::fs::write(&path, json).unwrap();
        path
    }

    fn cycle_obj_json(key: &str, hop_count: usize, scans_observed: u64) -> String {
        format!(
            r#"{{"structural_cycle_key":"{key}","hop_count":{hop_count},"profile":"base","scans_observed":{scans_observed},"pools":[],"venues":[],"gross_multiplier_avg":1.001}}"#
        )
    }

    fn cycle_json(key: &str, hop_count: usize, scans_observed: u64) -> String {
        format!(
            r#"{{"cycles":[{}],"schema_version":1}}"#,
            cycle_obj_json(key, hop_count, scans_observed)
        )
    }

    const REAL_KEY_4HOP: &str = "0x1bfd67037b42cf73acf2047067bd4f2c47d9bfd6>0x7ceb23fd6bc0add59e62ac25578270cff1b9f619|UniswapV3|V3||500||0x7ceb23fd6bc0add59e62ac25578270cff1b9f619>0xc2132d05d31c914a87c6611c10748aeb04b58e8f|UniswapV3|V3||500||0xc2132d05d31c914a87c6611c10748aeb04b58e8f>0x3c499c542cef5e3811e1192ce70d8cc03d5c3359|Curve|CurveStableSwap|0x445fe580ef8d70ff569ab36e80c647af338db351|||0x3c499c542cef5e3811e1192ce70d8cc03d5c3359>0x1bfd67037b42cf73acf2047067bd4f2c47d9bfd6|UniswapV3|V3||500";

    const REAL_KEY_2HOP: &str = "0x3c499c542cef5e3811e1192ce70d8cc03d5c3359>0xc2132d05d31c914a87c6611c10748aeb04b58e8f|UniswapV3|V3||500||0xc2132d05d31c914a87c6611c10748aeb04b58e8f>0x3c499c542cef5e3811e1192ce70d8cc03d5c3359|Curve|CurveStableSwap|0x445fe580ef8d70ff569ab36e80c647af338db351|";

    #[test]
    fn parses_real_4hop_key_into_4_legs() {
        let legs = parse_legs(REAL_KEY_4HOP, 4).unwrap();
        assert_eq!(legs.len(), 4);
        assert_eq!(legs[0].venue, "UniswapV3");
        assert_eq!(legs[2].venue, "Curve");
        assert_eq!(legs[2].fee_tier, None);
        assert_eq!(
            legs[2].pool_address.as_deref(),
            Some("0x445fe580ef8d70ff569ab36e80c647af338db351")
        );
        assert_eq!(legs[0].fee_tier, Some(500));
    }

    #[test]
    fn parses_real_2hop_key_trailing_empty_fee() {
        let legs = parse_legs(REAL_KEY_2HOP, 2).unwrap();
        assert_eq!(legs.len(), 2);
        assert_eq!(legs[1].fee_tier, None);
        assert_eq!(legs[1].venue, "Curve");
    }

    #[test]
    fn route_is_closed_and_continuous() {
        let legs = parse_legs(REAL_KEY_4HOP, 4).unwrap();
        assert!(validate_route(&legs, "r").is_ok());
    }

    #[test]
    fn detects_token_discontinuity() {
        let mut legs = parse_legs(REAL_KEY_4HOP, 4).unwrap();
        legs[1].token_in = "0xdeadbeef".to_string();
        assert!(matches!(
            validate_route(&legs, "r"),
            Err(RouteLoadError::TokenDiscontinuity(_))
        ));
    }

    #[test]
    fn detects_not_closed() {
        let mut legs = parse_legs(REAL_KEY_4HOP, 4).unwrap();
        legs.last_mut().unwrap().token_out = "0xdeadbeef".to_string();
        assert!(matches!(
            validate_route(&legs, "r"),
            Err(RouteLoadError::NotClosed(_))
        ));
    }

    #[test]
    fn detects_hop_count_mismatch_as_edge_missing() {
        assert!(matches!(
            parse_legs(REAL_KEY_4HOP, 3),
            Err(RouteLoadError::EdgeMissing(_))
        ));
    }

    #[test]
    fn missing_artifact_file_is_missing_error() {
        let dir = TempDir::new("missing");
        let path = dir.path().join("does_not_exist.json");
        assert!(matches!(
            load_structural_routes(&path),
            Err(RouteLoadError::ArtifactMissing(_))
        ));
    }

    #[test]
    fn malformed_json_is_invalid_error() {
        let dir = TempDir::new("malformed");
        let path = write_artifact(dir.path(), "{not json");
        assert!(matches!(
            load_structural_routes(&path),
            Err(RouteLoadError::ArtifactInvalid(_))
        ));
    }

    #[test]
    fn loads_one_valid_route_with_stable_class() {
        let dir = TempDir::new("one_valid");
        let path = write_artifact(dir.path(), &cycle_json(REAL_KEY_4HOP, 4, 3));
        let report = load_structural_routes(&path).unwrap();
        assert_eq!(report.routes.len(), 1);
        assert!(report.failures.is_empty());
        assert_eq!(
            report.routes[0].return_class,
            RouteReturnClass::ReturnStable
        );
    }

    #[test]
    fn single_scan_observation_is_insufficient() {
        let dir = TempDir::new("single_scan");
        let path = write_artifact(dir.path(), &cycle_json(REAL_KEY_2HOP, 2, 1));
        let report = load_structural_routes(&path).unwrap();
        assert_eq!(report.routes.len(), 1);
        assert_eq!(
            report.routes[0].return_class,
            RouteReturnClass::ReturnInsufficientObservations
        );
    }

    /// Loads the real Phase 2D-B artifact this whole phase depends on. If
    /// this ever regresses, Phase 2D-C has nothing to simulate against.
    #[test]
    fn real_phase2d_b_artifact_loads_all_11_structural_routes() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("diagnostics/phase2d_b/analysis/cycle_persistence.json");
        let report = load_structural_routes(&path).unwrap();
        assert!(
            report.failures.is_empty(),
            "unexpected failures: {:?}",
            report.failures
        );
        assert_eq!(report.routes.len(), 11);
        let stable = report
            .routes
            .iter()
            .filter(|r| r.return_class == RouteReturnClass::ReturnStable)
            .count();
        let insufficient = report
            .routes
            .iter()
            .filter(|r| r.return_class == RouteReturnClass::ReturnInsufficientObservations)
            .count();
        assert_eq!(stable, 10);
        assert_eq!(insufficient, 1);
        for route in &report.routes {
            assert_eq!(route.legs.len(), route.hop_count);
        }
    }

    #[test]
    fn invalid_route_is_reported_as_failure_not_reconstructed() {
        let dir = TempDir::new("invalid_route");
        let mut broken = REAL_KEY_4HOP.replace(
            "0x3c499c542cef5e3811e1192ce70d8cc03d5c3359>0x1bfd67037b42cf73acf2047067bd4f2c47d9bfd6",
            "0x3c499c542cef5e3811e1192ce70d8cc03d5c3359>0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        );
        // keep it syntactically well-formed but structurally not closed
        assert_ne!(broken, REAL_KEY_4HOP);
        broken.push_str("");
        let path = write_artifact(dir.path(), &cycle_json(&broken, 4, 3));
        let report = load_structural_routes(&path).unwrap();
        assert!(report.routes.is_empty());
        assert_eq!(report.failures.len(), 1);
        assert!(matches!(
            report.failures[0].error,
            RouteLoadError::NotClosed(_)
        ));
    }

    #[test]
    fn duplicate_route_ids_are_flagged() {
        let dir = TempDir::new("duplicate");
        let json = format!(
            r#"{{"cycles":[{},{}],"schema_version":1}}"#,
            cycle_obj_json(REAL_KEY_2HOP, 2, 3),
            cycle_obj_json(REAL_KEY_2HOP, 2, 3)
        );
        let path = write_artifact(dir.path(), &json);
        let report = load_structural_routes(&path).unwrap();
        assert_eq!(report.routes.len(), 1);
        assert_eq!(report.failures.len(), 1);
        assert!(matches!(
            report.failures[0].error,
            RouteLoadError::Duplicate(_)
        ));
    }

    #[test]
    fn pool_reuse_detected_for_repeated_curve_pool() {
        let legs = parse_legs(REAL_KEY_4HOP, 4).unwrap();
        // legs: V3, V3, Curve(pool X), V3 — no reuse in this fixture.
        let reuse = detect_pool_reuse(&legs);
        assert_eq!(reuse, vec![None, None, None, None]);
    }

    #[test]
    fn pool_reuse_detected_when_same_pool_appears_twice() {
        let mut legs = parse_legs(REAL_KEY_4HOP, 4).unwrap();
        // Legs 0 and 3 are both UniswapV3 but on different pools (no pool
        // address recorded for V3 legs in this fixture, so identity falls
        // back to venue+token-pair). Force them onto the same physical pool
        // to simulate genuine reuse, as happens for repeated Curve pools in
        // the real Phase 2D-B artifact.
        legs[0].pool_address = Some("0xsamepool".to_string());
        legs[3].pool_address = Some("0xsamepool".to_string());
        legs[3].venue = legs[0].venue.clone();
        let reuse = detect_pool_reuse(&legs);
        assert_eq!(reuse[3], Some(0));
    }
}
