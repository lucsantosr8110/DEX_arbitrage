//! Cross-model helpers for replay scan: CPMM vs Curve StableSwap.
//!
//! Curve **não** tem `DexType` no `FlashloanExecutor` — ciclos com perna Curve
//! são quote-only (sem eth_call). Não altera o finder.

use anyhow::{anyhow, bail, Result};
use serde::Serialize;

/// Modelo de curva por venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CurveModel {
    Cpmm,
    StableSwap,
}

impl CurveModel {
    pub fn as_str(self) -> &'static str {
        match self {
            CurveModel::Cpmm => "Cpmm",
            CurveModel::StableSwap => "StableSwap",
        }
    }
}

/// Stables alvo (Polygon): USDT0 ≡ USDT nativo `0xc213…`.
pub const STABLE_SYMBOLS: &[&str] = &["USDC", "USDT", "DAI"];

/// Decimals canônicos — abort se cache divergir.
pub fn expected_stable_decimals(symbol: &str) -> Option<u8> {
    match symbol.to_ascii_uppercase().as_str() {
        "USDC" | "USDT" | "USDT0" | "USDC.E" | "USDT.E" => Some(6),
        "DAI" => Some(18),
        _ => None,
    }
}

pub fn validate_stable_decimals(symbol: &str, decimals: u8) -> Result<()> {
    let Some(exp) = expected_stable_decimals(symbol) else {
        bail!("stable desconhecido: {symbol}");
    };
    if decimals != exp {
        bail!("decimals abort: {symbol} got={decimals} expected={exp}");
    }
    Ok(())
}

pub fn venue_curve_model(dex: &str) -> CurveModel {
    let n = dex.to_ascii_lowercase().replace(' ', "").replace('_', "");
    if n.contains("curve") {
        CurveModel::StableSwap
    } else {
        CurveModel::Cpmm
    }
}

/// `true` se o ciclo tem ≥1 StableSwap e ≥1 Cpmm.
pub fn cycle_is_cross_model<'a, I>(venues: I) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    let mut has_ss = false;
    let mut has_cpmm = false;
    for v in venues {
        match venue_curve_model(v) {
            CurveModel::StableSwap => has_ss = true,
            CurveModel::Cpmm => has_cpmm = true,
        }
    }
    has_ss && has_cpmm
}

/// FlashloanExecutor: `DexType { QUICKSWAP, SUSHISWAP, UNISWAP_V3 }` — sem Curve.
pub fn curve_executor_supported() -> bool {
    false
}

/// Perna executável pelo contrato on-chain (sem Curve).
pub fn leg_executor_supported(dex: &str) -> bool {
    match venue_curve_model(dex) {
        CurveModel::StableSwap => curve_executor_supported(),
        CurveModel::Cpmm => {
            let n = dex
                .to_ascii_lowercase()
                .replace(' ', "")
                .replace('_', "")
                .replace("v2", "")
                .replace("v3", "");
            matches!(n.as_str(), "quickswap" | "sushiswap" | "uniswap")
        }
    }
}

pub fn route_all_legs_executable<'a, I>(venues: I) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    venues.into_iter().all(leg_executor_supported)
}

/// eth_call só com edge, sob cap, e rota 100% executável.
pub fn should_eth_call_for_route(
    edge: bool,
    eth_calls_so_far: u64,
    max_eth_calls: u64,
    all_legs_executable: bool,
) -> bool {
    edge && eth_calls_so_far < max_eth_calls && all_legs_executable
}

