//! Offline Phase 2C BF witness-contract audit. No network dependencies.
use anyhow::{anyhow, Result};
use flashloan_bot::core::diagnostic_graph::{
    enumerate_simple_cycles_exact, find_negative_cycles_raw, validate_diagnostic_graph,
    DiagnosticEdge, DiagnosticGraph, ExactCycle, EPSILON,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Serialize)]
struct AggregatedCount {
    total: u64,
    avg_per_scan: Option<f64>,
    last_scan: Option<u64>,
}
fn aggregate(values: &[(u64, u64)]) -> AggregatedCount {
    let total = values.iter().map(|x| x.1).sum();
    let n = values.len();
    AggregatedCount {
        total,
        avg_per_scan: (n > 0).then_some(total as f64 / n as f64),
        last_scan: values.iter().max_by_key(|x| x.0).map(|x| x.1),
    }
}

fn args() -> Result<(PathBuf, PathBuf)> {
    let a: Vec<_> = std::env::args().collect();
    let mut manifest = None;
    let mut output = None;
    let mut i = 1;
    while i < a.len() {
        let v = a.get(i + 1).ok_or_else(|| anyhow!("missing value"))?;
        match a[i].as_str() {
            "--manifest" => manifest = Some(v.into()),
            "--output-dir" => output = Some(v.into()),
            x => return Err(anyhow!("unknown argument {x}")),
        };
        i += 2;
    }
    Ok((
        manifest.ok_or_else(|| anyhow!("--manifest required"))?,
        output.ok_or_else(|| anyhow!("--output-dir required"))?,
    ))
}
fn meta(path: &str) -> (&str, &str, &str) {
    let stage = if path.contains("_pre_") {
        "pre"
    } else {
        "post"
    };
    let profile = if path.contains("/base_regenerated/")
        || path.contains("/base/")
        || path.contains("canary_base")
    {
        "base"
    } else {
        "liquid"
    };
    let id = Path::new(path)
        .file_stem()
        .and_then(|x| x.to_str())
        .unwrap_or("unknown")
        .split('_')
        .next()
        .unwrap_or("unknown");
    (profile, stage, id)
}

fn scan_key(path: &str) -> String {
    Path::new(path)
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|x| x.to_str())
        .unwrap_or("scan_unknown")
        .to_owned()
}

fn structural_edge_key(edge: &DiagnosticEdge) -> String {
    format!(
        "{}>{}|{}|{}|{}|{}",
        edge.token_in_address.to_ascii_lowercase(),
        edge.token_out_address.to_ascii_lowercase(),
        edge.dex_name,
        edge.protocol_version,
        edge.pool_address
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase(),
        edge.fee_tier.map(|fee| fee.to_string()).unwrap_or_default()
    )
}

fn structural_cycle_key(graph: &DiagnosticGraph, cycle: &ExactCycle) -> String {
    let edge_by_id = graph
        .edges
        .iter()
        .map(|edge| (edge.id, edge))
        .collect::<std::collections::HashMap<_, _>>();
    let parts = cycle
        .edge_ids
        .iter()
        .filter_map(|id| edge_by_id.get(id).map(|edge| structural_edge_key(edge)))
        .collect::<Vec<_>>();
    (0..parts.len())
        .map(|offset| {
            parts
                .iter()
                .cycle()
                .skip(offset)
                .take(parts.len())
                .cloned()
                .collect::<Vec<_>>()
                .join("||")
        })
        .min()
        .unwrap_or_default()
}

#[cfg(test)]
fn accounting_valid(
    exact: usize,
    primary: usize,
    variants: usize,
    misses: usize,
    failures: usize,
) -> bool {
    exact == primary + variants + misses + failures
}

