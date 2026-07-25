//! Fonte única do modelo de custo de uma rota de arbitragem.
//!
//! # O que já está no `total_rate` (e portanto NÃO é custo separado)
//!
//! Os rates de cada perna vêm de `getAmountsOut` (V2), `quoteExactInputSingle`
//! (V3) e `get_dy` (Curve), sempre cotados no **notional de trade** via
//! [`crate::dex::quote_amount_for_usd`]. Um quote de AMM devolve o output já
//! líquido de:
//!
//! - **fee do pool** (0.3% V2, tier/1e6 no V3, 4 bps Curve);
//! - **price impact** naquele tamanho de ordem.
//!
//! Logo `total_rate = Π step.expected_rate` é fee-inclusive **e** impact-inclusive.
//! Deduzir fee ou price impact de novo é dedução dupla — o mesmo erro que
//! `radar::extract_edges` e `expected_output_from_fee_inclusive_rate` já evitam.
//!
//! # O que é custo de verdade
//!
//! - **gás**: pago em POL, fora da curva do AMM.
//! - **prêmio do flashloan**: 5 bps na Aave V3 (`FLASHLOAN_PREMIUM_TOTAL`).
//! - **adverse move** (opcional, default 0): drift entre o bloco do quote e o
//!   bloco de execução. NÃO é price impact — é risco de latência/competição, e
//!   só entra se o operador optar explicitamente por `adverse_move_bps > 0`.
//!
//! Todos os gates de lucro (finder, executor, risk manager) deduzem a partir do
//! GROSS usando este módulo — convenção A5, uma única dedução por custo.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

/// `FLASHLOAN_PREMIUM_TOTAL` da Aave V3 (5 bps). Verificado on-chain 2026-07-24.
pub const AAVE_V3_PREMIUM_PCT: f64 = 0.0005;

/// Última estimativa VIVA de gás (micro-dólares; 0 = nunca publicada).
///
/// Escrita por [`crate::core::gas::GasEstimator::estimate_arbitrage_gas_usd`],
/// lida pelo finder e pelo risk manager. Existe para que os três gates de lucro
/// usem **o mesmo** número de gás em vez de três modelos divergentes (o finder
/// usava um estático de config, o risk manager saturava num teto fixo de $0.10).
static LAST_LIVE_GAS_USD_MICROS: AtomicU64 = AtomicU64::new(0);

/// Publica a estimativa viva de gás. Valores não-finitos/negativos são ignorados.
pub fn publish_live_gas_usd(usd: f64) {
    if !usd.is_finite() || usd < 0.0 {
        return;
    }
    let micros = (usd * 1e6).round();
    if micros < 1.0 || micros >= u64::MAX as f64 {
        return;
    }
    LAST_LIVE_GAS_USD_MICROS.store(micros as u64, Ordering::Relaxed);
}

/// Estimativa viva de gás, se já houve alguma.
pub fn live_gas_usd() -> Option<f64> {
    match LAST_LIVE_GAS_USD_MICROS.load(Ordering::Relaxed) {
        0 => None,
        m => Some(m as f64 / 1e6),
    }
}

/// Gás a usar num gate de lucro: preferir a medição viva, cair no estático.
pub fn gas_usd_or_fallback(fallback_usd: f64) -> f64 {
    live_gas_usd().unwrap_or_else(|| sane(fallback_usd))
}

/// Custos de uma rota, em USD. Cada campo aparece **uma** vez no net.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct TradeCosts {
    pub gas_usd: f64,
    pub flashloan_fee_usd: f64,
    pub adverse_move_usd: f64,
}

impl TradeCosts {
    pub fn total_usd(&self) -> f64 {
        self.gas_usd + self.flashloan_fee_usd + self.adverse_move_usd
    }
}

fn sane(v: f64) -> f64 {
    if v.is_finite() && v > 0.0 {
        v
    } else {
        0.0
    }
}

/// Lucro bruto = notional × (rate − 1). `total_rate` já é fee+impact-inclusive.
pub fn gross_profit_usd(trade_amount_usd: f64, total_rate: f64) -> f64 {
    if !trade_amount_usd.is_finite() || trade_amount_usd <= 0.0 || !total_rate.is_finite() {
        return 0.0;
    }
    trade_amount_usd * (total_rate - 1.0)
}

