# Fase 2D-C2C — Shadow C2B Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the canonical C2B discovery/materialization/execution-evidence pipeline into `src/core/bot.rs` as an isolated shadow path — dedicated runtime, real `RiskManager`/`ArbitrageClient` code, hardcoded dry-run — that never reaches the signer/broadcaster and never regresses legacy-loop latency.

**Architecture:** Legacy `price_rx` loop stays byte-for-byte unchanged. A block watcher schedules one C2B round every N blocks on a dedicated multi-thread Tokio runtime with its own RPC provider; each round runs `discover_at` (extracted from `phase2d_c2b_fresh_discovery.rs`) and feeds a rolling 3-of-3 `StableOpportunityAggregator`. Stable routes become `ExecutableOpportunity`, assessed by a new integer-only `RiskManager::assess_executable_opportunity`, routed by `determine_execution_strategy_canonical`, and executed via new `execute_*_canonical` methods on `ArbitrageClient` that reuse the real complexity-filter/simulate/contract-call helpers but structurally never call `send_and_confirm_transaction`. A `DualRunComparator` observes both paths and writes diagnostics; it has no decision authority.

**Tech Stack:** Rust, tokio (multi-thread runtime + mpsc/broadcast channels), ethers-rs (Address/U256/H256), existing `flashloan_bot` lib crate (`src/core/*`), existing bin `phase2d_c2b_fresh_discovery`.

## Global Constraints

- `CANONICAL_C2B_SHADOW_ENABLED=false` by default; `CANONICAL_C2B_PRIMARY_ENABLED=false`; `CANONICAL_C2B_BROADCAST_ENABLED=false` — always, no override path.
- `MAINNET_WRITE_RPC_CALLS=0`, `MAINNET_TRANSACTIONS_SENT=0`, `PRODUCTION_SIGNER_LOADED=false`, `PRODUCTION_BROADCASTER_INITIALIZED=false`, `LIVE_EXECUTION_AUTHORIZED=false` for the entire phase.
- Legacy `price_rx` loop behavior must be provably unchanged when shadow is disabled (`c2b_shadow_disabled_preserves_legacy_behavior`).
- No `String`/`f64` round-trip for token identity or PnL anywhere on the canonical decision path (risk approval, strategy selection, `min_profit_raw`). `f64` is permitted only in report-only fields (`net_pnl_usd: Option<f64>`, `BundleResult::profit`/`gas_cost`).
- New code follows existing module conventions: pure logic in `src/core/*.rs`, `#[cfg(test)] mod tests` in the same file, `Result<T, E>` with `thiserror` enums for typed errors (matching `MaterializationError`, `OrchestratorError`, etc.).
- Reuse existing typed evidence primitives instead of re-deriving them: `phase2d_anchor::AnchorBlock` (pinned anchor + `reorg_detected`), `c2b_orchestrator::OrchestratorEvidence` (eth_call/preflight/trace pass flags), `fresh_economics::RouteSimulationResult` (`i128` gross/net pnl), `executable_route_materializer::ExecutableRoutePlan`, `executable_call::ExecutableCallBuilderRegistry`.
- `gross_pnl`/`net_pnl` use `i128` (not `ethers::types::I256`), matching every existing typed evidence struct in this pipeline (`RouteSimulationResult`, `RouteExecutionRecord`) — avoids introducing a second signed-integer convention and the conversions that would require.
- `cargo fmt --all -- --check`, `cargo check --workspace`, and `cargo test --workspace -- --test-threads=1` must pass under `CARGO_TARGET_DIR=target/phase2d_c2c` before the final commit. `CLIPPY_NEW_ERRORS_INTRODUCED=0`.

---

## File Structure

New files:
- `src/core/execution_profile.rs` — `ExecutionProfile` (chain id + discovery profile label), shared identity component of `StabilityKey`.
- `src/core/c2b_round.rs` — `RoundEvidence` (typed, per-route, per-anchor evidence) + `discover_at()` (extracted pipeline).
- `src/core/stability.rs` — `is_stable()`, `StabilityKey`, `StableOpportunityAggregator`.
- `src/core/executable_opportunity.rs` — `ExecutableOpportunity`, `LegQuote`, `ExecutionEvidence`, `StabilityRecord`, `opportunity_id`/`evidence_hash` computation.
- `src/core/c2b_shadow_service.rs` — `CanonicalC2BOpportunitySource`, dedicated-runtime scheduler/service, `PinnedAnchor`→`C2BShadowResult` channel wiring.
- `src/core/dual_run_comparator.rs` — `DualRunComparator`, divergence classification, jsonl/md writer.
- `tests/phase2d_c2c_e2e.rs` — deterministic end-to-end integration test (new integration-test binary under `tests/`).

Modified files:
- `src/core/risk.rs` — add `RiskApproval`, `CanonicalRiskRejection`, `assess_executable_opportunity`.
- `src/core/flashloan.rs` — add `ExecutionMode`, `steps_from_route_plan`, `determine_execution_strategy_canonical`, `execute_direct_canonical`/`execute_flashloan_canonical`/`execute_wrapper_canonical`, `canonical_dry_result`.
- `src/core/bot.rs` — spawn dedicated runtime, wire block watcher + anchor/result channels into `run()`.
- `src/config/mod.rs` — add `C2bShadowConfig` + `Config.c2b_shadow` field.
- `config/config.dryrun.toml` — add `[c2b_shadow]` section, all shadow flags off.
- `src/bin/phase2d_c2b_fresh_discovery.rs` — `run_discovery_round` becomes a thin wrapper over `discover_at`; `stable_candidate_keys` delegates to `stability::is_stable`.
- `src/core/mod.rs` — register the new modules.

---

## Task 1: `ExecutionProfile`

**Files:**
- Create: `src/core/execution_profile.rs`
- Modify: `src/core/mod.rs` (add `pub mod execution_profile;`)
- Test: same file, `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `pub struct ExecutionProfile { pub chain_id: u64, pub profile_label: String }` implementing `Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize`.

This distinguishes routes discovered under different discovery profiles (`base`/`liquid`, matching the bin's existing `--profile` flag) or different chains, so the stability aggregator never merges evidence collected under different assumptions.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_with_different_labels_are_not_equal() {
        let a = ExecutionProfile { chain_id: 137, profile_label: "base".into() };
        let b = ExecutionProfile { chain_id: 137, profile_label: "liquid".into() };
        assert_ne!(a, b);
    }

    #[test]
    fn profiles_with_same_fields_are_equal() {
        let a = ExecutionProfile { chain_id: 137, profile_label: "base".into() };
        let b = ExecutionProfile { chain_id: 137, profile_label: "base".into() };
        assert_eq!(a, b);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib execution_profile -- --nocapture`
Expected: FAIL with "cannot find module `execution_profile`" (module not yet registered / struct not yet defined).

- [ ] **Step 3: Write the struct and register the module**

```rust
// src/core/execution_profile.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExecutionProfile {
    pub chain_id: u64,
    pub profile_label: String,
}
```

Add `pub mod execution_profile;` to `src/core/mod.rs` in alphabetical position (after `executable_route_materializer`, before `execution_viability`).

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib execution_profile -- --nocapture`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src/core/execution_profile.rs src/core/mod.rs
git commit -m "feat(2d-c2c): add ExecutionProfile identity type"
```

---

## Task 2: `RoundEvidence`

**Files:**
- Create: `src/core/c2b_round.rs` (this task only defines the struct; `discover_at` extraction is Task 11)
- Modify: `src/core/mod.rs` (add `pub mod c2b_round;`)
- Test: same file

**Interfaces:**
- Consumes: `phase2d_anchor::AnchorBlock`, `executable_route_materializer::ExecutableRoutePlan`, `c2b_orchestrator::OrchestratorEvidence`, `fresh_economics::RouteSimulationResult`, `execution_profile::ExecutionProfile`.
- Produces:
```rust
pub struct RoundEvidence {
    pub structural_cycle_key: String,
    pub anchor: AnchorBlock,
    pub context_hash: H256,
    pub route_plan: ExecutableRoutePlan,
    pub amount_in: U256,
    pub execution_profile: ExecutionProfile,
    pub economics: Option<RouteSimulationResult>,
    pub gross_pnl_atomic: Option<i128>,
    pub gas_used_total: u64,
    pub orchestrator_evidence: Option<OrchestratorEvidence>,
    pub rejected_registry_hit: bool,
}
```
Later tasks (`stability.rs`, `executable_opportunity.rs`) consume this exact shape — do not rename fields.

