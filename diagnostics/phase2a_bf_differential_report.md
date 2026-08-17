# Phase 2A — Bellman-Ford Differential Audit

## Verdict

`BF_VERDICT=NO_NEGATIVE_CYCLE_CONFIRMED`. The real Polygon read-only snapshot
has no negative cycle in exact enumeration or raw Bellman-Ford.

## Capture and graph

- Safety: diagnostic mode enabled; live trading/broadcast disabled; signer,
  broadcaster, and Jito uninitialized; zero transactions sent.
- Scan: 5 tokens, 40 raw directional quotes, 30 accepted, 10 rejected by
  reciprocity.
- Multigraph: 30 directed edges, 5 tokens, 0 invalid edges, 10 parallel-edge
  groups, and 10 bidirectional pair groups.
- Snapshot and comparison JSON are nonempty and valid.

## Differential metrics

| Metric | Value |
| --- | ---: |
| Exact cycles: 2 / 3 / 4 hops | 25 / 68 / 138 |
| Exact negative cycles | 0 |
| BF negative relaxations | 0 |
| BF reconstruction attempts/successes/failures | 0 / 0 / 0 |
| BF raw negative cycles | 0 |
| Cycles in both / exact-only / BF-only | 0 / 0 / 0 |

`COLLAPSED_EDGE_COUNT = DIAGNOSTIC_GRAPH_EDGES - LEGACY_PRICE_MAP_ENTRIES`.
This scan is `30 - 30 = 0`; the legacy map is keyed by DEX and directed pair,
so these values happen to be comparable here. Future grouping changes must not
assume that equivalence.

The diagnostic graph preserves accepted quotes as directed `EdgeId` records,
including raw amounts, rate, DEX/protocol, and provenance. The diagnostic BF
uses predecessor `EdgeId` and creates no synthetic inverse edge. The legacy
production detector remains isolated and still needs separate reconciliation.

## Validation

- Added five tests: profitable two-leg, missing return direction, parallel
  `EdgeId`s, JSON round-trip, deterministic replay.
- Format, full tests, and release build pass. Tests: 293 passed, 1 ignored,
  plus 2 integration tests passed.
- Two offline replays are byte-identical and initialize no RPC, price feed,
  DEX quoter, signer, broadcaster, or Jito client.
- Full Clippy remains `FAIL_LEGACY` with 127 existing library errors; zero
  findings reference the Phase 2A files.

## Evidence

- `phase2a_readonly_live_run.txt`
- `phase2a_scan_metrics.txt`
- `phase2a_graph_snapshot.json`
- `phase2a_cycle_comparison.json`
- `phase2a_replay_run_1.txt`, `phase2a_replay_run_2.txt`, `phase2a_replay_diff.txt`
- `phase2a_clippy.txt`