/// Prêmio do flashloan sobre o principal emprestado.
pub fn flashloan_fee_usd(trade_amount_usd: f64, fee_pct: f64) -> f64 {
    sane(trade_amount_usd) * sane(fee_pct)
}

/// Buffer opcional de drift quote→execução. **Não** é price impact.
pub fn adverse_move_usd(trade_amount_usd: f64, adverse_move_bps: u32) -> f64 {
    sane(trade_amount_usd) * (adverse_move_bps as f64 / 10_000.0)
}

/// Net = gross − custos. Única fórmula de net do projeto.
pub fn net_profit_usd(gross_profit_usd: f64, costs: &TradeCosts) -> f64 {
    if !gross_profit_usd.is_finite() {
        return f64::NEG_INFINITY;
    }
    gross_profit_usd - costs.total_usd()
}

/// Piso de tolerância por perna. Abaixo disto qualquer ruído normal de bloco
/// reverte a tx sem motivo.
pub const MIN_SLIPPAGE_BPS: u32 = 5;

/// Tolerância máxima de slippage POR PERNA que ainda deixa a rota no lucro.
///
/// Uma rota com edge de 20 bps não pode tolerar 250 bps de haircut: se o
/// mercado andar tudo isso, a tx **completa com prejuízo** em vez de reverter.
/// `amount_out_min` frouxo não é proteção — é o orçamento que um sandwich pode
/// extrair de você. O teto é o próprio lucro líquido, dividido entre as pernas.
///
/// Devolve valor em bps, já limitado a `[MIN_SLIPPAGE_BPS, ceiling_bps]`.
pub fn max_slippage_bps_for_edge(
    net_profit_usd: f64,
    trade_amount_usd: f64,
    n_hops: usize,
    ceiling_bps: u32,
) -> u32 {
    let hops = n_hops.max(1) as f64;
    if !net_profit_usd.is_finite() || net_profit_usd <= 0.0 || trade_amount_usd <= 0.0 {
        return MIN_SLIPPAGE_BPS.min(ceiling_bps.max(MIN_SLIPPAGE_BPS));
    }
    let budget_bps = (net_profit_usd / trade_amount_usd) * 10_000.0;
    let per_hop = (budget_bps / hops).floor();
    if !per_hop.is_finite() || per_hop <= 0.0 {
        return MIN_SLIPPAGE_BPS.min(ceiling_bps.max(MIN_SLIPPAGE_BPS));
    }
    (per_hop as u32)
        .clamp(MIN_SLIPPAGE_BPS, ceiling_bps.max(MIN_SLIPPAGE_BPS))
}