/// Fee floor teórico por venue (Curve StableSwap = 4 bps).
pub fn venue_fee_fraction(dex: &str, v3_fee_tier: Option<u32>) -> f64 {
    match venue_curve_model(dex) {
        CurveModel::StableSwap => 0.0004,
        CurveModel::Cpmm => {
            if dex.to_ascii_lowercase().contains("uniswapv3") {
                v3_fee_tier.unwrap_or(3000) as f64 / 1_000_000.0
            } else {
                0.003
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuotedLeg {
    pub dex: String,
    pub token_in: String,
    pub token_out: String,
    pub rate: f64,
    pub v3_fee_tier: Option<u32>,
}

impl QuotedLeg {
    pub fn model(&self) -> CurveModel {
        venue_curve_model(&self.dex)
    }
}

#[derive(Debug, Clone)]
pub struct CrossModelCycle {
    pub path: Vec<String>,
    pub legs: Vec<QuotedLeg>,
    pub cycle_rate: f64,
    pub cross_model: bool,
    pub fee_floor: f64,
}

impl CrossModelCycle {
    pub fn pair_label(&self) -> String {
        self.path.join("->")
    }

    pub fn route_label(&self) -> String {
        self.legs
            .iter()
            .map(|l| {
                format!(
                    "{}[{}]:{}→{}",
                    l.dex,
                    l.model().as_str(),
                    l.token_in,
                    l.token_out
                )
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    pub fn fee_tiers_label(&self) -> String {
        self.legs
            .iter()
            .map(|l| {
                l.v3_fee_tier
                    .map(|f| f.to_string())
                    .unwrap_or_else(|| {
                        if l.model() == CurveModel::StableSwap {
                            "4bps".into()
                        } else {
                            "-".into()
                        }
                    })
            })
            .collect::<Vec<_>>()
            .join(";")
    }

    pub fn venues(&self) -> Vec<&str> {
        self.legs.iter().map(|l| l.dex.as_str()).collect()
    }
}

/// Quotes indexados por (dex, token_in, token_out) → (rate, optional v3 fee).
pub type QuoteIndex = std::collections::HashMap<(String, String, String), (f64, Option<u32>)>;

/// Melhor ciclo A→B→C→A exigindo `cross_model` (ou same_model se `require_cross=false`).
pub fn find_best_stable_cycle(
    quotes: &QuoteIndex,
    require_cross: bool,
) -> Option<CrossModelCycle> {
    let stables = ["USDC", "USDT", "DAI"];
    let mut best: Option<CrossModelCycle> = None;

    for &a in &stables {
        for &b in &stables {
            if a == b {
                continue;
            }
            for &c in &stables {
                if c == a || c == b {
                    continue;
                }
                // hops: a→b, b→c, c→a
                let hop_pairs = [(a, b), (b, c), (c, a)];
                let mut candidates: Vec<Vec<QuotedLeg>> = vec![Vec::new()];

                for &(tin, tout) in &hop_pairs {
                    let mut next = Vec::new();
                    let mut found_any = false;
                    for ((dex, qi, qo), (rate, fee)) in quotes {
                        if !qi.eq_ignore_ascii_case(tin) || !qo.eq_ignore_ascii_case(tout) {
                            continue;
                        }
                        if !rate.is_finite() || *rate <= 0.0 {
                            continue;
                        }
                        found_any = true;
                        for prefix in &candidates {
                            let mut legs = prefix.clone();
                            legs.push(QuotedLeg {
                                dex: dex.clone(),
                                token_in: tin.to_string(),
                                token_out: tout.to_string(),
                                rate: *rate,
                                v3_fee_tier: *fee,
                            });
                            next.push(legs);
                        }
                    }
                    if !found_any {
                        candidates.clear();
                        break;
                    }
                    // Cap branching: keep only best rate per (dex) for this hop
                    // already expanded; prune to top venues by rate
                    next.sort_by(|x, y| {
                        let rx: f64 = x.iter().map(|l| l.rate).product();
                        let ry: f64 = y.iter().map(|l| l.rate).product();
                        ry.partial_cmp(&rx).unwrap_or(std::cmp::Ordering::Equal)
                    });
                    next.truncate(64);
                    candidates = next;
                }

                for legs in candidates {
                    if legs.len() != 3 {
                        continue;
                    }
                    let venues: Vec<&str> = legs.iter().map(|l| l.dex.as_str()).collect();
                    let cross = cycle_is_cross_model(venues.iter().copied());
                    if require_cross && !cross {
                        continue;
                    }
                    if !require_cross && cross {
                        continue;
                    }
                    let cycle_rate: f64 = legs.iter().map(|l| l.rate).product();
                    if !cycle_rate.is_finite() || cycle_rate <= 0.0 {
                        continue;
                    }
                    let fee_floor: f64 = legs
                        .iter()
                        .map(|l| 1.0 - venue_fee_fraction(&l.dex, l.v3_fee_tier))
                        .product();
                    let cyc = CrossModelCycle {
                        path: vec![a.into(), b.into(), c.into(), a.into()],
                        legs,
                        cycle_rate,
                        cross_model: cross,
                        fee_floor,
                    };
                    if best
                        .as_ref()
                        .map(|b| cyc.cycle_rate > b.cycle_rate)
                        .unwrap_or(true)
                    {
                        best = Some(cyc);
                    }
                }
            }
        }
    }
    best
}

/// Documentação / diagnóstico: Curve executável?
pub fn curve_execution_diagnostic() -> &'static str {
    "FlashloanExecutor.DexType = {QUICKSWAP, SUSHISWAP, UNISWAP_V3}; \
     _executeSingleSwap sem branch Curve → revert Unsupported DEX. \
     Cross-model = QUOTE-ONLY (sem eth_call)."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_model_flag_mixed_venues() {
        assert!(cycle_is_cross_model(["Curve", "UniswapV3", "QuickSwap"]));
        assert!(!cycle_is_cross_model(["UniswapV3", "QuickSwap", "SushiSwap"]));
        assert!(!cycle_is_cross_model(["Curve", "Curve", "Curve"]));
    }

    #[test]
    fn curve_not_executable_quote_only() {
        assert!(!curve_executor_supported());
        assert!(!leg_executor_supported("Curve"));
        assert!(leg_executor_supported("UniswapV3"));
        assert!(!should_eth_call_for_route(true, 0, 40, false));
        assert!(should_eth_call_for_route(true, 0, 40, true));
        assert!(!should_eth_call_for_route(false, 0, 40, true));
        // cross-model route with Curve never eth_calls
        assert!(!route_all_legs_executable(["Curve", "UniswapV3", "QuickSwap"]));
        assert!(route_all_legs_executable(["UniswapV3", "QuickSwap", "SushiSwap"]));
    }

    #[test]
    fn stable_decimals_canonical() {
        assert_eq!(expected_stable_decimals("USDC"), Some(6));
        assert_eq!(expected_stable_decimals("USDT"), Some(6));
        assert_eq!(expected_stable_decimals("USDT0"), Some(6));
        assert_eq!(expected_stable_decimals("DAI"), Some(18));
        assert!(validate_stable_decimals("USDC", 6).is_ok());
        assert!(validate_stable_decimals("DAI", 18).is_ok());
        assert!(validate_stable_decimals("USDC", 18).is_err());
        assert!(validate_stable_decimals("DAI", 6).is_err());
    }

    #[test]
    fn find_cross_model_cycle_from_quotes() {
        let mut q = QuoteIndex::new();
        // Force mispricing: Curve USDC→USDT high, CPMM elsewhere
        q.insert(("Curve".into(), "USDC".into(), "USDT".into()), (1.002, None));
        q.insert(("UniswapV3".into(), "USDT".into(), "DAI".into()), (1.0, Some(100)));
        q.insert(("QuickSwap".into(), "DAI".into(), "USDC".into()), (1.0, None));
        // also CPMM-only alternatives below 1
        q.insert(("UniswapV3".into(), "USDC".into(), "USDT".into()), (0.999, Some(100)));
        q.insert(("Curve".into(), "USDT".into(), "DAI".into()), (0.999, None));
        q.insert(("Curve".into(), "DAI".into(), "USDC".into()), (0.999, None));

        let cross = find_best_stable_cycle(&q, true).expect("cross");
        assert!(cross.cross_model);
        assert!(cross.cycle_rate > 1.0);
        assert!(cross.venues().iter().any(|v| venue_curve_model(v) == CurveModel::StableSwap));
        assert!(cross.venues().iter().any(|v| venue_curve_model(v) == CurveModel::Cpmm));

        let same = find_best_stable_cycle(&q, false);
        if let Some(s) = same {
            assert!(!s.cross_model);
        }
    }
}