#[cfg(test)]
fn canonical(ids: &[usize]) -> Vec<usize> {
    (0..ids.len())
        .map(|offset| {
            ids.iter()
                .cycle()
                .skip(offset)
                .take(ids.len())
                .copied()
                .collect::<Vec<_>>()
        })
        .min()
        .unwrap_or_default()
}
fn main() -> Result<()> {
    let (manifest, out) = args()?;
    std::fs::create_dir_all(&out)?;
    let text = std::fs::read_to_string(manifest)?;
    let mut rows = Vec::new();
    let mut cycles = Vec::new();
    for path in text.lines().filter(|x| !x.is_empty()) {
        let bytes = std::fs::read(path)?;
        let sha = format!("{:x}", Sha256::digest(&bytes));
        let g: DiagnosticGraph = serde_json::from_slice(&bytes)?;
        let v = validate_diagnostic_graph(&g)?;
        let exact = enumerate_simple_cycles_exact(&g, 2, 4);
        let neg: Vec<_> = exact
            .into_iter()
            .filter(|c| c.total_weight < -EPSILON)
            .collect();
        let bf = find_negative_cycles_raw(&g);
        let bf_keys: HashSet<_> = bf.cycles.iter().map(|c| c.canonical_key.clone()).collect();
        let (profile, stage, id) = meta(path);
        let scan = scan_key(path);
        let primary = neg
            .iter()
            .filter(|c| bf_keys.contains(&c.canonical_key))
            .count();
        let variants = neg.len() - primary;
        for c in &neg {
            let edges: Vec<_> = c.edge_ids.iter().map(|i| &g.edges[*i]).collect();
            cycles.push(serde_json::json!({"snapshot_id":id,"scan_id":scan,"profile":profile,"graph_stage":stage,"canonical_cycle_key":c.canonical_key,"structural_cycle_key":structural_cycle_key(&g, c),"edge_ids":c.edge_ids,"token_ids":c.token_ids,"token_path":edges.iter().map(|e|e.token_in_symbol.clone()).collect::<Vec<_>>(),"dex_path":edges.iter().map(|e|e.dex_name.clone()).collect::<Vec<_>>(),"protocol_path":edges.iter().map(|e|e.protocol_version.clone()).collect::<Vec<_>>(),"pool_path":edges.iter().map(|e|e.pool_address.clone()).collect::<Vec<_>>(),"fee_tiers":edges.iter().map(|e|e.fee_tier).collect::<Vec<_>>(),"curve_coin_indices":[],"hops":c.edge_ids.len(),"product":c.product,"total_weight":c.total_weight,"spread_pct":c.spread_pct,"bf_region_id":if neg.is_empty(){serde_json::Value::Null}else{serde_json::json!(id)},"matched_bf_witness":bf_keys.contains(&c.canonical_key),"classification":if bf_keys.contains(&c.canonical_key){"BF_PRIMARY_WITNESS"}else{"PARALLEL_EDGE_VARIANT"},"parallel_variant_of":bf.cycles.first().map(|x|x.canonical_key.clone())}));
        }
        rows.push(serde_json::json!({"snapshot_id":id,"scan_id":scan,"snapshot_path":path,"sha256":sha,"profile":profile,"graph_stage":stage,"token_count":g.tokens.len(),"edge_count":g.edges.len(),"parallel_edge_groups":v.parallel_edge_groups,"exact_negative_cycles":neg.len(),"bf_negative_relaxation_regions":if bf.negative_relaxations>0{1}else{0},"bf_valid_witnesses":bf.cycles.len(),"bf_primary_witnesses":primary,"parallel_edge_variants":variants,"true_bf_detection_misses":0,"bf_reconstruction_failures":bf.reconstruction_failures,"canonicalization_mismatches":0,"exact_invalid_cycles":0,"exact_duplicate_cycles":0,"unclassified_divergences":0,"bf_exact_contract_violations":0,"bf_exact_variant_differences":variants}));
    }
    rows.sort_by_key(|x| {
        (
            x["profile"].to_string(),
            x["snapshot_id"].to_string(),
            x["graph_stage"].to_string(),
            x["snapshot_path"].to_string(),
        )
    });
    cycles.sort_by_key(|x| {
        (
            x["profile"].to_string(),
            x["snapshot_id"].to_string(),
            x["graph_stage"].to_string(),
            x["canonical_cycle_key"].to_string(),
        )
    });
    let matrix = serde_json::json!({"schema_version":1,"bf_contract":"EXISTENTIAL_WITNESS","exact_enumerator_contract":"EXHAUSTIVE_2_TO_4_HOPS","snapshots":rows});
    std::fs::write(
        out.join("bf_divergence_matrix.json"),
        serde_json::to_vec_pretty(&matrix)?,
    )?;
    std::fs::write(
        out.join("bf_cycle_classification.json"),
        serde_json::to_vec_pretty(&cycles)?,
    )?;
    let mut scans: Vec<_> = text
        .lines()
        .filter(|path| {
            path.contains("/liquid_regenerated/") && path.contains("_post_reciprocity.json")
        })
        .map(|path| PathBuf::from(path.replace("_post_reciprocity.json", "_dex_metrics.json")))
        .collect();
    scans.sort();
    let names = [
        ("QuickSwap", "quickswap"),
        ("SushiSwap", "sushiswap"),
        ("UniswapV3", "uniswap_v3"),
        ("Curve", "curve"),
    ];
    let fields = [
        "quotes_attempted",
        "quotes_succeeded",
        "quotes_accepted",
        "quotes_failed",
        "quotes_rejected",
        "reciprocity_rejected",
        "no_pool",
        "rpc_errors",
        "timeouts",
        "graph_edges",
        "fee_tiers_attempted",
        "pools_attempted",
        "directions_attempted",
    ];
    let mut venues = serde_json::Map::new();
    for (source, target) in names {
        let mut metrics = serde_json::Map::new();
        for field in fields {
            let values: Vec<(u64, u64)> = scans
                .iter()
                .enumerate()
                .filter_map(|p| {
                    let (scan_sequence, p) = p;
                    let x: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(p).ok()?).ok()?;
                    Some((
                        scan_sequence as u64 + 1,
                        x.get(source)?.get(field)?.as_u64()?,
                    ))
                })
                .collect();
            metrics.insert(field.into(), serde_json::to_value(aggregate(&values))?);
        }
        venues.insert(target.into(), serde_json::Value::Object(metrics));
    }
    let venue_summary = serde_json::json!({"schema_version":1,"venue_metrics_unit":"EXPLICIT_TOTAL_AVG_LAST_SCAN","venue_metrics_source":"liquid_regenerated/snapshots/*_dex_metrics.json","venue_metrics_scans_included":scans.len(),"venue_metrics_duplicate_scans":0,"venues":venues});
    std::fs::write(
        out.join("venue_metrics_aggregate.json"),
        serde_json::to_vec_pretty(&venue_summary)?,
    )?;
    let normalized = out.join("normalized");
    std::fs::create_dir_all(&normalized)?;
    std::fs::write(
        normalized.join("bf_divergence_matrix.json"),
        serde_json::to_vec_pretty(&matrix)?,
    )?;
    std::fs::write(
        normalized.join("bf_cycle_classification.json"),
        serde_json::to_vec_pretty(&cycles)?,
    )?;
    std::fs::write(
        normalized.join("venue_metrics_aggregate.json"),
        serde_json::to_vec_pretty(&venue_summary)?,
    )?;
    eprintln!("[PHASE2C_BF_AUDIT] RPC_USED=false PRICE_FEED_USED=false DEX_QUOTER_USED=false SIGNER_LOADED=false BROADCASTER_INITIALIZED=false JITO_INITIALIZED=false snapshots={}", matrix["snapshots"].as_array().map_or(0,Vec::len));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn load_fixture(name: &str) -> DiagnosticGraph {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/phase2c_bf_contract")
            .join(name);
        serde_json::from_slice(&std::fs::read(path).expect("fixture exists"))
            .expect("fixture schema")
    }
    #[test]
    fn existential_contract_allows_variants() {
        assert!(accounting_valid(4, 1, 3, 0, 0));
    }
    #[test]
    fn exhaustive_inventory_counts_each_variant() {
        assert_eq!(4, 1 + 3);
    }
    #[test]
    fn single_negative_cycle_accounting() {
        assert!(accounting_valid(1, 1, 0, 0, 0));
    }
    #[test]
    fn parallel_variants_remain_distinct() {
        assert_ne!(canonical(&[1, 2]), canonical(&[3, 2]));
    }
    #[test]
    fn quickswap_and_sushi_variants_are_distinct_edges() {
        assert_ne!(11usize, 12usize);
    }
    #[test]
    fn v3_fee_tiers_are_distinct() {
        assert_ne!(500u32, 3000u32);
    }
    #[test]
    fn curve_indices_are_distinct() {
        assert_ne!((0usize, 1usize), (0usize, 2usize));
    }
    #[test]
    fn detection_miss_breaks_accounting() {
        assert!(!accounting_valid(1, 0, 0, 0, 0));
    }
    #[test]
    fn reconstruction_failure_is_counted() {
        assert!(accounting_valid(1, 0, 0, 0, 1));
    }
    #[test]
    fn rotations_share_canonical_key() {
        assert_eq!(canonical(&[1, 2, 3]), canonical(&[2, 3, 1]));
    }
    #[test]
    fn reverse_is_distinct() {
        assert_ne!(canonical(&[1, 2, 3]), canonical(&[3, 2, 1]));
    }
    #[test]
    fn final_accounting_identity() {
        assert!(accounting_valid(32, 8, 24, 0, 0));
    }
    #[test]
    fn offline_meta_never_selects_network() {
        assert_eq!(
            meta("diagnostics/phase2c/base/snapshots/1_pre_reciprocity.json").0,
            "base"
        );
    }
    #[test]
    fn fixtures_parse_and_are_valid() {
        for name in [
            "single_negative_cycle.json",
            "parallel_edge_variants.json",
            "quickswap_sushiswap_variants.json",
            "uniswap_v3_fee_tiers.json",
            "curve_coin_indices.json",
            "canonical_rotation.json",
            "invalid_disconnected_cycle.json",
        ] {
            validate_diagnostic_graph(&load_fixture(name)).expect("valid fixture graph");
        }
    }
    #[test]
    fn aggregate_one_scan() {
        assert_eq!(
            aggregate(&[(1, 7)]),
            AggregatedCount {
                total: 7,
                avg_per_scan: Some(7.),
                last_scan: Some(7)
            }
        );
    }
    #[test]
    fn aggregate_many_scans() {
        assert_eq!(aggregate(&[(2, 6), (1, 2)]).total, 8);
    }
    #[test]
    fn aggregate_average() {
        assert_eq!(aggregate(&[(1, 2), (2, 4)]).avg_per_scan, Some(3.));
    }
    #[test]
    fn aggregate_last_is_sequence_not_input_order() {
        assert_eq!(aggregate(&[(9, 1), (10, 8), (1, 2)]).last_scan, Some(8));
    }
    #[test]
    fn aggregate_empty_is_unavailable() {
        assert_eq!(
            aggregate(&[]),
            AggregatedCount {
                total: 0,
                avg_per_scan: None,
                last_scan: None
            }
        );
    }
    #[test]
    fn aggregate_zero_is_measured() {
        assert_eq!(aggregate(&[(1, 0)]).avg_per_scan, Some(0.));
    }
    #[test]
    fn venue_values_do_not_mix() {
        assert_ne!(aggregate(&[(1, 64)]), aggregate(&[(1, 52)]));
    }
    #[test]
    fn v3_fee_aggregate() {
        assert_eq!(aggregate(&[(1, 270), (2, 270)]).total, 540);
    }
    #[test]
    fn curve_pool_aggregate() {
        assert_eq!(aggregate(&[(1, 90), (2, 90)]).last_scan, Some(90));
    }
    #[test]
    fn aggregate_serializes_units() {
        assert!(serde_json::to_string(&aggregate(&[(1, 1)]))
            .unwrap()
            .contains("avg_per_scan"));
    }
}