/// Delta esperado de um `eth_call` de simulação.
///
/// `eth_call` não cobra gás, mas o prêmio do flashloan É pago dentro da tx
/// (a Aave exige principal+premium no repay). Este é o número comparável com
/// `profit_realizado_usd` — usar `net_profit_usd` no lugar dele injeta o custo
/// de gás (e qualquer adverse move) como erro de previsão fantasma.
pub fn expected_sim_delta_usd(gross_profit_usd: f64, flashloan_fee_usd: f64) -> f64 {
    if !gross_profit_usd.is_finite() {
        return 0.0;
    }
    gross_profit_usd - sane(flashloan_fee_usd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_deducts_each_cost_exactly_once() {
        let costs = TradeCosts {
            gas_usd: 0.005,
            flashloan_fee_usd: 0.05,
            adverse_move_usd: 0.0,
        };
        assert!((costs.total_usd() - 0.055).abs() < 1e-12);
        let net = net_profit_usd(0.30, &costs);
        assert!((net - 0.245).abs() < 1e-12);
    }

    #[test]
    fn price_impact_is_not_a_cost_term() {
        // Rate fee+impact-inclusive de 1.003 em $100 => gross $0.30.
        let gross = gross_profit_usd(100.0, 1.003);
        assert!((gross - 0.3).abs() < 1e-9);
        // Custo real: gás + prêmio. Nada de 25/50 bps de "price impact".
        let costs = TradeCosts {
            gas_usd: 0.005,
            flashloan_fee_usd: flashloan_fee_usd(100.0, AAVE_V3_PREMIUM_PCT),
            adverse_move_usd: adverse_move_usd(100.0, 0),
        };
        assert!((costs.flashloan_fee_usd - 0.05).abs() < 1e-12);
        assert_eq!(costs.adverse_move_usd, 0.0);
        // Hurdle total = 5.5 bps, não 30.9 bps.
        assert!((costs.total_usd() - 0.055).abs() < 1e-12);
        assert!(net_profit_usd(gross, &costs) > 0.0);
    }

    #[test]
    fn adverse_move_is_opt_in() {
        assert_eq!(adverse_move_usd(100.0, 0), 0.0);
        assert!((adverse_move_usd(100.0, 25) - 0.25).abs() < 1e-12);
    }

    #[test]
    fn sim_delta_excludes_gas_includes_premium() {
        let gross = 0.30;
        let fl = 0.05;
        let delta = expected_sim_delta_usd(gross, fl);
        assert!((delta - 0.25).abs() < 1e-12);
        // net (com gás) é menor que o delta esperado do eth_call
        let net = net_profit_usd(
            gross,
            &TradeCosts {
                gas_usd: 0.005,
                flashloan_fee_usd: fl,
                adverse_move_usd: 0.0,
            },
        );
        assert!(net < delta);
    }

    #[test]
    fn live_gas_publish_and_fallback() {
        // Sem publicação válida, cai no estático.
        publish_live_gas_usd(f64::NAN);
        publish_live_gas_usd(-1.0);
        publish_live_gas_usd(0.0); // < 1 micro-dólar => ignorado
        // (outros testes podem já ter publicado; só exigimos o contrato do fallback)
        assert!((gas_usd_or_fallback(0.0092) - live_gas_usd().unwrap_or(0.0092)).abs() < 1e-9);

        publish_live_gas_usd(0.0051);
        assert_eq!(live_gas_usd(), Some(0.0051));
        // Viva vence o estático inflado.
        assert!((gas_usd_or_fallback(0.10) - 0.0051).abs() < 1e-9);
    }

    #[test]
    fn slippage_budget_never_exceeds_the_edge() {
        // Edge de 20 bps em $100 => net $0.20, 3 hops => ~6 bps/hop.
        let bps = max_slippage_bps_for_edge(0.20, 100.0, 3, 50);
        assert!(bps <= 7, "esperava ~6 bps/hop, veio {bps}");
        // Total do haircut não pode passar do edge.
        assert!((bps as f64) * 3.0 <= 20.0 + 1.0);
        // E jamais acima do teto configurado.
        assert!(bps <= 50);
    }

    #[test]
    fn slippage_budget_respects_ceiling_when_edge_is_fat() {
        // Edge gordo (5%) não libera slippage infinito: teto do config manda.
        let bps = max_slippage_bps_for_edge(5.0, 100.0, 3, 50);
        assert_eq!(bps, 50);
    }

    #[test]
    fn slippage_budget_floors_on_zero_or_negative_edge() {
        // Sem lucro, aperta no piso — não abre folga para sandwich.
        assert_eq!(max_slippage_bps_for_edge(0.0, 100.0, 3, 50), MIN_SLIPPAGE_BPS);
        assert_eq!(max_slippage_bps_for_edge(-1.0, 100.0, 3, 50), MIN_SLIPPAGE_BPS);
        assert_eq!(
            max_slippage_bps_for_edge(f64::NAN, 100.0, 3, 50),
            MIN_SLIPPAGE_BPS
        );
        // n_hops=0 não divide por zero.
        assert!(max_slippage_bps_for_edge(0.20, 100.0, 0, 50) >= MIN_SLIPPAGE_BPS);
    }

    #[test]
    fn negative_and_nonfinite_inputs_are_safe() {
        assert_eq!(gross_profit_usd(-1.0, 1.01), 0.0);
        assert_eq!(gross_profit_usd(100.0, f64::NAN), 0.0);
        assert_eq!(flashloan_fee_usd(100.0, -0.1), 0.0);
        assert_eq!(flashloan_fee_usd(f64::INFINITY, 0.0005), 0.0);
        // rate < 1 => gross negativo (prejuízo), preservado
        assert!(gross_profit_usd(100.0, 0.999) < 0.0);
    }
}
