//! Phase 2D-D call-trace validation. Pure logic — takes an already-fetched
//! `debug_traceTransaction` (`callTracer`) JSON tree and an allowlist; it
//! never calls RPC itself.
//!
//! Walks the call tree looking for anything a two-leg USDC->USDT->USDC swap
//! has no legitimate reason to do: a call to an address outside the
//! allowlist, a non-zero native-value transfer, a `SELFDESTRUCT`, or a
//! `DELEGATECALL` to something that isn't a known proxy implementation.
//! Uniswap V3's swap callback (the pool calling back into the router/caller
//! mid-swap to pull payment) is recognized by selector and never flagged.

use ethers::types::Address;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;

/// `uniswapV3SwapCallback(int256,int256,bytes)` selector — the pool calls
/// this back into the caller mid-swap; it is not an "unexpected call".
pub const UNISWAP_V3_SWAP_CALLBACK_SELECTOR: [u8; 4] = [0xfa, 0x46, 0x1e, 0x33];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum AnomalyKind {
    UnexpectedCallTarget,
    UnexpectedNativeValueTransfer,
    SelfDestruct,
    UnexpectedDelegateTarget,
    InternalRevert,
}

#[derive(Debug, Clone, Serialize)]
pub struct TraceAnomaly {
    pub path: String,
    pub kind: AnomalyKind,
    pub detail: String,
}

fn parse_address(v: &Value) -> Option<Address> {
    v.as_str()?.parse().ok()
}

fn parse_value(v: &Value) -> u128 {
    v.as_str()
        .and_then(|s| u128::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0)
}

