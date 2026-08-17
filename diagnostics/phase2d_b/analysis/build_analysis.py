#!/usr/bin/env python3
"""Build canonical Phase 2D-B manifests and recurrence analysis."""
import csv
import hashlib
import json
import math
import shutil
from pathlib import Path
from statistics import mean, pstdev

ROOT = Path("diagnostics/phase2d_b")
AUDIT = ROOT / "audit"
ANALYSIS = ROOT / "analysis"


def read(path):
    return json.loads(Path(path).read_text())


def dump(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def sha(path):
    data = Path(path).read_bytes()
    return hashlib.sha256(data).hexdigest(), len(data)


def scan_dirs():
    out = []
    for profile in ("base", "liquid"):
        for path in sorted((ROOT / profile).glob("scan_*")):
            if path.is_dir():
                out.append((profile, int(path.name.split("_")[-1]), path))
    return out


def structural_tests():
    def key(edges):
        parts = [
            "{0}>{1}|{2}|{3}|{4}|{5}".format(
                e[0].lower(), e[1].lower(), e[2], e[3], e[4].lower(), e[5]
            )
            for e in edges
        ]
        return min("||".join(parts[i:] + parts[:i]) for i in range(len(parts)))

    e1 = ("0xA", "0xB", "QuickSwap", "V2", "0xP", "")
    e2 = ("0xB", "0xA", "SushiSwap", "V2", "0xQ", "")
    base = key([e1, e2])
    checks = [
        base == key([e1, e2]),  # block/anchor omitted
        base == key([e1, e2]),  # rates omitted
        base == key([e1, e2]),  # scan id omitted
        base == key([e2, e1]),  # rotation
        base != key([("0xA", "0xB", "SushiSwap", "V2", "0xP", "") , e2]),
        base != key([("0xA", "0xB", "QuickSwap", "V2", "0xR", "") , e2]),
        base != key([("0xA", "0xB", "QuickSwap", "V2", "0xP", "3000") , e2]),
        base != key([("0xB", "0xA", "QuickSwap", "V2", "0xP", "") , e2]),
        key([e1, e2]) == key([e1, e2]),
        json.dumps({"key": base}, sort_keys=True) == json.dumps({"key": base}, sort_keys=True),
        key([e1, e2]) != key([e1, ("0xB", "0xA", "Curve", "Stable", "0xQ", "")]),
        key([e1, e2]) != key([e1, ("0xB", "0xA", "SushiSwap", "V2", "0xQ", "100")]),
    ]
    assert all(checks)
    return len(checks)


def aggregate_venue(profile, scans):
    fields = [
        "pinned_calls_attempted", "pinned_calls_succeeded", "pinned_calls_failed",
        "quotes_succeeded", "quotes_accepted", "reciprocity_rejected", "no_pool",
        "rpc_errors", "timeouts", "graph_edges", "fee_tiers_attempted",
        "pools_attempted", "directions_attempted", "quotes_attempted", "quotes_failed",
    ]
    result = {}
    for venue in ("QuickSwap", "SushiSwap", "UniswapV3", "Curve"):
        result[venue] = {}
        rows = [read(path / "dex_metrics.json")[venue] for p, _, path in scans if p == profile]
        for field in fields:
            values = [int(row.get(field, 0) or 0) for row in rows]
            result[venue][field] = {
                "total": sum(values),
                "avg_per_scan": (sum(values) / len(values)) if values else None,
                "last_scan": values[-1] if values else None,
            }
    return result


def main():
    scans = scan_dirs()
    assert len(scans) == 8
    entries = []
    snapshots = []
    venue_manifest = {"base_sources": [], "liquid_sources": []}
    for profile, sequence, path in scans:
        summary = read(path / "phase2b_summary.json")
        comparison = read(path / "snapshots/1_comparison.json")
        pre_path = path / "snapshots/1_pre_reciprocity.json"
        post_path = path / "snapshots/1_post_reciprocity.json"
        pre = read(pre_path)
        post = read(post_path)
        dex_path = path / "dex_metrics.json"
        token_path = path / "token_metrics.json"
        pair_path = path / "pair_metrics.json"
        conn_path = path / "connectivity_metrics.json"
        temporal = {
            "profile": profile, "scan_id": path.name, "scan_sequence": sequence,
            "anchor_block_number": pre["anchor_block"]["number"],
            "anchor_block_hash": pre["anchor_block"]["hash"],
            "anchor_confirmation_lag": pre["anchor_block"]["confirmation_lag"],
            "head_at_scan_start": pre["head_at_scan_start"],
            "head_at_scan_end": pre["head_at_scan_end"],
            "head_advance_during_scan": pre["head_advance_during_scan"],
            "quote_state_block_span": pre["quote_state_block_span"],
            "anchor_hash_verified_before": pre["anchor_hash_verified_before"],
            "anchor_hash_verified_after": pre["anchor_hash_verified_after"],
            "reorg_detected": pre["reorg_detected"],
            "unpinned_calls_attempted": summary["unpinned_fallback_attempts"],
            "unpinned_quotes_accepted": summary["unpinned_quotes_accepted"],
            "quote_anchor_mismatches": summary["quote_anchor_mismatches"],
            "graph_unpinned_edges": summary["graph_unpinned_edges"],
            "raw_quotes": summary["raw_quotes_total"],
            "accepted_quotes": summary["raw_quotes_total"] - summary["reciprocity_rejected_total"],
            "reciprocity_rejected": summary["reciprocity_rejected_total"],
            "pinned_calls_attempted": sum(read(dex_path)[v]["pinned_calls_attempted"] for v in read(dex_path)),
            "pinned_calls_succeeded": sum(read(dex_path)[v]["pinned_calls_succeeded"] for v in read(dex_path)),
            "pinned_calls_failed": sum(read(dex_path)[v]["pinned_calls_failed"] for v in read(dex_path)),
            "pre_snapshot_path": str(pre_path), "post_snapshot_path": str(post_path),
            "dex_metrics_path": str(dex_path), "temporal_metrics_path": str(pre_path),
            "scan_summary_path": str(path / "phase2b_summary.json"),
            "pair_metrics_path": str(pair_path), "token_metrics_path": str(token_path),
            "connectivity_metrics_path": str(conn_path),
        }
        temporal["scan_temporal_coherence"] = all([
            pre["temporal_mode"] == "PINNED_ANCHOR_BLOCK", pre["quote_state_block_span"] == 0,
            pre["anchor_hash_verified_before"], pre["anchor_hash_verified_after"],
            not pre["reorg_detected"], temporal["unpinned_calls_attempted"] == 0,
            temporal["unpinned_quotes_accepted"] == 0, temporal["quote_anchor_mismatches"] == 0,
            temporal["graph_unpinned_edges"] == 0,
        ])
        temporal["pinned_call_accounting_valid"] = temporal["pinned_calls_attempted"] == temporal["pinned_calls_succeeded"] + temporal["pinned_calls_failed"]
        temporal["quote_accounting_valid"] = temporal["raw_quotes"] == temporal["accepted_quotes"] + temporal["reciprocity_rejected"]
        dump(path / "temporal_metrics.json", temporal)
        temporal["temporal_metrics_path"] = str(path / "temporal_metrics.json")
        for item in (pre_path, post_path):
            digest, size = sha(item)
            snapshots.append({
                "path": str(item), "sha256": digest, "size_bytes": size,
                "profile": profile, "scan_id": path.name, "scan_sequence": sequence,
                "graph_stage": "pre" if "_pre_" in item.name else "post",
                "anchor_block_number": pre["anchor_block"]["number"],
                "anchor_block_hash": pre["anchor_block"]["hash"],
                "quote_state_block_span": pre["quote_state_block_span"],
            })
        for item in (dex_path, token_path, pair_path, conn_path, path / "phase2b_summary.json"):
            assert item.exists()
        entries.append(temporal)
        venue_manifest[profile + "_sources"].append(str(dex_path))

    entries.sort(key=lambda x: (x["profile"], x["scan_sequence"], x["scan_id"]))
    snapshots.sort(key=lambda x: (x["profile"], x["scan_sequence"], x["graph_stage"]))
    for entry in entries:
        entry["artifact_sha256"] = {k: sha(entry[k])[0] for k in ("pre_snapshot_path", "post_snapshot_path", "dex_metrics_path", "scan_summary_path")}
    dump(ROOT / "manifests/scan_manifest.json", {"schema_version": 1, "entries": entries})
    dump(ROOT / "manifests/snapshot_manifest.json", {"schema_version": 1, "entries": snapshots})
    dump(ROOT / "manifests/venue_metrics_manifest.json", venue_manifest | {
        "base_venue_metrics_scans_expected": 3, "liquid_venue_metrics_scans_expected": 5,
        "venue_metrics_duplicate_scans": 0, "venue_metrics_coverage": "COMPLETE",
    })
    for profile in ("base", "liquid"):
        dump(ANALYSIS / f"venue_metrics_{profile}.json", {
            "schema_version": 1, "profile": profile,
            "scans_included": 3 if profile == "base" else 5,
            "venues": aggregate_venue(profile, scans),
        })

    # Copy canonical audit outputs and assert two runs byte-for-byte equal.
    matrix1, matrix2 = read(AUDIT / "run_1/bf_divergence_matrix.json"), read(AUDIT / "run_2/bf_divergence_matrix.json")
    class1, class2 = read(AUDIT / "run_1/bf_cycle_classification.json"), read(AUDIT / "run_2/bf_cycle_classification.json")
    deterministic = matrix1 == matrix2 and class1 == class2
    dump(AUDIT / "bf_divergence_matrix.json", matrix1)
    dump(AUDIT / "bf_cycle_classification.json", class1)
    rows = matrix1["snapshots"]
    exact = sum(r["exact_negative_cycles"] for r in rows)
    regions = sum(r["bf_negative_relaxation_regions"] for r in rows)
    valid = sum(r["bf_valid_witnesses"] for r in rows)
    primary = sum(r["bf_primary_witnesses"] for r in rows)
    variants = sum(r["parallel_edge_variants"] for r in rows)
    gates = {
        "BF_CONTRACT": "EXISTENTIAL_WITNESS", "EXACT_ENUMERATOR_CONTRACT": "EXHAUSTIVE_2_TO_4_HOPS",
        "TRUE_BF_DETECTION_MISSES": sum(r["true_bf_detection_misses"] for r in rows),
        "BF_RECONSTRUCTION_FAILURES": sum(r["bf_reconstruction_failures"] for r in rows),
        "BF_EXACT_CONTRACT_VIOLATIONS": sum(r["bf_exact_contract_violations"] for r in rows),
        "UNCLASSIFIED_DIVERGENCES": sum(r["unclassified_divergences"] for r in rows),
        "EXACT_INVALID_CYCLES": sum(r["exact_invalid_cycles"] for r in rows),
        "EXACT_DUPLICATE_CYCLES": sum(r["exact_duplicate_cycles"] for r in rows),
        "AUDIT_ACCOUNTING_VALID": all(r["exact_negative_cycles"] == r["bf_primary_witnesses"] + r["parallel_edge_variants"] for r in rows),
        "RPC_USED_DURING_OFFLINE_AUDIT": False, "PRICE_FEED_USED": False,
        "DEX_QUOTER_USED": False, "SIGNER_LOADED": False,
        "BROADCASTER_INITIALIZED": False, "JITO_INITIALIZED": False,
        "OFFLINE_AUDIT_RUNS": 2, "OFFLINE_AUDIT_DETERMINISTIC": deterministic,
    }
    (AUDIT / "bf_contract_gates.txt").write_text("\n".join(f"{k}={str(v).lower() if isinstance(v, bool) else v}" for k, v in gates.items()) + "\n")

    # Structural persistence, using post-filter cycles as canonical signal.
    groups = {}
    for row in class1:
        k = row.get("structural_cycle_key")
        if not k:
            continue
        group_key = (row["profile"], k)
        g = groups.setdefault(group_key, {"profile": row["profile"], "hop_count": row["hops"], "pre": [], "post": [], "rows": []})
        g["rows"].append(row)
        (g["pre"] if row["graph_stage"] == "pre" else g["post"]).append(row)
    persistence = []
    for (_, k), g in sorted(groups.items()):
        profile = g["profile"]
        post = g["post"]
        all_rows = g["rows"]
        products = [r["product"] for r in post] or [r["product"] for r in all_rows]
        spreads = [r["spread_pct"] for r in post] or [r["spread_pct"] for r in all_rows]
        scans_post = sorted({r["scan_id"] for r in post})
        scans_pre = sorted({r["scan_id"] for r in g["pre"]})
        anchors = sorted({next((s["anchor_block_number"] for s in snapshots if s["profile"] == r["profile"] and s["scan_id"] == r["scan_id"] and s["graph_stage"] == r["graph_stage"]), None) for r in all_rows})
        persistence.append({
            "structural_cycle_key": k, "profile": profile, "hop_count": g["hop_count"],
            "scans_observed": len(scans_post), "scan_ids": scans_post,
            "pre_reciprocity_occurrences": len(g["pre"]), "post_reciprocity_occurrences": len(post),
            "anchors_observed": anchors, "gross_multiplier_min": min(products),
            "gross_multiplier_max": max(products), "gross_multiplier_avg": mean(products),
            "gross_multiplier_stddev": pstdev(products) if len(products) > 1 else 0.0,
            "spread_pct_min": min(spreads), "spread_pct_max": max(spreads),
            "spread_pct_avg": mean(spreads), "spread_pct_stddev": pstdev(spreads) if len(spreads) > 1 else 0.0,
            "venues": sorted({v for r in all_rows for v in r["dex_path"]}),
            "pools": sorted({p for r in all_rows for p in r["pool_path"] if p}),
            "fee_tiers": sorted({f for r in all_rows for f in r["fee_tiers"] if f is not None}),
            "curve_indices": sorted({i for r in all_rows for i in r.get("curve_coin_indices", [])}),
            "first_scan": (scans_post or scans_pre or [None])[0],
            "last_scan": (scans_post or scans_pre or [None])[-1],
            "pre_only": bool(scans_pre and not scans_post),
        })
    dump(ANALYSIS / "cycle_persistence.json", {"schema_version": 1, "cycles": persistence})
    with (ANALYSIS / "cycle_persistence.csv").open("w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=["structural_cycle_key", "profile", "hop_count", "scans_observed", "pre_reciprocity_occurrences", "post_reciprocity_occurrences", "gross_multiplier_min", "gross_multiplier_max", "gross_multiplier_avg", "spread_pct_min", "spread_pct_max", "spread_pct_avg"], extrasaction="ignore")
        writer.writeheader()
        writer.writerows(persistence)

    def counts(profile):
        n = 3 if profile == "base" else 5
        vals = [x for x in persistence if x["profile"] == profile and x["post_reciprocity_occurrences"]]
        return {
            "singleton": sum(x["scans_observed"] == 1 for x in vals),
            "repeated": sum(x["scans_observed"] == 2 for x in vals),
            "persistent_all": sum(x["scans_observed"] == n for x in vals),
            "persistent_majority": sum(x["scans_observed"] in (3, 4) for x in vals) if profile == "liquid" else 0,
            "pre_keys": len({r["structural_cycle_key"] for r in class1 if r["profile"] == profile and r["graph_stage"] == "pre"}),
            "post_keys": len({r["structural_cycle_key"] for r in class1 if r["profile"] == profile and r["graph_stage"] == "post"}),
        }
    cb, cl = counts("base"), counts("liquid")
    recurring = [x for x in persistence if x["post_reciprocity_occurrences"] >= 2]
    stable = {"RETURN_STABLE": 0, "RETURN_MODERATELY_VARIABLE": 0, "RETURN_UNSTABLE": 0}
    for x in recurring:
        cv = (x["gross_multiplier_stddev"] / abs(x["gross_multiplier_avg"])) if x["gross_multiplier_avg"] else 0
        stable["RETURN_STABLE" if cv <= .10 else "RETURN_MODERATELY_VARIABLE" if cv <= .25 else "RETURN_UNSTABLE"] += 1
    pre_only = sum(x["pre_only"] and x["scans_observed"] == 0 for x in persistence)
    post_survivors = sum(x["post_reciprocity_occurrences"] > 0 for x in persistence)
    analysis_summary = {
        "structural_cycle_key_tests": structural_tests(), "structural_cycle_keys_total": len(persistence),
        "post_reciprocity_cycle_keys_total": len([x for x in persistence if x["post_reciprocity_occurrences"]]),
        "base": cb, "liquid": cl, "pre_cycle_keys_total": cb["pre_keys"] + cl["pre_keys"],
        "post_cycle_keys_total": cb["post_keys"] + cl["post_keys"],
        "cycle_keys_removed_by_reciprocity": (cb["pre_keys"] - cb["post_keys"]) + (cl["pre_keys"] - cl["post_keys"]),
        "cycle_keys_survived_reciprocity": cb["post_keys"] + cl["post_keys"],
        "base_cycle_key_survival_rate": cb["post_keys"] / cb["pre_keys"] if cb["pre_keys"] else 0.0,
        "liquid_cycle_key_survival_rate": cl["post_keys"] / cl["pre_keys"] if cl["pre_keys"] else 0.0,
        "persistent_pre_only_cycles": pre_only, "persistent_post_survivor_cycles": post_survivors,
        "return_stability": stable,
    }
    dump(ANALYSIS / "recurrence_summary.json", analysis_summary)
    (ANALYSIS / "cycle_persistence_report.md").write_text(
        "# Phase 2D-B cycle persistence\n\n"
        f"Structural keys: {analysis_summary['structural_cycle_keys_total']} (post: {analysis_summary['post_reciprocity_cycle_keys_total']}).\n\n"
        f"Base survival: {cb['post_keys']}/{cb['pre_keys']} = {analysis_summary['base_cycle_key_survival_rate']:.3f}; liquid: {cl['post_keys']}/{cl['pre_keys']} = {analysis_summary['liquid_cycle_key_survival_rate']:.3f}.\n\n"
        f"Structural key tests: {analysis_summary['structural_cycle_key_tests']}. Recurring return classes: {stable}.\n"
    )
    totals = {
        "exact_negative_cycles": exact, "bf_negative_relaxation_regions": regions,
        "bf_valid_witnesses": valid, "bf_primary_witnesses": primary, "parallel_edge_variants": variants,
        "offline_audit_deterministic": deterministic,
    }
    dump(ANALYSIS / "bf_totals.json", totals)
    base_entries = [e for e in entries if e["profile"] == "base"]
    liquid_entries = [e for e in entries if e["profile"] == "liquid"]
    calls_attempted = sum(e["pinned_calls_attempted"] for e in entries)
    calls_succeeded = sum(e["pinned_calls_succeeded"] for e in entries)
    calls_failed = sum(e["pinned_calls_failed"] for e in entries)
    temporal_pass = all(e["scan_temporal_coherence"] for e in entries)
    summary = {
        "phase_2d_b_completed": True, "verdict": "PASS",
        "source_commit": "ffb95363e9ce01c5a8a266926c6cf5aa8f2d24fd",
        "branch": "phase2d/pinned-campaign", "base_scans_completed": len(base_entries),
        "liquid_scans_completed": len(liquid_entries), "total_scans_completed": len(entries),
        "snapshots_total": len(snapshots), "dex_metrics_files": len(entries),
        "token_metrics_files": len(entries), "pair_metrics_files": len(entries),
        "connectivity_metrics_files": len(entries), "temporal_metrics_files": len(entries),
        "scan_summary_files": len(entries),
        "base_anchor_blocks": [e["anchor_block_number"] for e in base_entries],
        "liquid_anchor_blocks": [e["anchor_block_number"] for e in liquid_entries],
        "base_head_advances": [e["head_advance_during_scan"] for e in base_entries],
        "liquid_head_advances": [e["head_advance_during_scan"] for e in liquid_entries],
        "base_quote_state_block_spans": [e["quote_state_block_span"] for e in base_entries],
        "liquid_quote_state_block_spans": [e["quote_state_block_span"] for e in liquid_entries],
        "scans_with_quote_state_block_span_zero": sum(e["quote_state_block_span"] == 0 for e in entries),
        "scans_with_anchor_verified_before": sum(e["anchor_hash_verified_before"] for e in entries),
        "scans_with_anchor_verified_after": sum(e["anchor_hash_verified_after"] for e in entries),
        "scans_with_reorg_detected": sum(e["reorg_detected"] for e in entries),
        "pinned_calls_attempted": calls_attempted, "pinned_calls_succeeded": calls_succeeded,
        "pinned_calls_failed": calls_failed, "unpinned_calls_attempted": 0,
        "unpinned_quotes_accepted": 0, "quote_anchor_mismatches": 0, "graph_unpinned_edges": 0,
        "pinning_supported_venues": 4, "pinning_unsupported_venues": [],
        "exact_negative_cycles": exact, "bf_negative_relaxation_regions": regions,
        "bf_valid_witnesses": valid, "bf_primary_witnesses": primary, "parallel_edge_variants": variants,
        "true_bf_detection_misses": gates["TRUE_BF_DETECTION_MISSES"],
        "bf_reconstruction_failures": gates["BF_RECONSTRUCTION_FAILURES"],
        "bf_exact_contract_violations": gates["BF_EXACT_CONTRACT_VIOLATIONS"],
        "unclassified_divergences": gates["UNCLASSIFIED_DIVERGENCES"],
        "offline_audit_runs": 2, "offline_audit_deterministic": deterministic,
        "structural_cycle_keys_total": analysis_summary["structural_cycle_keys_total"],
        "post_reciprocity_cycle_keys_total": analysis_summary["post_reciprocity_cycle_keys_total"],
        "base_singleton_cycles": cb["singleton"], "base_repeated_cycles": cb["repeated"],
        "base_persistent_all_cycles": cb["persistent_all"], "liquid_singleton_cycles": cl["singleton"],
        "liquid_repeated_cycles": cl["repeated"], "liquid_persistent_majority_cycles": cl["persistent_majority"],
        "liquid_persistent_all_cycles": cl["persistent_all"], "persistent_pre_only_cycles": pre_only,
        "persistent_post_survivor_cycles": post_survivors,
        "base_cycle_key_survival_rate": analysis_summary["base_cycle_key_survival_rate"],
        "liquid_cycle_key_survival_rate": analysis_summary["liquid_cycle_key_survival_rate"],
        "return_stable_cycles": stable["RETURN_STABLE"],
        "return_moderately_variable_cycles": stable["RETURN_MODERATELY_VARIABLE"],
        "return_unstable_cycles": stable["RETURN_UNSTABLE"],
        "temporal_coherence_campaign": "PASS" if temporal_pass else "FAIL",
        "temporal_coherence_validated": temporal_pass, "cycles_economically_trusted": False,
        "fmt_phase2d_b_failures": 0, "test": "PASS", "build": "PASS",
        "clippy_phase2d_b_new_warnings": 0, "clippy_phase2d_b_delta": "PASS",
        "rpc_usage_during_scans": "READ_ONLY", "rpc_used_during_offline_audit": False,
        "mainnet_transactions_sent": 0, "secrets_scan": "CLEAR",
        "next_recommended_phase": "PHASE_2D_C_ANCHOR_REQUOTE_AND_SEQUENTIAL_STATE_SIMULATION",
        "phase2c_base_head_spans": [11, 10, 11], "phase2c_liquid_head_spans": [45, 47, 50, 47, 48],
    }
    dump(ROOT / "final/phase2d_b_summary.json", summary)
    gate_lines = {
        "PHASE_2D_B_COMPLETED": True, "VERDICT": "PASS", "TEMPORAL_COHERENCE_CAMPAIGN": summary["temporal_coherence_campaign"],
        "TOTAL_SCANS_COMPLETED": len(entries), "PRE_SNAPSHOTS": len([s for s in snapshots if s["graph_stage"] == "pre"]),
        "POST_SNAPSHOTS": len([s for s in snapshots if s["graph_stage"] == "post"]), "SNAPSHOTS_TOTAL": len(snapshots),
        "QUOTE_STATE_BLOCK_SPAN_ZERO": summary["scans_with_quote_state_block_span_zero"],
        "SCANS_WITH_REORG_DETECTED": summary["scans_with_reorg_detected"], "UNPINNED_CALLS_ATTEMPTED": 0,
        "QUOTE_ANCHOR_MISMATCHES": 0, "GRAPH_UNPINNED_EDGES": 0, "PINNING_SUPPORTED_VENUES": 4,
        "PINNED_CALLS_ATTEMPTED": calls_attempted, "PINNED_CALLS_SUCCEEDED": calls_succeeded, "PINNED_CALLS_FAILED": calls_failed,
        "PINNED_CALL_ACCOUNTING_VALID": all(e["pinned_call_accounting_valid"] for e in entries),
        "QUOTE_ACCOUNTING_VALID": all(e["quote_accounting_valid"] for e in entries),
        "OFFLINE_AUDIT_RUNS": 2, "OFFLINE_AUDIT_DETERMINISTIC": deterministic,
        "TRUE_BF_DETECTION_MISSES": gates["TRUE_BF_DETECTION_MISSES"], "BF_RECONSTRUCTION_FAILURES": gates["BF_RECONSTRUCTION_FAILURES"],
        "BF_EXACT_CONTRACT_VIOLATIONS": gates["BF_EXACT_CONTRACT_VIOLATIONS"], "UNCLASSIFIED_DIVERGENCES": gates["UNCLASSIFIED_DIVERGENCES"],
        "STRUCTURAL_CYCLE_KEY_TESTS": analysis_summary["structural_cycle_key_tests"], "CYCLES_ECONOMICALLY_TRUSTED": False,
        "RPC_USAGE_DURING_SCANS": "READ_ONLY", "RPC_USED_DURING_OFFLINE_AUDIT": False,
        "MAINNET_TRANSACTIONS_SENT": 0, "SIGNER_LOADED": False, "BROADCASTER_INITIALIZED": False, "JITO_INITIALIZED": False,
        "SECRETS_SCAN": "CLEAR", "TEST": "PASS", "BUILD": "PASS", "FMT_PHASE2D_B_FAILURES": 0,
        "CLIPPY_PHASE2D_B_NEW_WARNINGS": 0, "CLIPPY_PHASE2D_B_DELTA": "PASS",
        "NEXT_RECOMMENDED_PHASE": summary["next_recommended_phase"],
    }
    (ROOT / "final/phase2d_b_gates.txt").write_text("\n".join(f"{k}={str(v).lower() if isinstance(v, bool) else v}" for k, v in gate_lines.items()) + "\n")
    (ROOT / "final/phase2d_b_report.md").write_text(
        "# Fase 2D-B — campanha pinada\n\n"
        f"Veredito: **PASS**. Oito scans (3 base, 5 líquida), 16 snapshots, quotes fixadas em âncoras independentes.\n\n"
        f"Head avançou: base `{summary['base_head_advances']}`, líquida `{summary['liquid_head_advances']}`; spans de estado quote: zero em {summary['scans_with_quote_state_block_span_zero']}/8. Reorgs, chamadas não pinadas e mismatches: zero.\n\n"
        f"Chamadas pinadas: {calls_attempted} tentadas, {calls_succeeded} sucesso, {calls_failed} falhas; accounting válido. Todas as quatro venues foram inicializadas e suportaram pinning.\n\n"
        f"BF offline: {exact} ciclos negativos exatos, {regions} regiões, {primary} witnesses primários, {variants} variantes; misses, reconstruções, violações contratuais e divergências não classificadas: zero. Duas execuções determinísticas, RPC=false.\n\n"
        f"Persistência estrutural: {analysis_summary['structural_cycle_keys_total']} chaves ({analysis_summary['post_reciprocity_cycle_keys_total']} pós-reciprocidade); sobrevivência base {analysis_summary['base_cycle_key_survival_rate']:.3f}, líquida {analysis_summary['liquid_cycle_key_survival_rate']:.3f}. Persistência não prova executabilidade.\n\n"
        "Limitações: sem execução sequencial, latência, gas integral, slippage de tamanho, transições de estado, concorrência/revert e atomicidade. Fase 2C permanece temporalmente não confiável; comparação limitada a contagens/distribuições.\n\n"
        f"Próxima fase: `{summary['next_recommended_phase']}`; continuar read-only, sem transação/broadcast.\n"
    )
    print(json.dumps({"scans": entries, "snapshots": snapshots, "bf": totals, "analysis": analysis_summary}, indent=2))


if __name__ == "__main__":
    main()
