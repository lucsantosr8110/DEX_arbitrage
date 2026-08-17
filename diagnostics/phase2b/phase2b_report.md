# Phase 2B — Reciprocity Filter Audit

## Scope

Read-only Polygon quotes only; no signer, broadcaster, Jito, transaction, or bundle.

## Scan Window and Graphs

The ten-scan window captured 400 valid directional quotes. Pre-reciprocity
graphs retained 400 edges; post-filter graphs retained 300. The filter rejected
100 quotes (25.0%).

## Negative-Cycle Analysis

Pre and post graphs both contained zero negative cycles. BF and the exact
enumerator had zero divergences. Therefore
`RECIPROCITY_VERDICT=NO_NEGATIVE_CYCLES_IN_WINDOW`.

## Rejection Matrix and Pairing

The machine-readable matrix is `reciprocity_rejection_matrix.json`. Pool and
fee metadata are unavailable in the current read-only quotes, so all 100
comparisons are classified `UNKNOWN_POOL`; no claim of same-pool or cross-DEX
pairing is made.

## Temporal Coherence

Quotes are not pinned to an anchor block and have no per-quote block tag. The
final two-scan validation measured block spans 8 and 8 over roughly 11 seconds.
Thus `RECIPROCITY_MODEL_VERDICT=TEMPORAL_COHERENCE_UNAVAILABLE` and
`RECIPROCITY_BLOCK_CLASSIFICATION=UNAVAILABLE`.

## Replay, Tests, Safety

20 snapshots were replayed twice: 40 offline deterministic runs, zero failures.
Ten Phase-2B unit tests cover graph inclusion/exclusion, cycles, isolation,
grouping, pairing and aggregation. Safety remained read-only.

## Next Phase

`AMPLIAR_UNIVERSO_READ_ONLY`.