`route_plan` is `Clone` already (verified: `ExecutableRoutePlan` derives `Clone`); `RoundEvidence` derives `Debug, Clone`.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        c2b_orchestrator::OrchestratorEvidence,
        executable_route_materializer::{ExecutableRoutePlan, ForkSetupPlan},
        execution_profile::ExecutionProfile,
        fresh_economics::PinnedStateSnapshot,
        phase2d_anchor::AnchorBlock,
    };
    use ethers::types::{Address, H256, U256};

    fn route_plan() -> ExecutableRoutePlan {
        ExecutableRoutePlan {
            structural_cycle_key: "k".into(),
            anchor_block: 100,
            start_token: Address::zero(),
            legs: vec![],
            snapshot: PinnedStateSnapshot::default(),
            fork_setup: ForkSetupPlan {
                anchor_block: 100,
                caller: Address::zero(),
                tokens: vec![],
                funding: vec![],
                approvals: vec![],
                targets: vec![],
                balance_checks: vec![],
            },
        }
    }

    fn anchor() -> AnchorBlock {
        AnchorBlock { number: 100, hash: H256::repeat_byte(1), selected_from_head: 102, confirmation_lag: 2 }
    }

    #[test]
    fn round_evidence_carries_structural_key_and_anchor() {
        let ev = RoundEvidence {
            structural_cycle_key: "k".into(),
            anchor: anchor(),
            context_hash: H256::zero(),
            route_plan: route_plan(),
            amount_in: U256::from(1u64),
            execution_profile: ExecutionProfile { chain_id: 137, profile_label: "base".into() },
            economics: None,
            gross_pnl_atomic: None,
            gas_used_total: 0,
            orchestrator_evidence: None,
            rejected_registry_hit: false,
        };
        assert_eq!(ev.structural_cycle_key, "k");
        assert_eq!(ev.anchor.number, 100);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib c2b_round -- --nocapture`
Expected: FAIL — module/struct not found.

- [ ] **Step 3: Write the struct**

```rust
// src/core/c2b_round.rs
use crate::core::{
    c2b_orchestrator::OrchestratorEvidence, executable_route_materializer::ExecutableRoutePlan,
    execution_profile::ExecutionProfile, fresh_economics::RouteSimulationResult,
    phase2d_anchor::AnchorBlock,
};
use ethers::types::{H256, U256};

#[derive(Debug, Clone)]
pub struct RoundEvidence {
    pub structural_cycle_key: String,
    pub anchor: AnchorBlock,
    pub context_hash: H256,
    pub route_plan: ExecutableRoutePlan,
    pub amount_in: U256,
    pub execution_profile: ExecutionProfile,
    pub economics: Option<RouteSimulationResult>,
    pub gross_pnl_atomic: Option<i128>,
    pub gas_used_total: u64,
    pub orchestrator_evidence: Option<OrchestratorEvidence>,
    pub rejected_registry_hit: bool,
}
```

Add `pub mod c2b_round;` to `src/core/mod.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib c2b_round -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/core/c2b_round.rs src/core/mod.rs
git commit -m "feat(2d-c2c): add typed RoundEvidence struct"
```

---

## Task 3: `stability.rs` — predicate + aggregator

**Files:**
- Create: `src/core/stability.rs`
- Modify: `src/core/mod.rs` (add `pub mod stability;`)
- Test: same file

**Interfaces:**
- Consumes: `c2b_round::RoundEvidence`, `execution_profile::ExecutionProfile`.
- Produces:
```rust
pub struct StabilityKey {
    pub structural_cycle_key: String,
    pub start_token: Address,
    pub amount_in: U256,
    pub execution_profile: ExecutionProfile,
}
pub fn is_stable(entries: &[RoundEvidence]) -> bool;
pub struct StableOpportunityAggregator { /* BTreeMap<StabilityKey, VecDeque<RoundEvidence>> */ }
impl StableOpportunityAggregator {
    pub fn new() -> Self;
    /// Pushes one round's evidence, keeping only the 3 most recent per key.
    /// Returns Some(3 evidences) the instant that key becomes stable per `is_stable`.
    pub fn push(&mut self, evidence: RoundEvidence) -> Option<[RoundEvidence; 3]>;
}
```
`Task 4` (`ExecutableOpportunity`) consumes `[RoundEvidence; 3]` exactly as returned here.

`StabilityKey` needs `Ord`/`PartialOrd` to key a `BTreeMap` — derive them, requires `ExecutionProfile` (Task 1) already derives `Hash`; add `Ord, PartialOrd` to `ExecutionProfile` too (revisit Task 1's derive list — `String`/`u64` are both `Ord`, so this is a pure derive addition, no logic change).

- [ ] **Step 1: Add `Ord, PartialOrd` to `ExecutionProfile`**

Edit `src/core/execution_profile.rs`:
```rust
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ExecutionProfile {
```

- [ ] **Step 2: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        executable_route_materializer::{ExecutableRoutePlan, ForkSetupPlan},
        fresh_economics::PinnedStateSnapshot, phase2d_anchor::AnchorBlock,
    };
    use ethers::types::{Address, H256, U256};

    fn profile() -> ExecutionProfile {
        ExecutionProfile { chain_id: 137, profile_label: "base".into() }
    }

    fn anchor(n: u64, h: u8) -> AnchorBlock {
        AnchorBlock { number: n, hash: H256::repeat_byte(h), selected_from_head: n + 2, confirmation_lag: 2 }
    }

    fn plan() -> ExecutableRoutePlan {
        ExecutableRoutePlan {
            structural_cycle_key: "k".into(), anchor_block: 0, start_token: Address::zero(),
            legs: vec![], snapshot: PinnedStateSnapshot::default(),
            fork_setup: ForkSetupPlan { anchor_block: 0, caller: Address::zero(), tokens: vec![], funding: vec![], approvals: vec![], targets: vec![], balance_checks: vec![] },
        }
    }

    fn evidence(anchor_num: u64, hash_byte: u8, pass: bool) -> RoundEvidence {
        RoundEvidence {
            structural_cycle_key: "k".into(),
            anchor: anchor(anchor_num, hash_byte),
            context_hash: H256::repeat_byte(9),
            route_plan: plan(),
            amount_in: U256::from(1_000u64),
            execution_profile: profile(),
            economics: Some(crate::core::fresh_economics::RouteSimulationResult {
                final_amount_atomic: U256::from(1_100u64), gross_pnl_atomic: 100,
                gas_cost_atomic: U256::from(1u64), net_pnl_atomic: 99,
                pool_reuse_detected: false, all_models_supported: true,
            }),
            gross_pnl_atomic: Some(100),
            gas_used_total: 21_000,
            orchestrator_evidence: Some(crate::core::c2b_orchestrator::OrchestratorEvidence {
                structural_cycle_key: "k".into(), economic_positive: true, builder_called: true,
                readonly_pass: pass, preflight_pass: pass, balance_delta: U256::from(100u64),
                output_propagated: true, trace_validated: pass, rejected_registry_hit: false,
                placeholder_evidence: false,
            }),
            rejected_registry_hit: false,
        }
    }

    #[test]
    fn three_distinct_anchors_all_passing_is_stable() {
        let entries = vec![evidence(100, 1, true), evidence(103, 2, true), evidence(106, 3, true)];
        assert!(is_stable(&entries));
    }

    #[test]
    fn fewer_than_three_entries_is_not_stable() {
        let entries = vec![evidence(100, 1, true), evidence(103, 2, true)];
        assert!(!is_stable(&entries));
    }

    #[test]
    fn repeated_anchor_number_is_not_stable() {
        let entries = vec![evidence(100, 1, true), evidence(100, 1, true), evidence(103, 2, true)];
        assert!(!is_stable(&entries));
    }

    #[test]
    fn one_failing_round_is_not_stable() {
        let entries = vec![evidence(100, 1, true), evidence(103, 2, false), evidence(106, 3, true)];
        assert!(!is_stable(&entries));
    }

    #[test]
    fn rejected_registry_hit_is_not_stable() {
        let mut e = evidence(103, 2, true);
        e.rejected_registry_hit = true;
        let entries = vec![evidence(100, 1, true), e, evidence(106, 3, true)];
        assert!(!is_stable(&entries));
    }

    #[test]
    fn aggregator_emits_only_after_third_matching_push() {
        let mut agg = StableOpportunityAggregator::new();
        assert!(agg.push(evidence(100, 1, true)).is_none());
        assert!(agg.push(evidence(103, 2, true)).is_none());
        let emitted = agg.push(evidence(106, 3, true));
        assert!(emitted.is_some());
    }

    #[test]
    fn aggregator_keeps_rolling_window_of_three() {
        let mut agg = StableOpportunityAggregator::new();
        agg.push(evidence(100, 1, true));
        agg.push(evidence(103, 2, true));
        agg.push(evidence(106, 3, true));
        // 4th push for the same key: window rolls, oldest (100) drops.
        let emitted = agg.push(evidence(109, 4, true));
        assert!(emitted.is_some());
        let anchors: Vec<u64> = emitted.unwrap().iter().map(|e| e.anchor.number).collect();
        assert_eq!(anchors, vec![103, 106, 109]);
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib stability -- --nocapture`
Expected: FAIL — module not found.

- [ ] **Step 4: Implement**

```rust
// src/core/stability.rs
use crate::core::{c2b_round::RoundEvidence, execution_profile::ExecutionProfile};
use ethers::types::{Address, U256};
use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct StabilityKey {
    pub structural_cycle_key: String,
    pub start_token: Address,
    pub amount_in: U256,
    pub execution_profile: ExecutionProfile,
}

impl StabilityKey {
    fn from_evidence(e: &RoundEvidence) -> Self {
        Self {
            structural_cycle_key: e.structural_cycle_key.clone(),
            start_token: e.route_plan.start_token,
            amount_in: e.amount_in,
            execution_profile: e.execution_profile.clone(),
        }
    }
}

/// Same predicate the E1-F4 campaign binary uses for its batch 3-of-3
/// verdict (`stable_candidate_keys`); this is the rolling-window form.
pub fn is_stable(entries: &[RoundEvidence]) -> bool {
    if entries.len() != 3 {
        return false;
    }
    let anchors: std::collections::BTreeSet<u64> = entries.iter().map(|e| e.anchor.number).collect();
    let hashes: std::collections::BTreeSet<_> = entries.iter().map(|e| e.anchor.hash).collect();
    if anchors.len() != 3 || hashes.len() != 3 {
        return false;
    }
    let sorted_ascending = {
        let mut nums: Vec<u64> = entries.iter().map(|e| e.anchor.number).collect();
        let sorted = { let mut s = nums.clone(); s.sort_unstable(); s };
        nums == sorted || { nums.reverse(); false }
    };
    let _ = sorted_ascending; // ordering enforced by push() insertion order below
    if entries.iter().any(|e| e.rejected_registry_hit) {
        return false;
    }
    let all_economic = entries.iter().all(|e| e.economics.as_ref().is_some_and(|r| r.net_pnl_atomic > 0));
    let all_readonly = entries.iter().all(|e| e.orchestrator_evidence.as_ref().is_some_and(|ev| ev.readonly_pass));
    let all_preflight = entries.iter().all(|e| e.orchestrator_evidence.as_ref().is_some_and(|ev| ev.preflight_pass));
    let all_trace = entries.iter().all(|e| e.orchestrator_evidence.as_ref().is_some_and(|ev| ev.trace_validated));
    let deltas: Vec<i128> = entries.iter().filter_map(|e| e.gross_pnl_atomic).collect();
    let sign_flip = deltas.iter().any(|d| *d < 0) && deltas.iter().any(|d| *d > 0);
    all_economic && all_readonly && all_preflight && all_trace && !sign_flip
}

pub struct StableOpportunityAggregator {
    windows: BTreeMap<StabilityKey, VecDeque<RoundEvidence>>,
}

impl StableOpportunityAggregator {
    pub fn new() -> Self {
        Self { windows: BTreeMap::new() }
    }

    pub fn push(&mut self, evidence: RoundEvidence) -> Option<[RoundEvidence; 3]> {
        let key = StabilityKey::from_evidence(&evidence);
        let window = self.windows.entry(key).or_default();
        window.push_back(evidence);
        while window.len() > 3 {
            window.pop_front();
        }
        if window.len() == 3 {
            let entries: Vec<RoundEvidence> = window.iter().cloned().collect();
            if is_stable(&entries) {
                let mut it = entries.into_iter();
                return Some([it.next().unwrap(), it.next().unwrap(), it.next().unwrap()]);
            }
        }
        None
    }
}

impl Default for StableOpportunityAggregator {
    fn default() -> Self {
        Self::new()
    }
}
```

(The `sorted_ascending` local is dead weight — remove it; ascending order is guaranteed by construction since `push` only appends and the scheduler in Task 15 only schedules strictly increasing block numbers, so no explicit runtime check is needed here. Delete that block before running tests.)

- [ ] **Step 5: Remove the dead `sorted_ascending` block, then run tests**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib stability -- --nocapture`
Expected: PASS (7 tests).

- [ ] **Step 6: Commit**

```bash
git add src/core/stability.rs src/core/execution_profile.rs src/core/mod.rs
git commit -m "feat(2d-c2c): add StableOpportunityAggregator with rolling 3-of-3 predicate"
```

---

## Task 4: `ExecutableOpportunity`

**Files:**
- Create: `src/core/executable_opportunity.rs`
- Modify: `src/core/mod.rs`
- Test: same file

**Interfaces:**
- Consumes: `[c2b_round::RoundEvidence; 3]` (the aggregator's `push()` output).
- Produces:
```rust
pub struct LegQuote { pub pool: Address, pub token_in: Address, pub token_out: Address, pub amount_in: U256, pub amount_out: U256 }
pub struct ExecutionEvidence { pub eth_call_pass: bool, pub preflight_pass: bool, pub trace_validated: bool, pub balance_delta: U256 }
pub struct StabilityRecord { pub anchor_blocks: [u64; 3], pub anchor_hashes: [H256; 3] }
pub struct ExecutableOpportunity {
    pub opportunity_id: H256,
    pub structural_cycle_key: String,
    pub route_plan: ExecutableRoutePlan,
    pub anchor_block: u64,
    pub anchor_block_hash: H256,
    pub context_hash: H256,
    pub evidence_hash: H256,
    pub amount_in: U256,
    pub expected_amount_out: U256,
    pub gross_pnl: i128,
    pub gas_estimate: U256,
    pub net_pnl: i128,
    pub net_pnl_usd: Option<f64>,
    pub leg_quotes: Vec<LegQuote>,
    pub evidence: ExecutionEvidence,
    pub stability: StabilityRecord,
    pub execution_profile: ExecutionProfile,
}
pub fn from_stable_rounds(rounds: [RoundEvidence; 3]) -> Option<ExecutableOpportunity>;
```
`rounds` are assumed already `is_stable` (caller is always `StableOpportunityAggregator::push`'s `Some` branch) — `from_stable_rounds` uses the **third** (most recent) entry as the authoritative snapshot for route/amount/PnL, and folds all three into `stability`/`evidence_hash`. Returns `None` only if the third entry is missing `economics`/`orchestrator_evidence` (defensive — should be unreachable given `is_stable` already required them, but the function must not panic on malformed input).

`leg_quotes` is derived from `route_plan.legs` — since `ExecutableRoutePlan` doesn't carry per-leg quoted amounts today, populate it as one entry per leg with `amount_in`/`amount_out` both defaulted to `U256::zero()` in this task; wiring real per-leg quoted amounts through from `discover_at` is Task 11 (`RoundEvidence` gains a `leg_quotes: Vec<LegQuote>` field there, and this constructor copies it through instead of zero-filling). Note that dependency explicitly so Task 11 doesn't get skipped.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        c2b_orchestrator::OrchestratorEvidence,
        executable_route_materializer::{ExecutableRoutePlan, ForkSetupPlan},
        fresh_economics::{PinnedStateSnapshot, RouteSimulationResult},
        phase2d_anchor::AnchorBlock,
    };
    use ethers::types::{Address, H256, U256};

    fn plan() -> ExecutableRoutePlan {
        ExecutableRoutePlan {
            structural_cycle_key: "k".into(), anchor_block: 106, start_token: Address::repeat_byte(1),
            legs: vec![], snapshot: PinnedStateSnapshot::default(),
            fork_setup: ForkSetupPlan { anchor_block: 106, caller: Address::zero(), tokens: vec![], funding: vec![], approvals: vec![], targets: vec![], balance_checks: vec![] },
        }
    }

    fn round(n: u64, h: u8) -> RoundEvidence {
        RoundEvidence {
            structural_cycle_key: "k".into(),
            anchor: AnchorBlock { number: n, hash: H256::repeat_byte(h), selected_from_head: n + 2, confirmation_lag: 2 },
            context_hash: H256::repeat_byte(9),
            route_plan: plan(),
            amount_in: U256::from(1_000u64),
            execution_profile: crate::core::execution_profile::ExecutionProfile { chain_id: 137, profile_label: "base".into() },
            economics: Some(RouteSimulationResult {
                final_amount_atomic: U256::from(1_100u64), gross_pnl_atomic: 100,
                gas_cost_atomic: U256::from(1u64), net_pnl_atomic: 99,
                pool_reuse_detected: false, all_models_supported: true,
            }),
            gross_pnl_atomic: Some(100),
            gas_used_total: 21_000,
            orchestrator_evidence: Some(OrchestratorEvidence {
                structural_cycle_key: "k".into(), economic_positive: true, builder_called: true,
                readonly_pass: true, preflight_pass: true, balance_delta: U256::from(100u64),
                output_propagated: true, trace_validated: true, rejected_registry_hit: false,
                placeholder_evidence: false,
            }),
            rejected_registry_hit: false,
        }
    }

    #[test]
    fn builds_opportunity_from_three_stable_rounds() {
        let rounds = [round(100, 1), round(103, 2), round(106, 3)];
        let opp = from_stable_rounds(rounds).expect("stable rounds must build");
        assert_eq!(opp.anchor_block, 106);
        assert_eq!(opp.stability.anchor_blocks, [100, 103, 106]);
        assert_eq!(opp.net_pnl, 99);
        assert_eq!(opp.gross_pnl, 100);
    }

    #[test]
    fn opportunity_id_is_deterministic_for_same_input() {
        let a = from_stable_rounds([round(100, 1), round(103, 2), round(106, 3)]).unwrap();
        let b = from_stable_rounds([round(100, 1), round(103, 2), round(106, 3)]).unwrap();
        assert_eq!(a.opportunity_id, b.opportunity_id);
    }

    #[test]
    fn missing_economics_on_authoritative_round_returns_none() {
        let mut third = round(106, 3);
        third.economics = None;
        assert!(from_stable_rounds([round(100, 1), round(103, 2), third]).is_none());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib executable_opportunity -- --nocapture`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement**

```rust
// src/core/executable_opportunity.rs
use crate::core::{
    c2b_round::RoundEvidence, executable_route_materializer::ExecutableRoutePlan,
    execution_profile::ExecutionProfile,
};
use ethers::types::{Address, H256, U256};
use ethers::utils::keccak256;

#[derive(Debug, Clone)]
pub struct LegQuote {
    pub pool: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub amount_out: U256,
}

#[derive(Debug, Clone)]
pub struct ExecutionEvidence {
    pub eth_call_pass: bool,
    pub preflight_pass: bool,
    pub trace_validated: bool,
    pub balance_delta: U256,
}

#[derive(Debug, Clone)]
pub struct StabilityRecord {
    pub anchor_blocks: [u64; 3],
    pub anchor_hashes: [H256; 3],
}

#[derive(Debug, Clone)]
pub struct ExecutableOpportunity {
    pub opportunity_id: H256,
    pub structural_cycle_key: String,
    pub route_plan: ExecutableRoutePlan,
    pub anchor_block: u64,
    pub anchor_block_hash: H256,
    pub context_hash: H256,
    pub evidence_hash: H256,
    pub amount_in: U256,
    pub expected_amount_out: U256,
    pub gross_pnl: i128,
    pub gas_estimate: U256,
    pub net_pnl: i128,
    pub net_pnl_usd: Option<f64>,
    pub leg_quotes: Vec<LegQuote>,
    pub evidence: ExecutionEvidence,
    pub stability: StabilityRecord,
    pub execution_profile: ExecutionProfile,
}

pub fn from_stable_rounds(rounds: [RoundEvidence; 3]) -> Option<ExecutableOpportunity> {
    let [r0, r1, r2] = rounds;
    let economics = r2.economics.as_ref()?;
    let orch = r2.orchestrator_evidence.as_ref()?;

    let anchor_blocks = [r0.anchor.number, r1.anchor.number, r2.anchor.number];
    let anchor_hashes = [r0.anchor.hash, r1.anchor.hash, r2.anchor.hash];

    let leg_quotes: Vec<LegQuote> = r2
        .route_plan
        .legs
        .iter()
        .map(|leg| LegQuote {
            pool: leg.pool,
            token_in: leg.token_in,
            token_out: leg.token_out,
            amount_in: U256::zero(),
            amount_out: U256::zero(),
        })
        .collect();

    let evidence_hash = H256::from(keccak256(format!(
        "{}:{:?}:{:?}:{}:{}:{}",
        r2.structural_cycle_key, anchor_blocks, anchor_hashes, economics.net_pnl_atomic,
        orch.readonly_pass, orch.trace_validated,
    )));

    let opportunity_id = H256::from(keccak256(format!(
        "{}:{:?}:{}",
        r2.structural_cycle_key, r2.amount_in, evidence_hash,
    )));

    Some(ExecutableOpportunity {
        opportunity_id,
        structural_cycle_key: r2.structural_cycle_key.clone(),
        route_plan: r2.route_plan.clone(),
        anchor_block: r2.anchor.number,
        anchor_block_hash: r2.anchor.hash,
        context_hash: r2.context_hash,
        evidence_hash,
        amount_in: r2.amount_in,
        expected_amount_out: economics.final_amount_atomic,
        gross_pnl: economics.gross_pnl_atomic,
        gas_estimate: economics.gas_cost_atomic,
        net_pnl: economics.net_pnl_atomic,
        net_pnl_usd: None,
        leg_quotes,
        evidence: ExecutionEvidence {
            eth_call_pass: orch.readonly_pass,
            preflight_pass: orch.preflight_pass,
            trace_validated: orch.trace_validated,
            balance_delta: orch.balance_delta,
        },
        stability: StabilityRecord { anchor_blocks, anchor_hashes },
        execution_profile: r2.execution_profile.clone(),
    })
}
```

Add `pub mod executable_opportunity;` to `src/core/mod.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib executable_opportunity -- --nocapture`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src/core/executable_opportunity.rs src/core/mod.rs
git commit -m "feat(2d-c2c): add ExecutableOpportunity built from stable rounds"
```

---

## Task 5: Canonical risk approval

**Files:**
- Modify: `src/core/risk.rs`
- Test: same file, `#[cfg(test)] mod tests` (existing module — add to it)

**Interfaces:**
- Consumes: `executable_opportunity::ExecutableOpportunity`.
- Produces:
```rust
pub struct RiskApproval { pub min_profit_raw: U256, pub max_gas_raw: U256, pub max_slippage_bps: u32 }
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalRiskRejection {
    StaleAnchor, InvalidContextHash, UnstableRoute, NonPositiveNetPnl,
    GasExceedsLimit, SlippageExceedsLimit, EthCallFailed, PreflightFailed,
    TraceFailed, IncompleteQuotes, RouteDiscontinuity, RejectedRegistryHit,
}
pub struct CanonicalRiskConfig {
    pub absolute_min_profit_floor_raw: U256,
    pub retention_bps: u32,
    pub max_gas_raw: U256,
    pub max_slippage_bps: u32,
    pub max_anchor_age_blocks: u64,
}
impl RiskManager {
    pub fn assess_executable_opportunity(
        &self,
        opportunity: &ExecutableOpportunity,
        cfg: &CanonicalRiskConfig,
        current_head_block: u64,
    ) -> Result<RiskApproval, Vec<CanonicalRiskRejection>>;
}
```
Task 6 (strategy selector) and Task 9 (canonical executor) consume `RiskApproval` and `CanonicalRiskRejection` exactly as named here.

- [ ] **Step 1: Write the failing tests**

Add to `src/core/risk.rs`'s existing `#[cfg(test)] mod tests` block (find it at the bottom of the file):

```rust
mod canonical_tests {
    use super::super::*;
    use crate::core::executable_opportunity::{ExecutionEvidence, StabilityRecord};
    use crate::core::execution_profile::ExecutionProfile;
    use crate::core::executable_route_materializer::{ExecutableRoutePlan, ForkSetupPlan};
    use crate::core::fresh_economics::PinnedStateSnapshot;
    use ethers::types::{Address, H256, U256};

    fn cfg() -> CanonicalRiskConfig {
        CanonicalRiskConfig {
            absolute_min_profit_floor_raw: U256::from(10u64),
            retention_bps: 2_000, // keep 20% margin over expected profit as floor
            max_gas_raw: U256::from(1_000_000u64),
            max_slippage_bps: 100,
            max_anchor_age_blocks: 32,
        }
    }

    fn opportunity(net_pnl: i128, anchor_block: u64) -> ExecutableOpportunity {
        ExecutableOpportunity {
            opportunity_id: H256::zero(),
            structural_cycle_key: "k".into(),
            route_plan: ExecutableRoutePlan {
                structural_cycle_key: "k".into(), anchor_block, start_token: Address::zero(),
                legs: vec![], snapshot: PinnedStateSnapshot::default(),
                fork_setup: ForkSetupPlan { anchor_block, caller: Address::zero(), tokens: vec![], funding: vec![], approvals: vec![], targets: vec![], balance_checks: vec![] },
            },
            anchor_block, anchor_block_hash: H256::zero(), context_hash: H256::repeat_byte(1),
            evidence_hash: H256::repeat_byte(2), amount_in: U256::from(1_000u64),
            expected_amount_out: U256::from(1_100u64), gross_pnl: net_pnl + 10,
            gas_estimate: U256::from(10u64), net_pnl, net_pnl_usd: None, leg_quotes: vec![],
            evidence: ExecutionEvidence { eth_call_pass: true, preflight_pass: true, trace_validated: true, balance_delta: U256::from(100u64) },
            stability: StabilityRecord { anchor_blocks: [anchor_block - 6, anchor_block - 3, anchor_block], anchor_hashes: [H256::zero(); 3] },
            execution_profile: ExecutionProfile { chain_id: 137, profile_label: "base".into() },
        }
    }

    #[test]
    fn approves_positive_pnl_within_limits() {
        let rm = RiskManager::with_defaults();
        let approval = rm.assess_executable_opportunity(&opportunity(200, 1000), &cfg(), 1002).unwrap();
        assert_eq!(approval.max_gas_raw, U256::from(1_000_000u64));
    }

    #[test]
    fn min_profit_raw_is_max_of_floor_and_retention() {
        let rm = RiskManager::with_defaults();
        // net_pnl=200, retention_bps=2000 (20%) -> 40, floor=10 -> min_profit_raw=40
        let approval = rm.assess_executable_opportunity(&opportunity(200, 1000), &cfg(), 1002).unwrap();
        assert_eq!(approval.min_profit_raw, U256::from(40u64));
    }

    #[test]
    fn rejects_non_positive_net_pnl() {
        let rm = RiskManager::with_defaults();
        let err = rm.assess_executable_opportunity(&opportunity(0, 1000), &cfg(), 1002).unwrap_err();
        assert!(err.contains(&CanonicalRiskRejection::NonPositiveNetPnl));
    }

    #[test]
    fn rejects_stale_anchor() {
        let rm = RiskManager::with_defaults();
        let err = rm.assess_executable_opportunity(&opportunity(200, 1000), &cfg(), 1000 + 33).unwrap_err();
        assert!(err.contains(&CanonicalRiskRejection::StaleAnchor));
    }

    #[test]
    fn rejects_when_trace_not_validated() {
        let rm = RiskManager::with_defaults();
        let mut opp = opportunity(200, 1000);
        opp.evidence.trace_validated = false;
        let err = rm.assess_executable_opportunity(&opp, &cfg(), 1002).unwrap_err();
        assert!(err.contains(&CanonicalRiskRejection::TraceFailed));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib risk::canonical_tests -- --nocapture`
Expected: FAIL — `assess_executable_opportunity`/`CanonicalRiskConfig`/`RiskApproval`/`CanonicalRiskRejection` not found.

- [ ] **Step 3: Implement**

Add near the top of `src/core/risk.rs` (after existing imports) and inside `impl RiskManager`:

```rust
use crate::core::executable_opportunity::ExecutableOpportunity;
use ethers::types::U256;

#[derive(Debug, Clone, Copy)]
pub struct RiskApproval {
    pub min_profit_raw: U256,
    pub max_gas_raw: U256,
    pub max_slippage_bps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalRiskRejection {
    StaleAnchor,
    InvalidContextHash,
    UnstableRoute,
    NonPositiveNetPnl,
    GasExceedsLimit,
    SlippageExceedsLimit,
    EthCallFailed,
    PreflightFailed,
    TraceFailed,
    IncompleteQuotes,
    RouteDiscontinuity,
    RejectedRegistryHit,
}

#[derive(Debug, Clone)]
pub struct CanonicalRiskConfig {
    pub absolute_min_profit_floor_raw: U256,
    pub retention_bps: u32,
    pub max_gas_raw: U256,
    pub max_slippage_bps: u32,
    pub max_anchor_age_blocks: u64,
}
```

Inside `impl RiskManager { ... }`:

```rust
pub fn assess_executable_opportunity(
    &self,
    opportunity: &ExecutableOpportunity,
    cfg: &CanonicalRiskConfig,
    current_head_block: u64,
) -> Result<RiskApproval, Vec<CanonicalRiskRejection>> {
    let mut rejections = Vec::new();

    let anchor_age = current_head_block.saturating_sub(opportunity.anchor_block);
    if anchor_age > cfg.max_anchor_age_blocks {
        rejections.push(CanonicalRiskRejection::StaleAnchor);
    }
    if opportunity.context_hash.is_zero() || opportunity.evidence_hash.is_zero() {
        rejections.push(CanonicalRiskRejection::InvalidContextHash);
    }
    let [a0, a1, a2] = opportunity.stability.anchor_blocks;
    if !(a0 < a1 && a1 < a2) {
        rejections.push(CanonicalRiskRejection::UnstableRoute);
    }
    if opportunity.net_pnl <= 0 {
        rejections.push(CanonicalRiskRejection::NonPositiveNetPnl);
    }
    if opportunity.gas_estimate > cfg.max_gas_raw {
        rejections.push(CanonicalRiskRejection::GasExceedsLimit);
    }
    if !opportunity.evidence.eth_call_pass {
        rejections.push(CanonicalRiskRejection::EthCallFailed);
    }
    if !opportunity.evidence.preflight_pass {
        rejections.push(CanonicalRiskRejection::PreflightFailed);
    }
    if !opportunity.evidence.trace_validated {
        rejections.push(CanonicalRiskRejection::TraceFailed);
    }
    if opportunity.route_plan.legs.is_empty() {
        rejections.push(CanonicalRiskRejection::RouteDiscontinuity);
    }

    if !rejections.is_empty() {
        return Err(rejections);
    }

    let net_pnl_raw = U256::from(opportunity.net_pnl.max(0) as u128);
    let retention_floor = net_pnl_raw.saturating_mul(U256::from(cfg.retention_bps)) / U256::from(10_000u64);
    let min_profit_raw = cfg.absolute_min_profit_floor_raw.max(retention_floor);

    Ok(RiskApproval {
        min_profit_raw,
        max_gas_raw: cfg.max_gas_raw,
        max_slippage_bps: cfg.max_slippage_bps,
    })
}
```

`SlippageExceedsLimit`, `IncompleteQuotes`, `RejectedRegistryHit` are constructed but not yet triggered by any check in this task — `leg_quotes` completeness and rejected-registry propagation onto `ExecutableOpportunity` land in Task 11 (currently `RoundEvidence.rejected_registry_hit` isn't threaded into `ExecutableOpportunity` — add that wiring in Task 11 and revisit this function then to add the two checks; leaving the variants defined now avoids a breaking enum change later).

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib risk:: -- --nocapture`
Expected: PASS, including all pre-existing `risk.rs` tests (no regressions).

- [ ] **Step 5: Commit**

```bash
git add src/core/risk.rs
git commit -m "feat(2d-c2c): add integer-only canonical risk approval"
```

---

## Task 6: `determine_execution_strategy_canonical`

**Files:**
- Modify: `src/core/flashloan.rs`
- Test: same file's existing `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `ExecutableOpportunity`, `risk::RiskApproval`, `Config`.
- Produces:
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalStrategyDecision {
    Direct, Flashloan, WrapperFlashloan, Skip(CanonicalSkipReason),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalSkipReason {
    UnsupportedVenue, FlashloanDisabled, WrapperDisabled, InsufficientRiskApproval,
}
pub fn determine_execution_strategy_canonical(
    opportunity: &ExecutableOpportunity,
    approval: &RiskApproval,
    cfg: &Config,
) -> CanonicalStrategyDecision;
```
Task 9 (canonical executor) matches on this exact enum to dispatch to `execute_direct_canonical`/`execute_flashloan_canonical`/`execute_wrapper_canonical`/short-circuit skip.

This is a free function (not a method on `ArbitrageClient`), placed near the existing `determine_execution_strategy` method in `flashloan.rs` — it needs no RPC/executor state, only `cfg` and the opportunity/approval, so it doesn't need `&self`.

Venue support check: every leg's `venue` must be one this contract's `AbiSwapStep.dex_type` encoding supports. Confirmed by reading `map_dex_type` (flashloan.rs, existing): only `QuickSwap`/`SushiSwap`/`UniswapV3` map to a `dex_type`; `Curve` has no encoding. `Venue` is `executable_call::Venue { UniswapV3, QuickSwap, SushiSwap, Curve }`.

- [ ] **Step 1: Write the failing tests**

Add near the bottom of `src/core/flashloan.rs`'s test module:

```rust
mod canonical_strategy_tests {
    use super::super::*;
    use crate::core::executable_call::Venue;
    use crate::core::executable_opportunity::{ExecutionEvidence, StabilityRecord};
    use crate::core::executable_route_materializer::{ExecutableLegPlan, ExecutableRoutePlan, ForkSetupPlan};
    use crate::core::execution_profile::ExecutionProfile;
    use crate::core::fresh_economics::PinnedStateSnapshot;
    use crate::core::risk::RiskApproval;
    use ethers::types::{Address, H256, U256};

    fn leg(venue: Venue) -> ExecutableLegPlan {
        ExecutableLegPlan {
            venue, token_in: Address::repeat_byte(1), token_out: Address::repeat_byte(2),
            pool: Address::repeat_byte(3), router: Address::repeat_byte(4), fee: Some(3000),
            curve_method: None, token_in_index: None, token_out_index: None, spender: Address::repeat_byte(4),
        }
    }

    fn opportunity(legs: Vec<ExecutableLegPlan>) -> ExecutableOpportunity {
        ExecutableOpportunity {
            opportunity_id: H256::zero(), structural_cycle_key: "k".into(),
            route_plan: ExecutableRoutePlan {
                structural_cycle_key: "k".into(), anchor_block: 100, start_token: Address::repeat_byte(1),
                legs, snapshot: PinnedStateSnapshot::default(),
                fork_setup: ForkSetupPlan { anchor_block: 100, caller: Address::zero(), tokens: vec![], funding: vec![], approvals: vec![], targets: vec![], balance_checks: vec![] },
            },
            anchor_block: 100, anchor_block_hash: H256::zero(), context_hash: H256::repeat_byte(1),
            evidence_hash: H256::repeat_byte(2), amount_in: U256::from(1_000u64),
            expected_amount_out: U256::from(1_100u64), gross_pnl: 110, gas_estimate: U256::from(10u64),
            net_pnl: 100, net_pnl_usd: None, leg_quotes: vec![],
            evidence: ExecutionEvidence { eth_call_pass: true, preflight_pass: true, trace_validated: true, balance_delta: U256::from(100u64) },
            stability: StabilityRecord { anchor_blocks: [94, 97, 100], anchor_hashes: [H256::zero(); 3] },
            execution_profile: ExecutionProfile { chain_id: 137, profile_label: "base".into() },
        }
    }

    fn approval() -> RiskApproval {
        RiskApproval { min_profit_raw: U256::from(10u64), max_gas_raw: U256::from(1_000_000u64), max_slippage_bps: 100 }
    }

    #[test]
    fn curve_leg_is_skipped_unsupported_venue() {
        let opp = opportunity(vec![leg(Venue::Curve)]);
        let mut cfg = Config::default();
        cfg.flashloan.enabled = true;
        let decision = determine_execution_strategy_canonical(&opp, &approval(), &cfg);
        assert_eq!(decision, CanonicalStrategyDecision::Skip(CanonicalSkipReason::UnsupportedVenue));
    }

    #[test]
    fn flashloan_disabled_falls_back_to_direct() {
        let opp = opportunity(vec![leg(Venue::QuickSwap)]);
        let mut cfg = Config::default();
        cfg.flashloan.enabled = false;
        let decision = determine_execution_strategy_canonical(&opp, &approval(), &cfg);
        assert_eq!(decision, CanonicalStrategyDecision::Direct);
    }

    #[test]
    fn flashloan_and_wrapper_enabled_selects_wrapper() {
        let opp = opportunity(vec![leg(Venue::QuickSwap)]);
        let mut cfg = Config::default();
        cfg.flashloan.enabled = true;
        cfg.execution.use_flashloan = true;
        cfg.wrapper.enabled = true;
        let decision = determine_execution_strategy_canonical(&opp, &approval(), &cfg);
        assert_eq!(decision, CanonicalStrategyDecision::WrapperFlashloan);
    }

    #[test]
    fn flashloan_enabled_without_wrapper_selects_flashloan() {
        let opp = opportunity(vec![leg(Venue::QuickSwap)]);
        let mut cfg = Config::default();
        cfg.flashloan.enabled = true;
        cfg.execution.use_flashloan = true;
        cfg.wrapper.enabled = false;
        let decision = determine_execution_strategy_canonical(&opp, &approval(), &cfg);
        assert_eq!(decision, CanonicalStrategyDecision::Flashloan);
    }
}
```

Check `Config`/`FlashloanConfig`/`ExecutionConfig`/`WrapperConfig` all derive `Default` before relying on `Config::default()` here — grep confirms `#[derive(... Default ...)]` is already present on `Config` and its sub-structs (same pattern the existing `flashloan.rs` test module already uses at line ~3327 `cfg.execution.dry_run = false`). If any sub-struct used here lacks `Default`, add it as part of this step (pure derive addition, no behavior change).

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib flashloan::canonical_strategy_tests -- --nocapture`
Expected: FAIL — symbols not found.

- [ ] **Step 3: Implement**

Add near `determine_execution_strategy` (the existing method, ~line 1401):

```rust
use crate::core::executable_call::Venue;
use crate::core::executable_opportunity::ExecutableOpportunity;
use crate::core::risk::RiskApproval;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalSkipReason {
    UnsupportedVenue,
    FlashloanDisabled,
    WrapperDisabled,
    InsufficientRiskApproval,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalStrategyDecision {
    Direct,
    Flashloan,
    WrapperFlashloan,
    Skip(CanonicalSkipReason),
}

fn venue_supported_by_executor(venue: Venue) -> bool {
    matches!(venue, Venue::QuickSwap | Venue::SushiSwap | Venue::UniswapV3)
}

pub fn determine_execution_strategy_canonical(
    opportunity: &ExecutableOpportunity,
    approval: &RiskApproval,
    cfg: &Config,
) -> CanonicalStrategyDecision {
    if opportunity
        .route_plan
        .legs
        .iter()
        .any(|leg| !venue_supported_by_executor(leg.venue))
    {
        return CanonicalStrategyDecision::Skip(CanonicalSkipReason::UnsupportedVenue);
    }
    if opportunity.net_pnl <= 0 || approval.min_profit_raw.is_zero() && opportunity.net_pnl == 0 {
        return CanonicalStrategyDecision::Skip(CanonicalSkipReason::InsufficientRiskApproval);
    }
    if !cfg.flashloan.enabled || !cfg.execution.use_flashloan {
        return CanonicalStrategyDecision::Direct;
    }
    if cfg.wrapper.enabled {
        CanonicalStrategyDecision::WrapperFlashloan
    } else {
        CanonicalStrategyDecision::Flashloan
    }
}
```

(`FlashloanDisabled`/`WrapperDisabled` variants are reserved for a future explicit per-opportunity capability check — e.g. wrapper contract not deployed for this chain — not yet exercised; keep them in the enum so callers that already match exhaustively don't need a second breaking change.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib flashloan:: -- --nocapture`
Expected: PASS, including pre-existing `flashloan.rs` tests.

- [ ] **Step 5: Commit**

```bash
git add src/core/flashloan.rs
git commit -m "feat(2d-c2c): add canonical strategy selector"
```

---

## Task 7: `ExecutionMode` + `steps_from_route_plan`

**Files:**
- Modify: `src/core/flashloan.rs`
- Test: same file

**Interfaces:**
- Produces:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode { LegacyConfigured, CanonicalShadow }
impl ExecutionMode {
    pub fn is_dry_run(self) -> bool; // CanonicalShadow -> always true; LegacyConfigured -> caller still reads cfg.execution.dry_run separately
}
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CanonicalExecutorError {
    #[error("UNSUPPORTED_VENUE: {0:?}")] UnsupportedVenue(Venue),
    #[error("EMPTY_ROUTE_PLAN")] EmptyRoutePlan,
}
impl ArbitrageClient {
    fn steps_from_route_plan(&self, plan: &ExecutableRoutePlan) -> Result<(Address, U256, Vec<AbiSwapStep>), CanonicalExecutorError>;
}
```
Task 9 consumes `steps_from_route_plan`'s output tuple exactly like the legacy `extract_and_convert_opp_data` output is consumed today (same `(Address, U256, Vec<AbiSwapStep>)` shape), and matches on `CanonicalExecutorError`.

This is the address-typed front-end that replaces the symbol round-trip: `token_in`/`token_out`/`pool`/`router` come straight from `ExecutableLegPlan` (already `Address`), `dex_type` from `Venue` via a new `dex_type_from_venue` (mirrors existing `map_dex_type` but takes the typed enum, not a string), fee-tier `extra_data` from `leg.fee: Option<u32>` directly via the existing `encode_v3_fee_extra_data` (reused, no change) instead of `build_extra_data_for_step`'s string-based `resolve_v3_fee_for_step`. `amount_out_min` is intentionally `U256::zero()` in this task — Task 9 fills it in from `RiskApproval`/leg quotes once wired.

- [ ] **Step 1: Write the failing tests**

```rust
mod canonical_executor_tests {
    use super::super::*;
    use crate::core::executable_call::Venue;
    use crate::core::executable_route_materializer::{ExecutableLegPlan, ExecutableRoutePlan, ForkSetupPlan};
    use crate::core::fresh_economics::PinnedStateSnapshot;
    use ethers::types::{Address, U256};

    fn plan(legs: Vec<ExecutableLegPlan>) -> ExecutableRoutePlan {
        ExecutableRoutePlan {
            structural_cycle_key: "k".into(), anchor_block: 100,
            start_token: legs.first().map(|l| l.token_in).unwrap_or_default(),
            legs, snapshot: PinnedStateSnapshot::default(),
            fork_setup: ForkSetupPlan { anchor_block: 100, caller: Address::zero(), tokens: vec![], funding: vec![], approvals: vec![], targets: vec![], balance_checks: vec![] },
        }
    }

    fn v3_leg() -> ExecutableLegPlan {
        ExecutableLegPlan {
            venue: Venue::UniswapV3, token_in: Address::repeat_byte(1), token_out: Address::repeat_byte(2),
            pool: Address::repeat_byte(3), router: Address::repeat_byte(4), fee: Some(3000),
            curve_method: None, token_in_index: None, token_out_index: None, spender: Address::repeat_byte(4),
        }
    }

    fn curve_leg() -> ExecutableLegPlan {
        ExecutableLegPlan {
            venue: Venue::Curve, token_in: Address::repeat_byte(1), token_out: Address::repeat_byte(2),
            pool: Address::repeat_byte(3), router: Address::repeat_byte(4), fee: None,
            curve_method: Some(crate::core::executable_route_materializer::CurveMethod::Exchange),
            token_in_index: Some(0), token_out_index: Some(1), spender: Address::repeat_byte(4),
        }
    }

    fn client() -> ArbitrageClient {
        // existing test helper already used elsewhere in this file's test module —
        // reuse whatever constructor the pre-existing `flashloan.rs` tests use
        // (grep this file's `mod tests` for `fn test_client(` before writing this
        // line; do not hand-roll a second one).
        super::super::tests::test_client()
    }

    #[test]
    fn builds_abi_steps_directly_from_addresses_no_symbol_lookup() {
        let plan = plan(vec![v3_leg()]);
        let (token_in, _amount, steps) = client().steps_from_route_plan(&plan).unwrap();
        assert_eq!(token_in, Address::repeat_byte(1));
        assert_eq!(steps[0].token_in, Address::repeat_byte(1));
        assert_eq!(steps[0].token_out, Address::repeat_byte(2));
    }

    #[test]
    fn curve_leg_is_rejected_as_unsupported_venue() {
        let plan = plan(vec![curve_leg()]);
        let err = client().steps_from_route_plan(&plan).unwrap_err();
        assert_eq!(err, CanonicalExecutorError::UnsupportedVenue(Venue::Curve));
    }

    #[test]
    fn empty_route_plan_is_rejected() {
        let plan = plan(vec![]);
        let err = client().steps_from_route_plan(&plan).unwrap_err();
        assert_eq!(err, CanonicalExecutorError::EmptyRoutePlan);
    }

    #[test]
    fn canonical_shadow_mode_is_always_dry_run() {
        assert!(ExecutionMode::CanonicalShadow.is_dry_run());
    }
}
```

If `test_client()` (or equivalent) does not already exist in `flashloan.rs`'s test module, inspect the existing tests around line 2652 (`use ethers::signers::LocalWallet;`) to find whatever helper they use to build an `ArbitrageClient` for tests, and reuse that exact helper — do not create a parallel construction path.

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib flashloan::canonical_executor_tests -- --nocapture`
Expected: FAIL — symbols not found.

- [ ] **Step 3: Implement**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    LegacyConfigured,
    CanonicalShadow,
}

impl ExecutionMode {
    pub fn is_dry_run(self) -> bool {
        matches!(self, ExecutionMode::CanonicalShadow)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CanonicalExecutorError {
    #[error("UNSUPPORTED_VENUE: {0:?}")]
    UnsupportedVenue(crate::core::executable_call::Venue),
    #[error("EMPTY_ROUTE_PLAN")]
    EmptyRoutePlan,
}

fn dex_type_from_venue(venue: crate::core::executable_call::Venue) -> Result<u8, CanonicalExecutorError> {
    use crate::core::executable_call::Venue;
    match venue {
        Venue::QuickSwap => Ok(0),
        Venue::SushiSwap => Ok(1),
        Venue::UniswapV3 => Ok(2),
        Venue::Curve => Err(CanonicalExecutorError::UnsupportedVenue(venue)),
    }
}

impl ArbitrageClient {
    fn steps_from_route_plan(
        &self,
        plan: &crate::core::executable_route_materializer::ExecutableRoutePlan,
    ) -> Result<(Address, U256, Vec<AbiSwapStep>), CanonicalExecutorError> {
        if plan.legs.is_empty() {
            return Err(CanonicalExecutorError::EmptyRoutePlan);
        }
        let mut steps = Vec::with_capacity(plan.legs.len());
        for leg in &plan.legs {
            let dex_type = dex_type_from_venue(leg.venue)?;
            let extra_data = if leg.venue == crate::core::executable_call::Venue::UniswapV3 {
                Self::encode_v3_fee_extra_data(leg.fee.unwrap_or(3000))
            } else {
                Bytes::new()
            };
            steps.push(AbiSwapStep {
                dex_type,
                token_in: leg.token_in,
                token_out: leg.token_out,
                amount_out_min: U256::zero(),
                extra_data,
            });
        }
        Ok((plan.start_token, U256::zero(), steps))
    }
}
```

Check the existing `encode_v3_fee_extra_data` visibility (`fn`/`pub(crate) fn`) — it's called from `build_extra_data_for_step` in the same `impl` block, so it's already reachable from another method on the same type; no visibility change needed.

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib flashloan:: -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/core/flashloan.rs
git commit -m "feat(2d-c2c): add address-typed canonical step builder, no symbol round-trip"
```

---

## Task 8: Canonical dry-result core + hard-gate tests

**Files:**
- Modify: `src/core/flashloan.rs`
- Test: same file

**Interfaces:**
- Produces:
```rust
impl ArbitrageClient {
    fn canonical_dry_result(&self, opportunity: &ExecutableOpportunity) -> BundleResult; // pure, no I/O
}
```
This is the "núcleo compartilhado" the three `execute_*_canonical` methods (Task 9) call at the end of their dry-run path. It is a pure function of `ExecutableOpportunity` — no RPC, no `send_and_confirm_transaction` call exists anywhere in its body or callers, which is what makes `canonical_shadow_cannot_reach_send_and_confirm` true by construction rather than by a runtime `if`.

- [ ] **Step 1: Write the failing tests**

```rust
mod hard_gate_tests {
    use super::super::*;

    #[test]
    fn canonical_dry_result_is_always_marked_success_without_tx_hash() {
        let client = super::super::tests::test_client();
        let opp = super::canonical_executor_tests_fixture_opportunity();
        let result = client.canonical_dry_result(&opp);
        assert!(result.tx_hash.is_none());
        assert_eq!(result.execution_mode.as_deref(), Some("canonical_shadow_dry_run"));
    }

    #[test]
    fn canonical_shadow_mode_cannot_be_overridden() {
        // ExecutionMode has exactly two variants and CanonicalShadow::is_dry_run()
        // is a compile-time-fixed `true` — there is no field, env read, or
        // constructor argument that can flip it. This test exists so a future
        // refactor that adds one (e.g. `CanonicalShadow(bool)`) fails loudly.
        assert!(ExecutionMode::CanonicalShadow.is_dry_run());
        assert!(!matches!(ExecutionMode::LegacyConfigured, ExecutionMode::CanonicalShadow));
    }
}
```

Add a small shared fixture function (referenced above) near the top of the canonical test modules so both `canonical_executor_tests` (Task 7) and `hard_gate_tests` can use it — extract the `opportunity(...)` builder from Task 6's test module into a `pub(super) fn canonical_executor_tests_fixture_opportunity() -> ExecutableOpportunity` in a shared `mod canonical_test_fixtures` and have Task 6/7/8's test modules call it instead of redefining their own. (This is a small consolidation; do it in this step rather than leaving three near-duplicate builders.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib flashloan::hard_gate_tests -- --nocapture`
Expected: FAIL — `canonical_dry_result` not found.

- [ ] **Step 3: Implement**

```rust
impl ArbitrageClient {
    fn canonical_dry_result(&self, opportunity: &ExecutableOpportunity) -> BundleResult {
        let profit_usd = opportunity.net_pnl_usd.unwrap_or(0.0);
        let gas_usd = 0.0; // report-only; canonical shadow has no live gas-price oracle wired to this call in 2D-C2C
        let mut result = BundleResult::new(true, profit_usd, gas_usd);
        result.execution_mode = Some("canonical_shadow_dry_run".into());
        result
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib flashloan:: -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/core/flashloan.rs
git commit -m "feat(2d-c2c): add pure canonical dry-result core, no path to send_and_confirm"
```

---

## Task 9: `execute_direct_canonical` / `execute_flashloan_canonical` / `execute_wrapper_canonical`

**Files:**
- Modify: `src/core/flashloan.rs`
- Test: same file

**Interfaces:**
- Consumes: `ExecutableOpportunity`, `risk::RiskApproval`, `ExecutionMode` (always called with `ExecutionMode::CanonicalShadow` in this phase).
- Produces:
```rust
impl ArbitrageClient {
    pub async fn execute_direct_canonical(&self, opportunity: &ExecutableOpportunity, approval: &RiskApproval) -> Result<BundleResult>;
    pub async fn execute_flashloan_canonical(&self, opportunity: &ExecutableOpportunity, approval: &RiskApproval) -> Result<BundleResult>;
    pub async fn execute_wrapper_canonical(&self, opportunity: &ExecutableOpportunity, approval: &RiskApproval) -> Result<BundleResult>;
}
```
Task 14 (`c2b_shadow_service.rs`) calls whichever of these three matches `CanonicalStrategyDecision` from Task 6.

Each method: build `(asset, amount, steps)` via `steps_from_route_plan` (Task 7), set `amount = opportunity.amount_in` and each step's `amount_out_min` from `approval.min_profit_raw`-derived slippage (use `approval.max_slippage_bps` against `opportunity.expected_amount_out`, same shape as legacy's slippage application — do not invent a new formula, mirror `apply_slippage_safe`'s intent: `amount_out_min = expected_amount_out * (10_000 - max_slippage_bps) / 10_000`, applied to the route's final expected output only, matching how `A6` in the legacy comment describes slippage being applied once per route, not re-deducted per hop), run the **existing** `apply_complexity_filters`/`validate_wrapper_steps` (reused unmodified), build the **existing** contract call (`self.executor.execute_direct(...)`, `self.executor.execute_flashloan(...)`, or `FlashloanCaller::trigger_flashloan(...)` via the existing `encode_callback_data`/`resolve_profit_recipient`/`assert_profit_recipient_matches_owner`), simulate via the existing `simulate_transaction`/`simulate_bool_transaction`, then **always** return `Ok(self.canonical_dry_result(opportunity))` — never call `send_and_confirm_transaction`.

- [ ] **Step 1: Write the failing tests**

```rust
mod canonical_execute_tests {
    use super::super::*;

    #[tokio::test]
    async fn direct_reaches_real_execute_core_in_shadow_mode() {
        let client = super::super::tests::test_client();
        let opp = super::canonical_test_fixtures::opportunity_with_quickswap_leg();
        let approval = super::canonical_test_fixtures::approval();
        let result = client.execute_direct_canonical(&opp, &approval).await.unwrap();
        assert_eq!(result.execution_mode.as_deref(), Some("canonical_shadow_dry_run"));
        assert!(result.tx_hash.is_none());
    }

    #[tokio::test]
    async fn flashloan_reaches_real_execute_core_in_shadow_mode() {
        let client = super::super::tests::test_client();
        let opp = super::canonical_test_fixtures::opportunity_with_quickswap_leg();
        let approval = super::canonical_test_fixtures::approval();
        let result = client.execute_flashloan_canonical(&opp, &approval).await.unwrap();
        assert_eq!(result.execution_mode.as_deref(), Some("canonical_shadow_dry_run"));
        assert!(result.tx_hash.is_none());
    }

    #[tokio::test]
    async fn wrapper_reaches_real_execute_core_in_shadow_mode() {
        let client = super::super::tests::test_client();
        let opp = super::canonical_test_fixtures::opportunity_with_quickswap_leg();
        let approval = super::canonical_test_fixtures::approval();
        let result = client.execute_wrapper_canonical(&opp, &approval).await.unwrap();
        assert_eq!(result.execution_mode.as_deref(), Some("canonical_shadow_dry_run"));
        assert!(result.tx_hash.is_none());
    }

    #[tokio::test]
    async fn curve_route_never_reaches_executor() {
        let client = super::super::tests::test_client();
        let opp = super::canonical_test_fixtures::opportunity_with_curve_leg();
        let approval = super::canonical_test_fixtures::approval();
        let err = client.execute_direct_canonical(&opp, &approval).await;
        assert!(err.is_err(), "Curve leg must fail steps_from_route_plan before any contract call is built");
    }

    #[tokio::test]
    async fn canonical_shadow_cannot_reach_send_and_confirm() {
        // send_and_confirm_transaction requires a live wallet/nonce and would
        // error/panic against the test client's stub middleware if reached.
        // All three canonical methods returning Ok(..) with tx_hash: None on
        // this test client is the behavioral proof that path was never taken.
        let client = super::super::tests::test_client();
        let opp = super::canonical_test_fixtures::opportunity_with_quickswap_leg();
        let approval = super::canonical_test_fixtures::approval();
        for result in [
            client.execute_direct_canonical(&opp, &approval).await.unwrap(),
            client.execute_flashloan_canonical(&opp, &approval).await.unwrap(),
            client.execute_wrapper_canonical(&opp, &approval).await.unwrap(),
        ] {
            assert!(result.tx_hash.is_none());
        }
    }
}
```

Add `opportunity_with_quickswap_leg()`, `opportunity_with_curve_leg()`, and `approval()` to the `canonical_test_fixtures` module started in Task 8.

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib flashloan::canonical_execute_tests -- --nocapture`
Expected: FAIL — methods not found.

- [ ] **Step 3: Implement**

```rust
impl ArbitrageClient {
    fn min_out_from_approval(expected_out: U256, max_slippage_bps: u32) -> U256 {
        expected_out.saturating_mul(U256::from(10_000u32.saturating_sub(max_slippage_bps))) / U256::from(10_000u32)
    }

    pub async fn execute_direct_canonical(
        &self,
        opportunity: &ExecutableOpportunity,
        approval: &RiskApproval,
    ) -> Result<BundleResult> {
        let (asset, _zero_amount, mut steps) = self.steps_from_route_plan(&opportunity.route_plan)?;
        let amount = opportunity.amount_in;
        if let Some(last) = steps.last_mut() {
            last.amount_out_min = Self::min_out_from_approval(opportunity.expected_amount_out, approval.max_slippage_bps);
        }
        let cfg = self.config.lock().await;
        self.apply_complexity_filters(&steps, &cfg)?;
        drop(cfg);

        let direct = self.executor.execute_direct(asset, amount, steps, approval.min_profit_raw);
        let _ = self.simulate_transaction(&direct).await; // evidence-only in shadow; failures don't block the report

        Ok(self.canonical_dry_result(opportunity))
    }

    pub async fn execute_flashloan_canonical(
        &self,
        opportunity: &ExecutableOpportunity,
        approval: &RiskApproval,
    ) -> Result<BundleResult> {
        let (asset, _zero_amount, mut steps) = self.steps_from_route_plan(&opportunity.route_plan)?;
        let amount = opportunity.amount_in;
        if let Some(last) = steps.last_mut() {
            last.amount_out_min = Self::min_out_from_approval(opportunity.expected_amount_out, approval.max_slippage_bps);
        }
        let cfg = self.config.lock().await;
        self.apply_complexity_filters(&steps, &cfg)?;
        drop(cfg);

        let call = self.executor.execute_flashloan(asset, amount, steps, approval.min_profit_raw);
        let _ = self.simulate_bool_transaction(&call).await;

        Ok(self.canonical_dry_result(opportunity))
    }

    pub async fn execute_wrapper_canonical(
        &self,
        opportunity: &ExecutableOpportunity,
        approval: &RiskApproval,
    ) -> Result<BundleResult> {
        let (asset, _zero_amount, mut steps) = self.steps_from_route_plan(&opportunity.route_plan)?;
        let amount = opportunity.amount_in;
        if let Some(last) = steps.last_mut() {
            last.amount_out_min = Self::min_out_from_approval(opportunity.expected_amount_out, approval.max_slippage_bps);
        }
        self.validate_wrapper_steps(&steps, asset)?;
        let cfg = self.config.lock().await;
        self.apply_complexity_filters(&steps, &cfg)?;
        let wrapper_addr = Address::from_str(&cfg.wrapper.address)?;
        let profit_recipient = self.resolve_profit_recipient(&cfg)?;
        drop(cfg);

        self.assert_profit_recipient_matches_owner(profit_recipient).await?;
        let params = Self::encode_callback_data(profit_recipient, &steps, approval.min_profit_raw);
        let contract = FlashloanCaller::new(wrapper_addr, self.middleware.clone());
        let call = contract.trigger_flashloan(asset, amount, params);
        let _ = self.simulate_transaction(&call).await;

        Ok(self.canonical_dry_result(opportunity))
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib flashloan:: -- --nocapture`
Expected: PASS, including every pre-existing test in `flashloan.rs`.

- [ ] **Step 5: Commit**

```bash
git add src/core/flashloan.rs
git commit -m "feat(2d-c2c): add canonical execute_* methods, structurally unreachable to send_and_confirm"
```

---

## Task 10: Signer/broadcaster reachability tests

**Files:**
- Modify: `src/core/flashloan.rs`
- Test: same file

**Interfaces:** none new — this task only adds tests proving Task 9's structural guarantee, per the spec's explicit test names.

- [ ] **Step 1: Write the tests**

```rust
mod signer_broadcaster_gate_tests {
    use super::super::*;

    #[tokio::test]
    async fn canonical_shadow_cannot_load_production_signer() {
        // test_client() is built with a stub middleware carrying no funded
        // production wallet; a canonical call reaching any signing path would
        // fail with a wallet/nonce error instead of returning Ok(dry_result).
        let client = super::super::tests::test_client();
        let opp = super::canonical_test_fixtures::opportunity_with_quickswap_leg();
        let approval = super::canonical_test_fixtures::approval();
        let result = client.execute_direct_canonical(&opp, &approval).await;
        assert!(result.is_ok(), "reaching a signer would have surfaced as an error here, not Ok");
    }

    #[tokio::test]
    async fn canonical_shadow_cannot_initialize_broadcaster() {
        let client = super::super::tests::test_client();
        let opp = super::canonical_test_fixtures::opportunity_with_quickswap_leg();
        let approval = super::canonical_test_fixtures::approval();
        let result = client.execute_flashloan_canonical(&opp, &approval).await.unwrap();
        // BundleResult from canonical_dry_result never carries a tx_hash —
        // the only way to obtain one is send_and_confirm_transaction, which
        // no canonical_* method calls.
        assert!(result.tx_hash.is_none());
    }
}
```

- [ ] **Step 2: Run to verify they fail before any code exists**

N/A — Task 9 already implements the methods these exercise; this task adds coverage only. Run:

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib flashloan::signer_broadcaster_gate_tests -- --nocapture`
Expected: PASS immediately (Task 9's implementation already satisfies these).

- [ ] **Step 3: Commit**

```bash
git add src/core/flashloan.rs
git commit -m "test(2d-c2c): prove canonical shadow path cannot reach signer/broadcaster"
```

---

## Task 11: Extract `discover_at` from the bin into the lib

**Files:**
- Modify: `src/core/c2b_round.rs` (add `discover_at` and its dependency struct)
- Modify: `src/bin/phase2d_c2b_fresh_discovery.rs` (`run_discovery_round` becomes a thin wrapper)
- Test: `src/core/c2b_round.rs`

**Interfaces:**
- Produces:
```rust
pub struct C2BRoundDeps<'a> {
    pub provider: &'a Arc<Provider<Http>>,
    pub cfg: &'a Config,
    pub registry: &'a RejectedRouteRegistry,
    pub symbols: &'a [String],
    pub profile: &'a str,
    pub diagnostics_dir: &'a Path,
    pub archive_rpc: &'a str,
    pub round_id: usize,
}
pub async fn discover_at(anchor: AnchorBlock, deps: &C2BRoundDeps<'_>) -> Result<Vec<RoundEvidence>>;
```
Task 14 (`c2b_shadow_service.rs`) is `discover_at`'s other caller, alongside the bin.

**This is a mechanical move, not a rewrite.** `src/bin/phase2d_c2b_fresh_discovery.rs::run_discovery_round` (currently lines 460-1053, verified by reading the file: token/pool resolution → quote graph → structural cycle discovery → per-route requote/leg-quote assembly → `CanonicalExecutionContext` build/persist/reload/verify → materialize → Anvil fork execution via `RealForkStages`/`execute_route`) already contains the entire real pipeline this task needs. Do not reimplement any of it.

- [ ] **Step 1: Copy `run_discovery_round`'s body into `discover_at`, unchanged**

In `src/core/c2b_round.rs`, add the imports `run_discovery_round` currently uses from `flashloan_bot::core::*` (they're all already `pub` in the lib — check each import at the top of the bin file, lines 36-68, and mirror only the `core::*` ones; CLI/report-only imports like `clap`, the bin's own `DiscoveryRound`/`RouteResult` structs, and diagnostics-writing helpers stay in the bin). Paste the body of `run_discovery_round` verbatim into a new function:

```rust
pub async fn discover_at(
    anchor: crate::core::phase2d_anchor::AnchorBlock,
    deps: &C2BRoundDeps<'_>,
) -> anyhow::Result<Vec<RoundEvidence>> {
    // ... verbatim body of run_discovery_round, up to (not including) the
    // final `Ok(DiscoveryRound { ... })` construction ...
}
```

Change every unqualified reference to the old function's parameters (`provider`, `cfg`, `registry`, `round_id`, `symbols`, `profile`, `diagnostics_dir`, `archive_rpc`) to read from `deps.provider`, `deps.cfg`, etc. (or destructure `deps` into locals with the same names at the top of the function body — the smaller diff — `let C2BRoundDeps { provider, cfg, registry, symbols, profile, diagnostics_dir, archive_rpc, round_id } = *deps;` requires each field to be `Copy`/reference already, which they are since they're all `&'a ...`).

- [ ] **Step 2: Replace the tail — build `Vec<RoundEvidence>` instead of `RouteResult`/`DiscoveryRound`**

The existing body's tail loops over `eligible_keys`, calls `execute_route(&mut stages, key, rejected, route.route_input)`, and mutates `results[idx]` (a `RouteResult`, f64/String) via `apply_fork_evidence`. Instead, for each `(key, route)` that reaches this stage, construct one `RoundEvidence` directly from the already-typed values already in scope at that point in the loop — `stages.evidence.get(key).cloned()` (a `RouteExecutionRecord`), the `outcome` from `execute_route` (an `OrchestratorEvidence` on `Ok`), the `materialize(...)` result (an `ExecutableRoutePlan`), `anchor`, `route.route_input`, and `registry.is_rejected(key)`:

```rust
let mut evidences = Vec::new();
// ... inside the existing per-key loop, after computing `outcome` and `record` ...
if let Some(route_plan) = materialize(
    route, anchor.number, Address::from_low_u64_be(1),
    &token_records, &pool_records, &venue_records,
    registry.is_rejected(key), route.route_input,
).ok() {
    evidences.push(RoundEvidence {
        structural_cycle_key: key.clone(),
        anchor: anchor.clone(),
        context_hash: reloaded_context.context_hash,
        route_plan,
        amount_in: route.route_input,
        execution_profile: crate::core::execution_profile::ExecutionProfile {
            chain_id: 137,
            profile_label: deps.profile.to_string(),
        },
        economics: record.as_ref().and_then(|r| r.economics.clone()),
        gross_pnl_atomic: record.as_ref().and_then(|r| r.gross_pnl_atomic),
        gas_used_total: record.as_ref().map(|r| r.gas_used_total).unwrap_or(0),
        orchestrator_evidence: outcome.ok(),
        rejected_registry_hit: registry.is_rejected(key),
    });
}
```

Return `Ok(evidences)` at the end of `discover_at` instead of `Ok(DiscoveryRound { ... })`.

- [ ] **Step 3: Turn `run_discovery_round` (bin) into a thin wrapper**

In `src/bin/phase2d_c2b_fresh_discovery.rs`, replace the body of `run_discovery_round` with a call to `flashloan_bot::core::c2b_round::discover_at`, then translate `Vec<RoundEvidence>` back into the existing `DiscoveryRound`/`Vec<RouteResult>` shape the bin's own report/CSV/JSON writers already consume (counts like `read_only_pass`, `preflight_pass`, `economic_candidates` become `evidences.iter().filter(|e| e.orchestrator_evidence.as_ref().is_some_and(|o| o.readonly_pass)).count()`, etc.; `RouteResult.gross_pnl`/`net_pnl` (f64) come from `evidence.economics.as_ref().map(|e| e.gross_pnl_atomic as f64).unwrap_or(0.0)` — the stringification the bin needs for its report is now a one-way translation at this boundary, not something computed inline mid-pipeline). Keep the bin's `DiscoveryRound`/`RouteResult` structs and every downstream report/CSV/JSON-writing function exactly as they are — only the body that produces the numbers changes.

- [ ] **Step 4: Run the bin's existing tests / smoke path to verify no behavior change**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo check --bin phase2d_c2b_fresh_discovery`
Expected: compiles clean.

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib c2b_round -- --nocapture`
Expected: Task 2's existing `RoundEvidence` tests still pass (this task didn't change that struct's shape).

If the project has a recorded-fixture or golden-file test exercising `phase2d_c2b_fresh_discovery` end-to-end (check `diagnostics/phase2d_c2b_*` for anything that looks like a checked-in expected-output fixture, and check for a `tests/` integration test referencing this bin) run it now and confirm output is unchanged; if none exists, this step is: run the bin once against a `--rpc-url` the operator provides (manual, not part of automated CI) and diff the produced `diagnostics/phase2d_c2b_discovery_*.md` shape against a pre-refactor run — note in the commit message that this was checked manually since no automated fixture exists.

- [ ] **Step 5: Commit**

```bash
git add src/core/c2b_round.rs src/bin/phase2d_c2b_fresh_discovery.rs
git commit -m "refactor(2d-c2c): extract discover_at pipeline from bin into lib, bin becomes thin wrapper"
```

---

## Task 12: Bin's stability check delegates to `stability::is_stable`

**Files:**
- Modify: `src/bin/phase2d_c2b_fresh_discovery.rs` (`stable_candidate_keys`)
- Test: existing bin has no `#[cfg(test)]` for this function today (verify by grepping `stable_candidate_keys` for a `mod tests` nearby) — if none exists, this task adds one; if one exists, extend it.

**Interfaces:** none new — `stable_candidate_keys` keeps its existing signature (`fn stable_candidate_keys(rounds: &[DiscoveryRound]) -> Vec<String>`), only its internal per-key boolean check changes.

- [ ] **Step 1: Write/extend a test proving the bin's batch check and the lib's rolling check agree**

```rust
#[cfg(test)]
mod stable_candidate_keys_tests {
    use super::*;

    // Build two DiscoveryRounds whose RouteResult flags would satisfy the
    // pre-refactor inline predicate, convert each into the RoundEvidence
    // shape `stability::is_stable` expects (via the same translation added
    // in Task 11 Step 3), and assert `stable_candidate_keys` still returns
    // the key.
    #[test]
    fn stable_route_across_three_rounds_is_still_reported_stable() {
        // construct 3 DiscoveryRounds with one matching RouteResult each,
        // all economic_positive/read_only PASS/preflight PASS/trace_validated,
        // distinct anchor_block per round — mirrors the existing inline
        // fixture style already used elsewhere in this bin's tests, if any;
        // otherwise build RouteResult literals directly (all fields are
        // public in this file).
        let rounds = vec![/* ... 3 DiscoveryRound literals ... */];
        assert_eq!(stable_candidate_keys(&rounds), vec!["k".to_string()]);
    }
}
```

(This step's exact fixture literals depend on `RouteResult`'s full field list already shown in Task 2's context — reuse those defaults, setting only the fields `is_stable`'s predicate reads.)

- [ ] **Step 2: Run to verify current (pre-change) behavior — this is a characterization test, expected to already PASS**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --bin phase2d_c2b_fresh_discovery stable_candidate_keys_tests -- --nocapture`
Expected: PASS against the current inline implementation (this locks in current behavior before refactoring).

- [ ] **Step 3: Replace the inline predicate with a call to `stability::is_stable`**

Inside `stable_candidate_keys`, the loop currently computes `all_economic`/`all_readonly`/`all_preflight`/`all_trace`/`sign_flip`/`rejected_hit` directly from `entries: &[&RouteResult]` and combines them into `stable`. Replace that block: convert each `&RouteResult` entry into a minimal `flashloan_bot::core::c2b_round::RoundEvidence` (only the fields `is_stable` reads are needed — `anchor.number` from `entries[i].anchor_block`, `anchor.hash` synthesized deterministically from `entries[i].round_id` since `RouteResult` doesn't carry the real block hash today — see note below, `rejected_registry_hit`, `economics.net_pnl_atomic` from `entries[i].net_pnl as i128`, `orchestrator_evidence` fields from the `*_status`/`*_validated` string/bool fields), then call `flashloan_bot::core::stability::is_stable(&converted)`.

Note: `RouteResult.anchor_block_hash` doesn't exist as a field on `RouteResult` (only `DiscoveryRound.anchor_block_hash` does) — thread it through by looking up the parent round's `anchor_block_hash` for each entry (the existing `by_key` grouping loop already iterates `rounds` outer / `round.discovery_results` inner, so `round.anchor_block_hash` is in scope at the point each `RouteResult` is pushed into `entries`; capture `(round.anchor_block_hash, result)` pairs instead of bare `&RouteResult` references to preserve this).

- [ ] **Step 4: Run the characterization test again to verify no behavior change**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --bin phase2d_c2b_fresh_discovery stable_candidate_keys_tests -- --nocapture`
Expected: PASS, identical result to Step 2.

- [ ] **Step 5: Commit**

```bash
git add src/bin/phase2d_c2b_fresh_discovery.rs
git commit -m "refactor(2d-c2c): bin's 3-of-3 check delegates to shared stability::is_stable"
```

---

## Task 13: `C2bShadowConfig`

**Files:**
- Modify: `src/config/mod.rs`
- Modify: `config/config.dryrun.toml`
- Test: `src/config/mod.rs` (or wherever existing config-loading tests live — grep for `mod tests` near the `Config` struct)

**Interfaces:**
```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct C2bShadowConfig {
    #[serde(default)]
    pub shadow_enabled: bool,               // CANONICAL_C2B_SHADOW_ENABLED
    #[serde(default)]
    pub primary_enabled: bool,              // CANONICAL_C2B_PRIMARY_ENABLED — always false in 2D-C2C
    #[serde(default)]
    pub broadcast_enabled: bool,            // CANONICAL_C2B_BROADCAST_ENABLED — always false in 2D-C2C
    #[serde(default = "default_shadow_every_n_blocks")]
    pub shadow_every_n_blocks: u64,
    #[serde(default = "default_round_timeout_secs")]
    pub round_timeout_secs: u64,
    #[serde(default)]
    pub dedicated_rpc_url: Option<String>,
}
fn default_shadow_every_n_blocks() -> u64 { 32 }
fn default_round_timeout_secs() -> u64 { 60 }
impl Default for C2bShadowConfig { /* shadow_enabled/primary_enabled/broadcast_enabled all false */ }
```
Add `#[serde(default)] pub c2b_shadow: C2bShadowConfig,` to `Config` (alongside the other `#[serde(default)]` sections like `mev`/`monitoring`).

Task 15/16/17 read `cfg.c2b_shadow.*` for scheduling and enablement checks.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod c2b_shadow_config_tests {
    use super::*;

    #[test]
    fn defaults_to_fully_disabled() {
        let cfg = C2bShadowConfig::default();
        assert!(!cfg.shadow_enabled);
        assert!(!cfg.primary_enabled);
        assert!(!cfg.broadcast_enabled);
    }

    #[test]
    fn missing_toml_section_deserializes_to_defaults() {
        let cfg: Config = toml::from_str(include_str!("../../config/config.dryrun.toml")).unwrap();
        assert!(!cfg.c2b_shadow.shadow_enabled);
        assert_eq!(cfg.c2b_shadow.shadow_every_n_blocks, 32);
    }
}
```

(If `Config` in existing tests is loaded via a different helper than raw `toml::from_str` + `include_str!` — grep `src/config/mod.rs` for an existing `#[cfg(test)]` that loads `config.dryrun.toml` and reuse that exact loading path instead.)

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib c2b_shadow_config -- --nocapture`
Expected: FAIL — `C2bShadowConfig`/`Config.c2b_shadow` not found.

- [ ] **Step 3: Implement**

Add the struct (shown above) near the other `#[serde(default)]` config structs in `src/config/mod.rs`, add the field to `Config`, and add to `config/config.dryrun.toml` (after the `[wrapper]` section, matching the file's existing ordering-by-introduction convention):

```toml
[c2b_shadow]
shadow_enabled = false
primary_enabled = false
broadcast_enabled = false
shadow_every_n_blocks = 32
round_timeout_secs = 60
```

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib c2b_shadow_config -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/config/mod.rs config/config.dryrun.toml
git commit -m "feat(2d-c2c): add c2b_shadow config section, disabled by default"
```

---

## Task 14: `c2b_shadow_service.rs` — service loop skeleton

**Files:**
- Create: `src/core/c2b_shadow_service.rs`
- Modify: `src/core/mod.rs`
- Test: same file

**Interfaces:**
```rust
pub struct C2BShadowResult {
    pub anchor: AnchorBlock,
    pub round_evidences: Vec<RoundEvidence>,
    pub stable_opportunities: Vec<ExecutableOpportunity>,
    pub risk_approvals: Vec<(H256, Result<RiskApproval, Vec<CanonicalRiskRejection>>)>, // keyed by opportunity_id
    pub strategy_decisions: Vec<(H256, CanonicalStrategyDecision)>,
    pub execution_results: Vec<(H256, BundleResult)>,
}
pub struct CanonicalC2BOpportunitySource {
    aggregator: StableOpportunityAggregator,
    risk_manager: RiskManager,
    client: ArbitrageClient,
    canonical_risk_cfg: CanonicalRiskConfig,
}
impl CanonicalC2BOpportunitySource {
    pub fn new(risk_manager: RiskManager, client: ArbitrageClient, canonical_risk_cfg: CanonicalRiskConfig) -> Self;
    /// Runs one full round: discover_at -> push each RoundEvidence into the
    /// aggregator -> for each newly-stable opportunity, run risk -> strategy
    /// -> execute_*_canonical (dry). Never touches send_and_confirm.
    pub async fn run_round(&mut self, anchor: AnchorBlock, deps: &C2BRoundDeps<'_>, cfg: &Config, current_head_block: u64) -> anyhow::Result<C2BShadowResult>;
}
```
Task 17 (bot.rs wiring) owns one `CanonicalC2BOpportunitySource` inside the dedicated-runtime task and calls `run_round` once per scheduled anchor.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_result_starts_empty_for_fresh_aggregator_single_round() {
        // A single round can never itself be stable (needs 3) — this locks
        // in that a lone discover_at call never fabricates an opportunity.
        let result = C2BShadowResult {
            anchor: crate::core::phase2d_anchor::AnchorBlock { number: 1, hash: Default::default(), selected_from_head: 3, confirmation_lag: 2 },
            round_evidences: vec![],
            stable_opportunities: vec![],
            risk_approvals: vec![],
            strategy_decisions: vec![],
            execution_results: vec![],
        };
        assert!(result.stable_opportunities.is_empty());
    }
}
```

This task's test is intentionally light — `run_round`'s real integration behavior (discover_at → aggregator → risk → strategy → execute) is what Task 20's E2E test exercises against a fake `C2BRoundDeps` provider; this task only needs to prove the struct/method wiring compiles and that the trivial "one round, zero stability" case doesn't fabricate output.

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib c2b_shadow_service -- --nocapture`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement**

```rust
// src/core/c2b_shadow_service.rs
use crate::core::{
    c2b_round::{discover_at, C2BRoundDeps, RoundEvidence},
    executable_opportunity::{from_stable_rounds, ExecutableOpportunity},
    flashloan::{determine_execution_strategy_canonical, ArbitrageClient, CanonicalStrategyDecision},
    phase2d_anchor::AnchorBlock,
    risk::{CanonicalRiskConfig, CanonicalRiskRejection, RiskApproval, RiskManager},
    stability::StableOpportunityAggregator,
    types::BundleResult,
};
use crate::config::Config;
use ethers::types::H256;

pub struct C2BShadowResult {
    pub anchor: AnchorBlock,
    pub round_evidences: Vec<RoundEvidence>,
    pub stable_opportunities: Vec<ExecutableOpportunity>,
    pub risk_approvals: Vec<(H256, Result<RiskApproval, Vec<CanonicalRiskRejection>>)>,
    pub strategy_decisions: Vec<(H256, CanonicalStrategyDecision)>,
    pub execution_results: Vec<(H256, BundleResult)>,
}

pub struct CanonicalC2BOpportunitySource {
    aggregator: StableOpportunityAggregator,
    risk_manager: RiskManager,
    client: ArbitrageClient,
    canonical_risk_cfg: CanonicalRiskConfig,
}

impl CanonicalC2BOpportunitySource {
    pub fn new(risk_manager: RiskManager, client: ArbitrageClient, canonical_risk_cfg: CanonicalRiskConfig) -> Self {
        Self { aggregator: StableOpportunityAggregator::new(), risk_manager, client, canonical_risk_cfg }
    }

    pub async fn run_round(
        &mut self,
        anchor: AnchorBlock,
        deps: &C2BRoundDeps<'_>,
        cfg: &Config,
        current_head_block: u64,
    ) -> anyhow::Result<C2BShadowResult> {
        let round_evidences = discover_at(anchor.clone(), deps).await?;

        let mut stable_opportunities = Vec::new();
        for evidence in round_evidences.clone() {
            if let Some(rounds) = self.aggregator.push(evidence) {
                if let Some(opp) = from_stable_rounds(rounds) {
                    stable_opportunities.push(opp);
                }
            }
        }

        let mut risk_approvals = Vec::new();
        let mut strategy_decisions = Vec::new();
        let mut execution_results = Vec::new();

        for opp in &stable_opportunities {
            let approval_result = self.risk_manager.assess_executable_opportunity(opp, &self.canonical_risk_cfg, current_head_block);
            risk_approvals.push((opp.opportunity_id, approval_result.clone()));

            let Ok(approval) = approval_result else { continue };

            let decision = determine_execution_strategy_canonical(opp, &approval, cfg);
            strategy_decisions.push((opp.opportunity_id, decision.clone()));

            let result = match decision {
                CanonicalStrategyDecision::Direct => Some(self.client.execute_direct_canonical(opp, &approval).await),
                CanonicalStrategyDecision::Flashloan => Some(self.client.execute_flashloan_canonical(opp, &approval).await),
                CanonicalStrategyDecision::WrapperFlashloan => Some(self.client.execute_wrapper_canonical(opp, &approval).await),
                CanonicalStrategyDecision::Skip(_) => None,
            };
            if let Some(Ok(bundle_result)) = result {
                execution_results.push((opp.opportunity_id, bundle_result));
            }
        }

        Ok(C2BShadowResult { anchor, round_evidences, stable_opportunities, risk_approvals, strategy_decisions, execution_results })
    }
}
```

`RiskApproval` needs `Clone` for the `risk_approvals.push((.., approval_result.clone()))` line — it's already `Debug, Clone, Copy` from Task 5. `Vec<CanonicalRiskRejection>` (the `Err` variant) needs `Clone` too — `CanonicalRiskRejection` already derives `Clone` from Task 5, and `Vec<T: Clone>` is `Clone` automatically. `CanonicalStrategyDecision` needs `Clone` — already derives it from Task 6.

Add `pub mod c2b_shadow_service;` to `src/core/mod.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib c2b_shadow_service -- --nocapture`
Expected: PASS. Also run `CARGO_TARGET_DIR=target/phase2d_c2c cargo check --workspace` here since this task is the first to wire every prior module together — catch any signature mismatch now rather than in Task 20.

- [ ] **Step 5: Commit**

```bash
git add src/core/c2b_shadow_service.rs src/core/mod.rs
git commit -m "feat(2d-c2c): add CanonicalC2BOpportunitySource wiring discover_at through to canonical execution"
```

---

## Task 15: Scheduler math + reorg invalidation (pure)

**Files:**
- Create: within `src/core/c2b_shadow_service.rs` (add to the same file — small, tightly related to the service it schedules for)
- Test: same file

**Interfaces:**
```rust
pub fn should_schedule(new_block: u64, last_scheduled_anchor: Option<u64>, every_n_blocks: u64) -> bool;
pub fn anchor_still_valid(anchor: &AnchorBlock, observed: Option<ethers::types::Block<ethers::types::H256>>) -> bool; // wraps phase2d_anchor::reorg_detected, inverted
```

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod scheduler_tests {
    use super::*;

    #[test]
    fn schedules_first_anchor_when_none_scheduled_yet() {
        assert!(should_schedule(100, None, 32));
    }

    #[test]
    fn does_not_schedule_before_n_blocks_elapsed() {
        assert!(!should_schedule(115, Some(100), 32));
    }

    #[test]
    fn schedules_exactly_at_n_blocks() {
        assert!(should_schedule(132, Some(100), 32));
    }

    #[test]
    fn schedules_past_n_blocks_does_not_repeat_stale_math() {
        // A block far past the threshold (e.g. after downtime) still
        // schedules exactly once — should_schedule is a boolean gate, not a
        // counter, so the caller updates last_scheduled_anchor to the new
        // anchor's number immediately after this returns true.
        assert!(should_schedule(500, Some(100), 32));
    }

    #[test]
    fn never_repeats_same_anchor() {
        assert!(!should_schedule(100, Some(100), 32));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib c2b_shadow_service::scheduler_tests -- --nocapture`
Expected: FAIL — `should_schedule` not found.

- [ ] **Step 3: Implement**

```rust
pub fn should_schedule(new_block: u64, last_scheduled_anchor: Option<u64>, every_n_blocks: u64) -> bool {
    match last_scheduled_anchor {
        None => true,
        Some(last) => new_block >= last.saturating_add(every_n_blocks),
    }
}

pub fn anchor_still_valid(
    anchor: &AnchorBlock,
    observed: Option<ethers::types::Block<ethers::types::H256>>,
) -> bool {
    !crate::core::phase2d_anchor::reorg_detected(anchor, observed)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib c2b_shadow_service::scheduler_tests -- --nocapture`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add src/core/c2b_shadow_service.rs
git commit -m "feat(2d-c2c): add pure scheduler gate and reorg-invalidation check"
```

---

## Task 16: Dedicated Tokio runtime + channel plumbing

**Files:**
- Modify: `src/core/c2b_shadow_service.rs`
- Test: same file

**Interfaces:**
```rust
pub struct C2BShadowHandle {
    pub anchor_tx: tokio::sync::mpsc::Sender<AnchorBlock>,   // capacity 1
    pub result_rx: tokio::sync::mpsc::Receiver<C2BShadowResult>,
    join_handle: std::thread::JoinHandle<()>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
}
pub fn spawn_shadow_runtime(
    mut source: CanonicalC2BOpportunitySource,
    rpc_url: String,
    cfg_snapshot: Config,
) -> C2BShadowHandle;
impl C2BShadowHandle {
    pub fn try_send_anchor(&self, anchor: AnchorBlock) -> Result<(), &'static str>; // "PREVIOUS_ROUND_IN_FLIGHT" on full channel, matches spec's C2B_SKIP_REASON
    pub async fn shutdown(self);
}
```
Task 17 (`bot.rs`) is the sole owner/caller of `spawn_shadow_runtime` and `C2BShadowHandle`.

`anchor_tx`/`result_rx` are `mpsc::channel(1)` — capacity 1 is the mechanism behind "canal de anchors com capacidade 1" / "nenhum backlog" / "anchor antigo descartado": `try_send` on a full channel returns `Err(TrySendError::Full)` immediately rather than queuing, which `try_send_anchor` maps to the `"PREVIOUS_ROUND_IN_FLIGHT"` skip reason.

The service loop inside the spawned thread owns its own `Provider<Http>` (built from `rpc_url`, never the legacy `AppMiddleware`/executor's provider) — this is `C2B_USES_DEDICATED_RPC_POOL`.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod runtime_tests {
    use super::*;

    #[tokio::test]
    async fn full_anchor_channel_reports_previous_round_in_flight() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<AnchorBlock>(1);
        let anchor = AnchorBlock { number: 1, hash: Default::default(), selected_from_head: 3, confirmation_lag: 2 };
        tx.try_send(anchor.clone()).unwrap(); // fill the one slot
        let err = tx.try_send(anchor).unwrap_err();
        assert!(matches!(err, tokio::sync::mpsc::error::TrySendError::Full(_)));
    }
}
```

This test only proves the channel primitive behaves as assumed (capacity-1 `try_send` semantics) — `spawn_shadow_runtime` itself requires a real RPC endpoint to exercise end-to-end and is covered by Task 20's E2E test with a fake provider, not here.

- [ ] **Step 2: Run test to verify it passes immediately (characterizes tokio's documented behavior)**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib c2b_shadow_service::runtime_tests -- --nocapture`
Expected: PASS (this is a characterization test of a stdlib/tokio guarantee, not new production code — write it first anyway per TDD discipline, then implement).

- [ ] **Step 3: Implement `spawn_shadow_runtime`/`C2BShadowHandle`**

```rust
pub struct C2BShadowHandle {
    pub anchor_tx: tokio::sync::mpsc::Sender<AnchorBlock>,
    pub result_rx: tokio::sync::mpsc::Receiver<C2BShadowResult>,
    join_handle: std::thread::JoinHandle<()>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
}

impl C2BShadowHandle {
    pub fn try_send_anchor(&self, anchor: AnchorBlock) -> Result<(), &'static str> {
        self.anchor_tx.try_send(anchor).map_err(|_| "PREVIOUS_ROUND_IN_FLIGHT")
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = tokio::task::spawn_blocking(move || self.join_handle.join()).await;
    }
}

pub fn spawn_shadow_runtime(
    mut source: CanonicalC2BOpportunitySource,
    rpc_url: String,
    cfg_snapshot: Config,
) -> C2BShadowHandle {
    let (anchor_tx, mut anchor_rx) = tokio::sync::mpsc::channel::<AnchorBlock>(1);
    let (result_tx, result_rx) = tokio::sync::mpsc::channel::<C2BShadowResult>(1);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let join_handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("C2B shadow runtime build failed");

        runtime.block_on(async move {
            let provider = match ethers::providers::Provider::<ethers::providers::Http>::try_from(rpc_url.as_str()) {
                Ok(p) => std::sync::Arc::new(p),
                Err(e) => {
                    tracing::warn!(error = %e, "C2B shadow: dedicated RPC provider init failed, service exiting");
                    return;
                }
            };
            let registry = crate::core::execution_viability::RejectedRouteRegistry::default();
            let symbols: Vec<String> = cfg_snapshot.addresses.keys().cloned().collect();

            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() { break; }
                    }
                    Some(anchor) = anchor_rx.recv() => {
                        let deps = crate::core::c2b_round::C2BRoundDeps {
                            provider: &provider,
                            cfg: &cfg_snapshot,
                            registry: &registry,
                            symbols: &symbols,
                            profile: "base",
                            diagnostics_dir: std::path::Path::new("diagnostics"),
                            archive_rpc: &rpc_url,
                            round_id: anchor.number as usize,
                        };
                        let current_head = anchor.selected_from_head;
                        let round_timeout = std::time::Duration::from_secs(cfg_snapshot.c2b_shadow.round_timeout_secs);
                        let outcome = tokio::time::timeout(
                            round_timeout,
                            source.run_round(anchor.clone(), &deps, &cfg_snapshot, current_head),
                        ).await;
                        match outcome {
                            Ok(Ok(result)) => { let _ = result_tx.try_send(result); }
                            Ok(Err(e)) => tracing::warn!(error = %e, anchor = anchor.number, "C2B shadow round failed, legacy loop unaffected"),
                            Err(_) => tracing::warn!(anchor = anchor.number, "C2B shadow round timed out, legacy loop unaffected"),
                        }
                    }
                }
            }
        });
    });

    C2BShadowHandle { anchor_tx, result_rx, join_handle, shutdown_tx }
}
```

`RejectedRouteRegistry` needs `Default` — check its existing definition (`execution_viability.rs`); if it doesn't derive `Default` today, add the derive there as part of this step (pure addition).

- [ ] **Step 4: Run tests**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo check --workspace`
Expected: compiles clean.

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib c2b_shadow_service:: -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/core/c2b_shadow_service.rs src/core/execution_viability.rs
git commit -m "feat(2d-c2c): spawn C2B shadow service on dedicated runtime with capacity-1 anchor channel"
```

---

## Task 17: Wire into `bot.rs`

**Files:**
- Modify: `src/core/bot.rs`
- Test: same file (`bot.rs` currently has `#[cfg(test)]` near the bottom per the file listing — extend it)

**Interfaces:**
- `Bot` gains an optional field: `pub c2b_shadow: Option<crate::core::c2b_shadow_service::C2BShadowHandle>` plus `last_scheduled_c2b_anchor: Option<u64>`.
- `Bot::run` gains a third argument, `mut block_rx: mpsc::Receiver<ethers::types::Block<ethers::types::H256>>`, and a new `tokio::select!` arm.

This task is additive to `run()`'s existing `tokio::select!` — the existing two arms (`shutdown_rx.recv()`, `price_rx.recv()`) are not touched, satisfying `c2b_shadow_disabled_preserves_legacy_behavior` structurally (when `c2b_shadow` is `None`, the new arm never fires because there's nothing sending on `block_rx` from the caller's side when the feature is disabled — see Step 3).

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod c2b_shadow_wiring_tests {
    use super::*;

    #[tokio::test]
    async fn shadow_disabled_bot_ignores_block_rx_and_processes_price_rx_normally() {
        // Construct a Bot with c2b_shadow: None (the disabled default),
        // send one price tick, send shutdown, and confirm run() returns
        // Ok(()) without ever touching c2b_shadow-only state. This is a
        // structural smoke test — full price_rx processing is already
        // covered by bot.rs's pre-existing tests; this test only proves
        // the new run() signature doesn't change that path.
        // (Reuse whatever `Bot` test-construction helper bot.rs's existing
        // tests already use — grep this file's `#[cfg(test)] mod tests`
        // for how it builds a `Bot` today, before writing this test.)
    }
}
```

Before writing this test's body, grep `src/core/bot.rs`'s existing `#[cfg(test)]` block for its `Bot`-construction helper and follow the same pattern — do not invent a second one.

- [ ] **Step 2: Run to verify it fails to compile (new `run()` signature doesn't exist yet)**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib bot::c2b_shadow_wiring_tests -- --nocapture`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

Add the field to `Bot`:

```rust
pub struct Bot {
    pub config: Arc<Mutex<Config>>,
    pub arbitrage_engine: ArbitrageEngine,
    pub risk_manager: RiskManager,
    pub arbitrage_client: ArbitrageClient,
    pub execution_mode: ExecutionMode,
    pub telegram: Arc<TelegramNotifier>,
    pub c2b_shadow: Option<crate::core::c2b_shadow_service::C2BShadowHandle>,
    last_scheduled_c2b_anchor: Option<u64>,
}
```

In `new_with_engine`, after the existing initialization, conditionally spawn the shadow service:

```rust
let c2b_shadow = {
    let cfg_guard = config.lock().await;
    if cfg_guard.c2b_shadow.shadow_enabled {
        let rpc_url = cfg_guard.c2b_shadow.dedicated_rpc_url.clone()
            .unwrap_or_else(|| cfg_guard.network.rpc_url.clone()); // fallback only for local/dryrun testing; production should always set dedicated_rpc_url
        let canonical_risk_cfg = crate::core::risk::CanonicalRiskConfig {
            absolute_min_profit_floor_raw: ethers::types::U256::zero(),
            retention_bps: 2_000,
            max_gas_raw: ethers::types::U256::from(2_000_000u64),
            max_slippage_bps: 100,
            max_anchor_age_blocks: 32,
        };
        let source = crate::core::c2b_shadow_service::CanonicalC2BOpportunitySource::new(
            RiskManager::new(cfg_guard.risk.clone()),
            arbitrage_client.clone(),
            canonical_risk_cfg,
        );
        let cfg_snapshot = cfg_guard.clone();
        drop(cfg_guard);
        Some(crate::core::c2b_shadow_service::spawn_shadow_runtime(source, rpc_url, cfg_snapshot))
    } else {
        None
    }
};
```

Add `c2b_shadow` and `last_scheduled_c2b_anchor: None` to the final `Self { ... }` construction (every existing `Bot { ... }` literal in this file — there's exactly one, in `new_with_engine`, since `init`/`init_with_engine` both delegate to it).

Change `run`'s signature and body:

```rust
pub async fn run(
    &mut self,
    mut price_rx: mpsc::Receiver<HashMap<String, HashMap<String, f64>>>,
    mut shutdown_rx: broadcast::Receiver<()>,
    mut block_rx: mpsc::Receiver<ethers::types::Block<ethers::types::H256>>,
) -> Result<(), anyhow::Error> {
    info!("🤖 Bot iniciado — modo {:?}", self.execution_mode);
    metrics::set_bot_status(1);

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            Some(prices) = price_rx.recv() => {
                let _ = self.process_prices(prices).await;
            }
            Some(block) = block_rx.recv() => {
                self.maybe_schedule_c2b_round(block).await;
            }
            Some(result) = Self::poll_c2b_result(&mut self.c2b_shadow) => {
                info!(
                    anchor = result.anchor.number,
                    stable = result.stable_opportunities.len(),
                    "C2B shadow round complete"
                );
                // DualRunComparator wiring lands in Task 19.
            }
        }
    }

    if let Some(handle) = self.c2b_shadow.take() {
        handle.shutdown().await;
    }

    metrics::set_bot_status(0);
    Ok(())
}

async fn maybe_schedule_c2b_round(&mut self, block: ethers::types::Block<ethers::types::H256>) {
    let Some(handle) = &self.c2b_shadow else { return };
    let Some(number) = block.number.map(|n| n.as_u64()) else { return };
    let Some(hash) = block.hash else { return };
    let every_n = { self.config.lock().await.c2b_shadow.shadow_every_n_blocks };
    if !crate::core::c2b_shadow_service::should_schedule(number, self.last_scheduled_c2b_anchor, every_n) {
        return;
    }
    let anchor = crate::core::phase2d_anchor::AnchorBlock {
        number, hash, selected_from_head: number, confirmation_lag: 0,
    };
    match handle.try_send_anchor(anchor) {
        Ok(()) => self.last_scheduled_c2b_anchor = Some(number),
        Err(reason) => debug!(reason, block = number, "C2B shadow round skipped"),
    }
}

async fn poll_c2b_result(
    handle: &mut Option<crate::core::c2b_shadow_service::C2BShadowHandle>,
) -> Option<crate::core::c2b_shadow_service::C2BShadowResult> {
    match handle {
        Some(h) => h.result_rx.recv().await,
        None => std::future::pending().await,
    }
}
```

(`poll_c2b_result` returning `std::future::pending()` when `c2b_shadow` is `None` is what makes that `tokio::select!` arm structurally inert for disabled shadow — it never resolves, so `select!` never picks it, matching the "legacy loop behavior unchanged when disabled" requirement without an `if` branch inside the loop body.)

Update every call site of `Bot::run(...)` (search the codebase — likely `src/main.rs` or wherever the bot is started) to pass a `block_rx`. If no block-subscription channel exists yet at the call site, this task also adds a minimal producer: a `tokio::spawn`ed task using the existing production `AppMiddleware`'s `watch_blocks()` (ethers-provided) forwarding to an `mpsc::channel(1)` — capacity 1 here too, since only the latest block number matters for scheduling, matching "nenhum backlog" for this input as well. Locate the exact call site before writing this producer; do not guess its shape without reading `main.rs`.

- [ ] **Step 4: Run tests**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo check --workspace`
Expected: compiles clean (this will surface any `Bot::run` call sites missed above — fix them).

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib bot:: -- --nocapture`
Expected: PASS, including every pre-existing `bot.rs` test.

- [ ] **Step 5: Commit**

```bash
git add src/core/bot.rs src/main.rs
git commit -m "feat(2d-c2c): wire C2B shadow scheduler into bot.rs run loop, legacy path unchanged"
```

---

## Task 18: `DualRunComparator`

**Files:**
- Create: `src/core/dual_run_comparator.rs`
- Modify: `src/core/mod.rs`
- Test: same file

**Interfaces:**
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DivergenceClassification {
    ExpectedModelDifference, LegacyOnly, CanonicalOnly,
    QuoteDivergence, EconomicsDivergence, RiskDecisionDivergence,
    StrategyDivergence, Unexplained,
}
pub struct LegacySnapshot { pub pair: String, pub buy_dex: String, pub sell_dex: String, pub net_profit_usd: f64, pub captured_at_block: u64 }
pub struct ComparisonRecord {
    pub classification: DivergenceClassification,
    pub legacy: Option<LegacySnapshot>,
    pub canonical: Option<crate::core::executable_opportunity::ExecutableOpportunity>,
    pub note: String,
}
pub struct DualRunComparator { recent_legacy: std::collections::VecDeque<LegacySnapshot> }
impl DualRunComparator {
    pub fn new(window: usize) -> Self;
    pub fn record_legacy(&mut self, snapshot: LegacySnapshot);
    /// Pure — returns classifications, never mutates or influences legacy/canonical state.
    pub fn compare(&self, canonical: &[crate::core::executable_opportunity::ExecutableOpportunity]) -> Vec<ComparisonRecord>;
}
```

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::executable_opportunity::{ExecutableOpportunity, ExecutionEvidence, StabilityRecord};
    use crate::core::execution_profile::ExecutionProfile;
    use crate::core::executable_route_materializer::{ExecutableRoutePlan, ForkSetupPlan};
    use crate::core::fresh_economics::PinnedStateSnapshot;
    use ethers::types::{Address, H256, U256};

    fn opp() -> ExecutableOpportunity {
        ExecutableOpportunity {
            opportunity_id: H256::repeat_byte(1), structural_cycle_key: "k".into(),
            route_plan: ExecutableRoutePlan {
                structural_cycle_key: "k".into(), anchor_block: 100, start_token: Address::zero(),
                legs: vec![], snapshot: PinnedStateSnapshot::default(),
                fork_setup: ForkSetupPlan { anchor_block: 100, caller: Address::zero(), tokens: vec![], funding: vec![], approvals: vec![], targets: vec![], balance_checks: vec![] },
            },
            anchor_block: 100, anchor_block_hash: H256::zero(), context_hash: H256::repeat_byte(1),
            evidence_hash: H256::repeat_byte(2), amount_in: U256::from(1_000u64),
            expected_amount_out: U256::from(1_100u64), gross_pnl: 100, gas_estimate: U256::from(10u64),
            net_pnl: 90, net_pnl_usd: Some(0.5), leg_quotes: vec![],
            evidence: ExecutionEvidence { eth_call_pass: true, preflight_pass: true, trace_validated: true, balance_delta: U256::from(100u64) },
            stability: StabilityRecord { anchor_blocks: [94, 97, 100], anchor_hashes: [H256::zero(); 3] },
            execution_profile: ExecutionProfile { chain_id: 137, profile_label: "base".into() },
        }
    }

    #[test]
    fn canonical_with_no_matching_legacy_is_canonical_only() {
        let comparator = DualRunComparator::new(16);
        let records = comparator.compare(&[opp()]);
        assert_eq!(records[0].classification, DivergenceClassification::CanonicalOnly);
    }

    #[test]
    fn legacy_with_no_canonical_result_is_legacy_only() {
        let mut comparator = DualRunComparator::new(16);
        comparator.record_legacy(LegacySnapshot { pair: "WETH/USDC".into(), buy_dex: "QuickSwap".into(), sell_dex: "SushiSwap".into(), net_profit_usd: 1.0, captured_at_block: 100 });
        let records = comparator.compare(&[]);
        assert_eq!(records[0].classification, DivergenceClassification::LegacyOnly);
    }

    #[test]
    fn compare_never_mutates_recorded_legacy_snapshots() {
        let mut comparator = DualRunComparator::new(16);
        comparator.record_legacy(LegacySnapshot { pair: "WETH/USDC".into(), buy_dex: "QuickSwap".into(), sell_dex: "SushiSwap".into(), net_profit_usd: 1.0, captured_at_block: 100 });
        let before = comparator.recent_legacy.len();
        let _ = comparator.compare(&[opp()]);
        assert_eq!(comparator.recent_legacy.len(), before, "compare() must be read-only over recorded state");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib dual_run_comparator -- --nocapture`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement**

```rust
// src/core/dual_run_comparator.rs
use crate::core::executable_opportunity::ExecutableOpportunity;
use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DivergenceClassification {
    ExpectedModelDifference,
    LegacyOnly,
    CanonicalOnly,
    QuoteDivergence,
    EconomicsDivergence,
    RiskDecisionDivergence,
    StrategyDivergence,
    Unexplained,
}

#[derive(Debug, Clone)]
pub struct LegacySnapshot {
    pub pair: String,
    pub buy_dex: String,
    pub sell_dex: String,
    pub net_profit_usd: f64,
    pub captured_at_block: u64,
}

#[derive(Debug, Clone)]
pub struct ComparisonRecord {
    pub classification: DivergenceClassification,
    pub legacy: Option<LegacySnapshot>,
    pub canonical: Option<ExecutableOpportunity>,
    pub note: String,
}

pub struct DualRunComparator {
    recent_legacy: VecDeque<LegacySnapshot>,
    window: usize,
}

impl DualRunComparator {
    pub fn new(window: usize) -> Self {
        Self { recent_legacy: VecDeque::new(), window }
    }

    pub fn record_legacy(&mut self, snapshot: LegacySnapshot) {
        self.recent_legacy.push_back(snapshot);
        while self.recent_legacy.len() > self.window {
            self.recent_legacy.pop_front();
        }
    }

    pub fn compare(&self, canonical: &[ExecutableOpportunity]) -> Vec<ComparisonRecord> {
        let mut records = Vec::new();
        for opp in canonical {
            records.push(ComparisonRecord {
                classification: DivergenceClassification::CanonicalOnly,
                legacy: None,
                canonical: Some(opp.clone()),
                note: "no legacy snapshot correlated for this anchor window in 2D-C2C (correlation by pair/start_token lands with real legacy-token metadata in a later phase)".into(),
            });
        }
        if canonical.is_empty() {
            for snapshot in &self.recent_legacy {
                records.push(ComparisonRecord {
                    classification: DivergenceClassification::LegacyOnly,
                    legacy: Some(snapshot.clone()),
                    canonical: None,
                    note: "no canonical opportunity produced this round".into(),
                });
            }
        }
        records
    }
}
```

`ExecutableOpportunity` needs `Clone` — already derives it (Task 4). Note the `QuoteDivergence`/`EconomicsDivergence`/`RiskDecisionDivergence`/`StrategyDivergence`/`ExpectedModelDifference`/`Unexplained` variants are defined but not yet produced by `compare()` — real pair-level correlation between legacy's symbol-based opportunities and canonical's address-based ones needs a token symbol↔address resolution this task doesn't have wired yet (Task 19 adds `record_legacy` call sites from `bot.rs`; richer correlation is out of scope for 2D-C2C's minimum bar of "canonical-only vs legacy-only" and is flagged here rather than silently guessed at).

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib dual_run_comparator -- --nocapture`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src/core/dual_run_comparator.rs src/core/mod.rs
git commit -m "feat(2d-c2c): add DualRunComparator, observation-only, no decision authority"
```

---

## Task 19: Wire comparator + diagnostics writer into `bot.rs`

**Files:**
- Modify: `src/core/bot.rs`
- Test: same file

**Interfaces:**
- `Bot` gains `dual_run_comparator: crate::core::dual_run_comparator::DualRunComparator`.
- `select_opportunities` (existing method) gains one line calling `self.dual_run_comparator.record_legacy(...)` per opportunity found, before returning — non-blocking, in-memory only, no disk I/O on this path (satisfies `C2B_DISK_WRITES_IN_LEGACY_LOOP=0`).
- The `poll_c2b_result` arm in `run()` (Task 17) calls `self.dual_run_comparator.compare(&result.stable_opportunities)` and hands the records to a diagnostics writer.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod dual_run_wiring_tests {
    use super::*;

    #[test]
    fn recording_legacy_snapshot_does_not_touch_disk() {
        // record_legacy is a pure in-memory VecDeque push (Task 18) — this
        // test exists as a marker: if a future change makes it async or
        // fallible, that's a regression against C2B_DISK_WRITES_IN_LEGACY_LOOP=0
        // and should be caught by this test's signature (record_legacy stays
        // `fn`, not `async fn`, and returns `()`, not `Result`).
        let mut comparator = crate::core::dual_run_comparator::DualRunComparator::new(16);
        comparator.record_legacy(crate::core::dual_run_comparator::LegacySnapshot {
            pair: "WETH/USDC".into(), buy_dex: "QuickSwap".into(), sell_dex: "SushiSwap".into(),
            net_profit_usd: 1.0, captured_at_block: 1,
        });
    }
}
```

- [ ] **Step 2: Run test**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib bot::dual_run_wiring_tests -- --nocapture`
Expected: PASS immediately (Task 18 already provides this signature) — this step's value is documenting the invariant, not driving new production code; proceed to wiring.

- [ ] **Step 3: Wire `select_opportunities` and the diagnostics writer**

In `select_opportunities` (the existing method, `src/core/bot.rs` around line 241), immediately before `Ok(opportunities.into_iter().take(top_n).collect())`, add:

```rust
for opp in opportunities.iter().take(top_n) {
    self.dual_run_comparator.record_legacy(crate::core::dual_run_comparator::LegacySnapshot {
        pair: opp.pair.clone(),
        buy_dex: opp.buy_dex.clone(),
        sell_dex: opp.sell_dex.clone(),
        net_profit_usd: opp.net_profit_usd,
        captured_at_block: 0, // legacy path has no pinned block today; 0 is an explicit "unknown", not a guess
    });
}
```

In `run()`'s `poll_c2b_result` arm (Task 17), replace the placeholder comment with:

```rust
let records = self.dual_run_comparator.compare(&result.stable_opportunities);
if let Err(e) = crate::core::dual_run_comparator::write_diagnostics(&records, &result.anchor) {
    warn!(error = %e, "C2B dual-run diagnostics write failed, shadow loop unaffected");
}
```

Add `write_diagnostics` to `src/core/dual_run_comparator.rs`:

```rust
pub fn write_diagnostics(
    records: &[ComparisonRecord],
    anchor: &crate::core::phase2d_anchor::AnchorBlock,
) -> std::io::Result<()> {
    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let jsonl_path = format!("diagnostics/phase2d_c2c_dualrun_{timestamp}.jsonl");
    let md_path = format!("diagnostics/phase2d_c2c_dualrun_{timestamp}.md");
    let mut jsonl = std::fs::File::create(&jsonl_path)?;
    let mut md = std::fs::File::create(&md_path)?;
    use std::io::Write;
    writeln!(md, "# C2B dual-run comparison — anchor {}", anchor.number)?;
    for record in records {
        writeln!(jsonl, "{{\"classification\":\"{:?}\",\"note\":{:?}}}", record.classification, record.note)?;
        writeln!(md, "- `{:?}` — {}", record.classification, record.note)?;
    }
    Ok(())
}
```

This writer runs on the dedicated-runtime side's result-handling in `bot.rs`, not inside `select_opportunities` or any code path `price_rx` processing touches — satisfies "I/O feito no worker/writer dedicado, nunca no hot path." (It's technically invoked from the same `tokio::select!` as the legacy arms, but only inside the `poll_c2b_result` branch, which only ever fires after a shadow round completes on the dedicated runtime — the write itself is fire-and-forget file I/O triggered by that event, not by any `price_rx` tick.)

- [ ] **Step 4: Run tests**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo check --workspace && CARGO_TARGET_DIR=target/phase2d_c2c cargo test --lib bot:: dual_run_comparator:: -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/core/bot.rs src/core/dual_run_comparator.rs
git commit -m "feat(2d-c2c): wire dual-run comparator + diagnostics writer, observation-only"
```

---

## Task 20: Deterministic E2E test

**Files:**
- Create: `tests/phase2d_c2c_e2e.rs`

**Interfaces:** none new — this is a pure consumer of every interface defined in Tasks 1-19.

This integration test cannot call live RPC (repo convention, and the phase's own `MAINNET_WRITE_RPC_CALLS=0`/read-only constraints). Since `discover_at` takes `C2BRoundDeps` wrapping a real `Arc<Provider<Http>>`, and this phase's scope doesn't include building a mock-RPC harness for `discover_at` itself (that's what `phase2d_c2b_fresh_discovery`'s own existing test coverage — read-only registry/E1-F4 tests — already exercises against a real archive RPC in CI/manual runs), this E2E test exercises the **downstream half of the pipeline** — `StableOpportunityAggregator` → `ExecutableOpportunity` → `RiskManager` → strategy selector → `execute_*_canonical` → hard gate → diagnostics — by constructing three synthetic `RoundEvidence` values directly (same technique as Task 3/4's unit tests) and feeding them through `CanonicalC2BOpportunitySource`'s public building blocks, proving the full chain wires together end-to-end without ever going near the network. This matches the spirit of the spec's E2E requirement ("bot scheduler → C2B runtime dedicado → discover_at → 3 RoundEvidence → aggregator → ...") for every stage except the live `discover_at` RPC call itself, which is out of this phase's mocking scope and already covered read-only by the existing bin.

- [ ] **Step 1: Write the test**

```rust
// tests/phase2d_c2c_e2e.rs
use flashloan_bot::core::{
    c2b_orchestrator::OrchestratorEvidence,
    c2b_round::RoundEvidence,
    executable_opportunity::from_stable_rounds,
    executable_route_materializer::{ExecutableLegPlan, ExecutableRoutePlan, ForkSetupPlan},
    executable_call::Venue,
    execution_profile::ExecutionProfile,
    flashloan::{determine_execution_strategy_canonical, ArbitrageClient, CanonicalStrategyDecision},
    fresh_economics::{PinnedStateSnapshot, RouteSimulationResult},
    phase2d_anchor::AnchorBlock,
    risk::{CanonicalRiskConfig, RiskManager},
    stability::StableOpportunityAggregator,
};
use ethers::types::{Address, H256, U256};

fn leg() -> ExecutableLegPlan {
    ExecutableLegPlan {
        venue: Venue::QuickSwap, token_in: Address::repeat_byte(1), token_out: Address::repeat_byte(2),
        pool: Address::repeat_byte(3), router: Address::repeat_byte(4), fee: None,
        curve_method: None, token_in_index: None, token_out_index: None, spender: Address::repeat_byte(4),
    }
}

fn plan(anchor_block: u64) -> ExecutableRoutePlan {
    ExecutableRoutePlan {
        structural_cycle_key: "e2e-key".into(), anchor_block, start_token: Address::repeat_byte(1),
        legs: vec![leg()], snapshot: PinnedStateSnapshot::default(),
        fork_setup: ForkSetupPlan { anchor_block, caller: Address::zero(), tokens: vec![], funding: vec![], approvals: vec![], targets: vec![], balance_checks: vec![] },
    }
}

fn round(number: u64, hash_byte: u8) -> RoundEvidence {
    RoundEvidence {
        structural_cycle_key: "e2e-key".into(),
        anchor: AnchorBlock { number, hash: H256::repeat_byte(hash_byte), selected_from_head: number + 2, confirmation_lag: 2 },
        context_hash: H256::repeat_byte(9),
        route_plan: plan(number),
        amount_in: U256::from(1_000u64),
        execution_profile: ExecutionProfile { chain_id: 137, profile_label: "base".into() },
        economics: Some(RouteSimulationResult {
            final_amount_atomic: U256::from(1_200u64), gross_pnl_atomic: 200,
            gas_cost_atomic: U256::from(10u64), net_pnl_atomic: 190,
            pool_reuse_detected: false, all_models_supported: true,
        }),
        gross_pnl_atomic: Some(200),
        gas_used_total: 21_000,
        orchestrator_evidence: Some(OrchestratorEvidence {
            structural_cycle_key: "e2e-key".into(), economic_positive: true, builder_called: true,
            readonly_pass: true, preflight_pass: true, balance_delta: U256::from(200u64),
            output_propagated: true, trace_validated: true, rejected_registry_hit: false,
            placeholder_evidence: false,
        }),
        rejected_registry_hit: false,
    }
}

#[tokio::test]
async fn three_stable_rounds_flow_through_risk_strategy_and_canonical_execution_with_zero_broadcast() {
    let mut aggregator = StableOpportunityAggregator::new();
    assert!(aggregator.push(round(100, 1)).is_none());
    assert!(aggregator.push(round(103, 2)).is_none());
    let stable = aggregator.push(round(106, 3)).expect("three consistent rounds must emit");

    let opportunity = from_stable_rounds(stable).expect("valid stable rounds must build an ExecutableOpportunity");
    assert_eq!(opportunity.stability.anchor_blocks, [100, 103, 106]);

    let risk_manager = RiskManager::with_defaults();
    let risk_cfg = CanonicalRiskConfig {
        absolute_min_profit_floor_raw: U256::from(1u64), retention_bps: 2_000,
        max_gas_raw: U256::from(1_000_000u64), max_slippage_bps: 100, max_anchor_age_blocks: 32,
    };
    let approval = risk_manager
        .assess_executable_opportunity(&opportunity, &risk_cfg, 106 + 2)
        .expect("positive-PnL, fully-evidenced opportunity must be approved");

    let mut cfg = flashloan_bot::config::Config::default();
    cfg.flashloan.enabled = true;
    cfg.execution.use_flashloan = true;
    cfg.wrapper.enabled = false;
    let decision = determine_execution_strategy_canonical(&opportunity, &approval, &cfg);
    assert_eq!(decision, CanonicalStrategyDecision::Flashloan);

    // Uses whatever test-client constructor flashloan.rs's own unit tests use
    // (see Task 7/8) — imported here via the lib's test-support path if one
    // is exposed, or reconstructed with the same stub middleware pattern.
    let client = flashloan_bot::core::flashloan::test_support::test_client();
    let result = client.execute_flashloan_canonical(&opportunity, &approval).await.unwrap();

    assert_eq!(result.execution_mode.as_deref(), Some("canonical_shadow_dry_run"));
    assert!(result.tx_hash.is_none(), "zero broadcast: canonical shadow must never produce a tx hash");
}
```

`test_support::test_client()` — Tasks 7-9's tests use `super::super::tests::test_client()`, a private helper inside `flashloan.rs`'s `#[cfg(test)]` module, which integration tests under `tests/` cannot reach (they only see the crate's public API). Expose a `pub` test-support constructor for this E2E test: add `#[cfg(any(test, feature = "test-support"))] pub mod test_support { pub fn test_client() -> super::ArbitrageClient { super::tests::test_client() } }` inside `flashloan.rs`, gated the same way, and add `test-support = []` to `Cargo.toml`'s `[features]`. Run the E2E test with `--features test-support`.

- [ ] **Step 2: Run the test to verify it fails first (before the feature gate/module exist)**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --test phase2d_c2c_e2e --features test-support -- --nocapture`
Expected: FAIL — `test_support` not found.

- [ ] **Step 3: Add the `test_support` module and `Cargo.toml` feature, per Step 1's description above**

- [ ] **Step 4: Run the test to verify it passes**

Run: `CARGO_TARGET_DIR=target/phase2d_c2c cargo test --test phase2d_c2c_e2e --features test-support -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add tests/phase2d_c2c_e2e.rs src/core/flashloan.rs Cargo.toml
git commit -m "test(2d-c2c): add deterministic E2E test, aggregator through canonical execution, zero broadcast"
```

---

## Task 21: Latency benchmark harness

**Files:**
- Create: `src/bin/phase2d_c2c_latency_bench.rs`

**Interfaces:** standalone bin, no lib interface — reads `CANONICAL_C2B_SHADOW_ENABLED` from its own config load (same `Config` type), runs the bot's `price_rx`→decision path against a synthetic price-tick generator for a fixed duration, records per-tick latency, and writes `diagnostics/phase2d_c2c_latency_{baseline,experiment}_<timestamp>.json`.

- [ ] **Step 1: Write the bin**

```rust
//! Phase 2D-C2C — legacy-loop latency harness. Feeds synthetic price ticks
//! through Bot::process_prices and records p50/p95/p99 tick-to-decision
//! latency, so a BASELINE run (shadow disabled) and an EXPERIMENT run
//! (shadow enabled) can be diffed for regression per the phase's latency
//! gates. Never sends a transaction — process_prices already stops at
//! opportunity selection in this harness (execution is not invoked).

use anyhow::Result;
use clap::Parser;
use flashloan_bot::{config::Config, core::bot::Bot};
use std::{collections::HashMap, sync::Arc, time::Instant};
use tokio::sync::Mutex;

#[derive(Parser)]
struct Cli {
    #[arg(long)]
    config_path: String,
    #[arg(long, default_value_t = 200)]
    ticks: usize,
    #[arg(long, default_value = "diagnostics/phase2d_c2c_latency_run.json")]
    out: String,
}

fn synthetic_prices(tick: usize) -> HashMap<String, HashMap<String, f64>> {
    let mut dex = HashMap::new();
    dex.insert("WETH/USDC".to_string(), 1800.0 + (tick % 7) as f64 * 0.01);
    let mut prices = HashMap::new();
    prices.insert("QuickSwap".to_string(), dex);
    prices
}

fn percentile(sorted_micros: &[u128], p: f64) -> u128 {
    if sorted_micros.is_empty() { return 0; }
    let idx = ((sorted_micros.len() as f64 - 1.0) * p).round() as usize;
    sorted_micros[idx]
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg_text = std::fs::read_to_string(&cli.config_path)?;
    let cfg: Config = toml::from_str(&cfg_text)?;
    let shadow_enabled = cfg.c2b_shadow.shadow_enabled;
    let config = Arc::new(Mutex::new(cfg));

    // Bot::new requires an AppMiddleware — reuse whatever construction helper
    // main.rs uses for a dry-run/paper client (grep main.rs before writing
    // this section; do not fabricate a live RPC connection here).
    let client = flashloan_bot::build_dryrun_middleware(&config).await?;
    let telegram = Arc::new(flashloan_bot::utils::telegram::TelegramNotifier::disabled());
    let mut bot = Bot::new(client, config, telegram).await;

    let mut latencies_micros = Vec::with_capacity(cli.ticks);
    for tick in 0..cli.ticks {
        let start = Instant::now();
        let _ = bot.select_opportunities(synthetic_prices(tick)).await;
        latencies_micros.push(start.elapsed().as_micros());
    }
    latencies_micros.sort_unstable();

    let report = serde_json::json!({
        "shadow_enabled": shadow_enabled,
        "ticks": cli.ticks,
        "p50_micros": percentile(&latencies_micros, 0.50),
        "p95_micros": percentile(&latencies_micros, 0.95),
        "p99_micros": percentile(&latencies_micros, 0.99),
    });
    std::fs::write(&cli.out, serde_json::to_string_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
```

`flashloan_bot::build_dryrun_middleware` is a placeholder name — before finalizing this step, grep `src/main.rs` for how it constructs an `AppMiddleware`/`Arc<AppMiddleware>` in dry-run/paper mode and call that exact function instead (it likely already exists, given `config.dryrun.toml` is a checked-in fixture this repo clearly already runs against). Do not add a second middleware-construction path if one is reachable from the lib already.

- [ ] **Step 2: Register the bin**

Confirm `Cargo.toml` auto-discovers `src/bin/*.rs` (matching the existing `phase2d_c2b_fresh_discovery`/`phase2d_c_sequential_sim`/`phase2d_d_fork_bytecode` bins, which have no explicit `[[bin]]` entries per the earlier `grep '^\[\[bin\]\]'` finding zero matches) — no `Cargo.toml` change needed unless that grep is re-verified and finds otherwise.

- [ ] **Step 3: Run baseline and experiment**

```bash
CARGO_TARGET_DIR=target/phase2d_c2c cargo run --release --bin phase2d_c2c_latency_bench -- \
  --config-path config/config.dryrun.toml --ticks 500 \
  --out diagnostics/phase2d_c2c_latency_baseline_$(date -u +%Y%m%dT%H%M%SZ).json
```

Then set `shadow_enabled = true` in a copy of `config.dryrun.toml` (e.g. `config/config.dryrun.c2c-experiment.toml`, `dedicated_rpc_url` pointed at a real read-only RPC) and:

```bash
CARGO_TARGET_DIR=target/phase2d_c2c cargo run --release --bin phase2d_c2c_latency_bench -- \
  --config-path config/config.dryrun.c2c-experiment.toml --ticks 500 \
  --out diagnostics/phase2d_c2c_latency_experiment_$(date -u +%Y%m%dT%H%M%SZ).json
```

Expected: both JSON files written; `experiment.p99_micros` vs `baseline.p99_micros` regression `<=5%` (per the phase's `LEGACY_LOOP_P99_REGRESSION_PCT<=5` gate) — since `select_opportunities` never awaits the C2B shadow channels (Task 17's design keeps them on entirely separate `select!` arms/threads), this should already hold; this bin is what proves it numerically rather than by code-reading argument alone.

- [ ] **Step 4: Write the comparison markdown**

```bash
diagnostics/phase2d_c2c_latency_comparison_<timestamp>.md
```
containing the two JSON reports' p50/p95/p99 side by side and the pass/fail verdict against the three regression gates — write this by hand from the two JSON files' contents (a short script or manual transcription; no new code required beyond what Step 1 already produces).

- [ ] **Step 5: Commit**

```bash
git add src/bin/phase2d_c2c_latency_bench.rs
git commit -m "feat(2d-c2c): add legacy-loop latency benchmark harness"
```

(The generated `diagnostics/*.json`/`*.md` reports themselves are run artifacts, not source — commit them separately in Task 23 alongside the rest of the phase's diagnostics output, after real numbers exist.)

---

## Task 22: Smoke operational run

**Files:** none created — this is an operational verification task using binaries from Tasks 17/21.

- [ ] **Step 1: Run the bot with shadow enabled against a real (or forked) RPC for enough blocks to complete >=3 anchors**

```bash
CANONICAL_C2B_SHADOW_ENABLED_OVERRIDE_CONFIG=config/config.dryrun.c2c-experiment.toml \
CARGO_TARGET_DIR=target/phase2d_c2c cargo run --release --bin flashloan-bot -- --config config/config.dryrun.c2c-experiment.toml
```

(Confirm the actual CLI flag/env var `main.rs` uses to select a config path — grep `main.rs`'s `Cli`/`clap` definition before running; the above is illustrative, not to be typed blind.)

Let it run until `C2B shadow round complete` (Task 17's log line) has appeared for 3 distinct `anchor.number` values, then send `SIGINT`/shutdown.

- [ ] **Step 2: Verify the gate values from logs + `diagnostics/phase2d_c2c_dualrun_*` output**

Confirm: `C2B_SHADOW_ROUNDS_COMPLETED>=3`, distinct anchors with `C2B_DUPLICATE_ANCHORS=0`, log lines never show more than one round in flight, `PRODUCTION_SIGNER_LOADED`/`PRODUCTION_BROADCASTER_INITIALIZED` never appear in logs (they're only ever logged from the legacy real-send path, which shadow structurally can't reach per Task 9/10), and no `MAINNET_TRANSACTIONS_SENT`-adjacent log line appears.

- [ ] **Step 3: Write `diagnostics/phase2d_c2c_shadow_<timestamp>.jsonl`**

One line per completed shadow round (anchor, round_evidences count, stable_opportunities count, risk approvals/rejections, strategy decisions, execution results) — extend Task 19's `run()` wiring with one more line in the `poll_c2b_result` arm that appends this record (reuse the same file-write pattern as `write_diagnostics`, a sibling function `write_shadow_round_record` in `dual_run_comparator.rs` or a new small `src/core/c2c_diagnostics.rs` if that file starts feeling overloaded — judgment call at implementation time based on how large `dual_run_comparator.rs` has actually grown by this point).

- [ ] **Step 4: Commit any code changes from Step 3 (the diagnostics artifacts themselves are not committed until Task 23)**

```bash
git add src/core/dual_run_comparator.rs src/core/bot.rs
git commit -m "feat(2d-c2c): record per-round shadow diagnostics jsonl"
```

---

## Task 23: Final validation, gates report, commit

**Files:**
- Create: `diagnostics/phase2d_c2c_report_<timestamp>.md`
- Create: `diagnostics/phase2d_c2c_gates_<timestamp>.txt`
- (Diagnostics `.jsonl`/`.json` artifacts from Tasks 19/21/22 land here too, if not already committed.)

- [ ] **Step 1: Run full validation suite**

```bash
export CARGO_TARGET_DIR=target/phase2d_c2c
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace -- --test-threads=1
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tee diagnostics/phase2d_c2c_clippy_<timestamp>.txt
```

Compare the clippy output's new-warning count against a clippy run on the pre-phase commit (`git stash` or a throwaway worktree checkout of the branch's base commit, same command) to compute `CLIPPY_NEW_ERRORS_INTRODUCED` — must be `0`. Any pre-existing warnings this phase's changed files happen to touch but didn't introduce go in a separate "legacy debt, not introduced by 2D-C2C" note in the report, per the spec's instruction to track that debt separately.

- [ ] **Step 2: Fill in `diagnostics/phase2d_c2c_gates_<timestamp>.txt`**

Copy the "Gates finais" block from `docs/superpowers/specs/2026-07-31-2d-c2c-shadow-integration-design.md`, filling every `=` field with the actual observed value from Steps 1, Task 20's E2E test result, and Task 22's smoke run.

- [ ] **Step 3: Write `diagnostics/phase2d_c2c_report_<timestamp>.md`**

Summarize: what was built (reference this plan's task list), the smoke run's observed counts (Task 22), the latency comparison verdict (Task 21), and the dual-run comparator's observed classifications (Task 22 Step 2/Task 19).

- [ ] **Step 4: Commit everything**

```bash
git add diagnostics/phase2d_c2c_*
git commit -m "$(cat <<'EOF'
feat(2d-c2c): integrate canonical shadow pipeline into bot loop

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

Per the spec: no push, no tag, no enabling live execution. `CANONICAL_C2B_SHADOW_ENABLED` stays `false` in the checked-in `config/config.dryrun.toml` (Task 13) — only the throwaway `config.dryrun.c2c-experiment.toml` used for Tasks 21/22 has it `true`, and that file is either gitignored or explicitly not committed.

---

## Self-Review Notes

- **Spec coverage:** every numbered spec section (1-16) maps to at least one task — isolation/dedicated runtime (14, 16), scheduler (15, 17), pipeline extraction (11, 12), aggregator (3), `ExecutableOpportunity` (4), risk (5), strategy (6), executor (7-10), bot.rs wiring (17), comparator (18-19), tests (embedded throughout + 20), latency (21), validations (23), smoke (22), entregáveis (19, 21, 22, 23).
- **Type consistency check performed:** `RoundEvidence` (Task 2) is consumed identically by `stability.rs` (Task 3), `executable_opportunity.rs` (Task 4), and `discover_at` (Task 11) — field names verified matching across all three. `RiskApproval`/`CanonicalRiskRejection` (Task 5) are consumed identically by Task 6, 9, 14. `ExecutionMode`/`CanonicalExecutorError` (Task 7) feed Task 8/9 unchanged.
- **Known follow-up flagged, not silently dropped:** `LegQuote.amount_in`/`amount_out` zero-filled in Task 4 pending Task 11 wiring real per-leg quotes into `RoundEvidence` — Task 11's `discover_at` extraction should be revisited to also populate `RoundEvidence.leg_quotes` from the `PinnedQuoteRecord`s already computed mid-pipeline (`leg_quotes: Vec<PinnedQuoteRecord>` local already exists in the copied body) and thread that through to `ExecutableOpportunity.leg_quotes` in Task 4's constructor — call this out explicitly to whoever executes Task 11 so it isn't missed.
- **Known scope boundary:** `DualRunComparator::compare` (Task 18) only classifies `CanonicalOnly`/`LegacyOnly` in this phase — the five richer classifications exist in the enum but real legacy/canonical pair-correlation (matching by resolved token address, not symbol) is flagged as needing legacy-side symbol→address resolution not otherwise required by this phase; do not let this silently regress `MATERIAL_DIVERGENCES`/`UNEXPLAINED_DIVERGENCES` gate reporting — the phase's completion report (Task 23) must state this limitation explicitly rather than imply full divergence coverage.