fn selector_of(input_hex: &str) -> Option<[u8; 4]> {
    let hex = input_hex.trim_start_matches("0x");
    if hex.len() < 8 {
        return None;
    }
    let mut out = [0u8; 4];
    for i in 0..4 {
        out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Recursively validates one call-tree frame and its children, appending any
/// anomalies found. `path` is a `.`-joined index path for auditability
/// (e.g. `"0.1.0"` = first tx's second subcall's first subcall).
fn walk(
    frame: &Value,
    path: String,
    allowlist: &HashSet<Address>,
    known_callback_selectors: &HashSet<[u8; 4]>,
    anomalies: &mut Vec<TraceAnomaly>,
) {
    let call_type = frame.get("type").and_then(Value::as_str).unwrap_or("");
    let to = frame.get("to").and_then(parse_address);
    let value = frame.get("value").map(parse_value).unwrap_or(0);
    let input = frame.get("input").and_then(Value::as_str).unwrap_or("0x");
    let error = frame.get("error").and_then(Value::as_str);

    if call_type.eq_ignore_ascii_case("SELFDESTRUCT") {
        anomalies.push(TraceAnomaly {
            path: path.clone(),
            kind: AnomalyKind::SelfDestruct,
            detail: format!("SELFDESTRUCT at {path}"),
        });
    }

    if value > 0 {
        anomalies.push(TraceAnomaly {
            path: path.clone(),
            kind: AnomalyKind::UnexpectedNativeValueTransfer,
            detail: format!(
                "native value={value} transferred at {path} (expected 0 — ERC-20 only route)"
            ),
        });
    }

    if let Some(to_addr) = to {
        let is_allowlisted = allowlist.contains(&to_addr);
        let is_recognized_callback = selector_of(input)
            .map(|sel| known_callback_selectors.contains(&sel))
            .unwrap_or(false);
        if !is_allowlisted && !is_recognized_callback {
            let kind = if call_type.eq_ignore_ascii_case("DELEGATECALL") {
                AnomalyKind::UnexpectedDelegateTarget
            } else {
                AnomalyKind::UnexpectedCallTarget
            };
            anomalies.push(TraceAnomaly {
                path: path.clone(),
                kind,
                detail: format!("{call_type} to non-allowlisted {to_addr:#x} at {path}"),
            });
        }
    }

    if let Some(err) = error {
        anomalies.push(TraceAnomaly {
            path: path.clone(),
            kind: AnomalyKind::InternalRevert,
            detail: format!("internal call reverted at {path}: {err}"),
        });
    }

    if let Some(calls) = frame.get("calls").and_then(Value::as_array) {
        for (i, child) in calls.iter().enumerate() {
            walk(
                child,
                format!("{path}.{i}"),
                allowlist,
                known_callback_selectors,
                anomalies,
            );
        }
    }
}

/// Validates a full `callTracer`-shaped trace against an allowlist of
/// expected contract addresses (tokens, router, pool). Returns every
/// anomaly found — an empty vec means the trace contained nothing outside
/// the expected call surface.
pub fn validate_trace(
    trace: &Value,
    allowlist: &HashSet<Address>,
    known_callback_selectors: &HashSet<[u8; 4]>,
) -> Vec<TraceAnomaly> {
    let mut anomalies = Vec::new();
    walk(
        trace,
        "0".to_string(),
        allowlist,
        known_callback_selectors,
        &mut anomalies,
    );
    anomalies
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn addr(n: u64) -> Address {
        Address::from_low_u64_be(n)
    }

    #[test]
    fn clean_two_hop_trace_has_no_anomalies() {
        let router = addr(0xE592_4270);
        let pool = addr(0xcafe);
        let allowlist: HashSet<Address> = [router, pool].into_iter().collect();
        let trace = json!({
            "type": "CALL", "to": format!("{router:#x}"), "value": "0x0", "input": "0x414bf389",
            "calls": [
                { "type": "CALL", "to": format!("{pool:#x}"), "value": "0x0", "input": "0x128acb08" }
            ]
        });
        let anomalies = validate_trace(&trace, &allowlist, &HashSet::new());
        assert!(anomalies.is_empty(), "{anomalies:?}");
    }

    #[test]
    fn uniswap_v3_callback_to_non_allowlisted_caller_is_not_flagged() {
        let pool = addr(0xcafe);
        let caller = addr(0xdead);
        let allowlist: HashSet<Address> = [pool].into_iter().collect();
        let mut callbacks = HashSet::new();
        callbacks.insert(UNISWAP_V3_SWAP_CALLBACK_SELECTOR);
        let trace = json!({
            "type": "CALL", "to": format!("{pool:#x}"), "value": "0x0", "input": "0x128acb08",
            "calls": [
                { "type": "CALL", "to": format!("{caller:#x}"), "value": "0x0", "input": "0xfa461e33" }
            ]
        });
        let anomalies = validate_trace(&trace, &allowlist, &callbacks);
        assert!(anomalies.is_empty(), "{anomalies:?}");
    }

    #[test]
    fn call_to_unlisted_address_is_flagged() {
        let allowlist: HashSet<Address> = HashSet::new();
        let evil = addr(0xebad1);
        let trace = json!({ "type": "CALL", "to": format!("{evil:#x}"), "value": "0x0", "input": "0x12345678" });
        let anomalies = validate_trace(&trace, &allowlist, &HashSet::new());
        assert_eq!(anomalies.len(), 1);
        assert_eq!(anomalies[0].kind, AnomalyKind::UnexpectedCallTarget);
    }

    #[test]
    fn nonzero_native_value_is_flagged_even_to_allowlisted_target() {
        let router = addr(0xE592_4270);
        let allowlist: HashSet<Address> = [router].into_iter().collect();
        let trace =
            json!({ "type": "CALL", "to": format!("{router:#x}"), "value": "0x1", "input": "0x" });
        let anomalies = validate_trace(&trace, &allowlist, &HashSet::new());
        assert!(anomalies
            .iter()
            .any(|a| a.kind == AnomalyKind::UnexpectedNativeValueTransfer));
    }

    #[test]
    fn selfdestruct_is_flagged() {
        let trace = json!({ "type": "SELFDESTRUCT", "to": null, "value": "0x0" });
        let anomalies = validate_trace(&trace, &HashSet::new(), &HashSet::new());
        assert!(anomalies
            .iter()
            .any(|a| a.kind == AnomalyKind::SelfDestruct));
    }

    #[test]
    fn delegatecall_to_unlisted_implementation_is_flagged_as_delegate_target() {
        let evil = addr(0xebad1);
        let trace = json!({ "type": "DELEGATECALL", "to": format!("{evil:#x}"), "value": "0x0", "input": "0x" });
        let anomalies = validate_trace(&trace, &HashSet::new(), &HashSet::new());
        assert_eq!(anomalies.len(), 1);
        assert_eq!(anomalies[0].kind, AnomalyKind::UnexpectedDelegateTarget);
    }

    #[test]
    fn delegatecall_to_allowlisted_implementation_is_not_flagged() {
        let proxy_impl = addr(0x1234);
        let allowlist: HashSet<Address> = [proxy_impl].into_iter().collect();
        let trace = json!({ "type": "DELEGATECALL", "to": format!("{proxy_impl:#x}"), "value": "0x0", "input": "0x" });
        let anomalies = validate_trace(&trace, &allowlist, &HashSet::new());
        assert!(anomalies.is_empty());
    }

    #[test]
    fn internal_revert_is_flagged() {
        let trace = json!({ "type": "CALL", "to": null, "value": "0x0", "input": "0x", "error": "execution reverted" });
        let anomalies = validate_trace(&trace, &HashSet::new(), &HashSet::new());
        assert!(anomalies
            .iter()
            .any(|a| a.kind == AnomalyKind::InternalRevert));
    }

    #[test]
    fn nested_anomaly_is_found_deep_in_the_tree() {
        let evil = addr(0xebad1);
        let trace = json!({
            "type": "CALL", "to": null, "value": "0x0", "input": "0x",
            "calls": [{
                "type": "CALL", "to": null, "value": "0x0", "input": "0x",
                "calls": [{ "type": "CALL", "to": format!("{evil:#x}"), "value": "0x0", "input": "0x" }]
            }]
        });
        let anomalies = validate_trace(&trace, &HashSet::new(), &HashSet::new());
        assert_eq!(anomalies.len(), 1);
        assert_eq!(anomalies[0].path, "0.0.0");
    }
}
