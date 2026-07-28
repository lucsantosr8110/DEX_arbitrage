//! GasOracle EWMA por venue (B2).
//!
//! Calibra o perfil estático (`gas_profile::swap_gas_units`) com `gas_used`
//! real dos receipts. Cada venue mantém um EWMA (alpha 0.15). A estimativa
//! calibrada usa `max(estático, ewma_p75)` — nunca abaixo do estático até ter
//! ≥20 amostras daquele venue (permanece fail-safe superestimado até então).
//!
//! Persistência: JSON em disco para sobreviver a restart. O path vem da config
//! (`gas_oracle_path`); se `None`, opera só em memória (sem inventar path
//! default — persistência exige opt-in do operador).
//!
//! `p75` é uma aproximação conservadora (`mean * 1.15`) — EWMA rastreia a média,
//! não a distribuição completa; o buffer de 15% cobre a cauda típica de gas em
//! Polygon sem subestimar. Documentado para não passar silencioso.

use crate::core::gas_profile::{
    classify_step, flashloan_overhead_gas, swap_gas_units, CALLDATA_GAS_PER_HOP,
    FlashloanProvider, TX_BASE_GAS, VenueKind,
};
use crate::core::types::ArbitrageStep;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::{debug, warn};

/// Alpha do EWMA (peso da nova amostra). 0.15 = 15% observação / 85% histórico.
const EWMA_ALPHA: f64 = 0.15;
/// Mínimo de amostras antes de confiar no EWMA (abaixo disso, só estático).
const MIN_SAMPLES: usize = 20;
/// Buffer p75 sobre a média EWMA (aproximação conservadora da cauda).
const P75_BUFFER: f64 = 1.15;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VenueEwma {
    mean: f64,
    n_samples: u64,
}

impl VenueEwma {
    fn new() -> Self {
        Self {
            mean: 0.0,
            n_samples: 0,
        }
    }

    fn record(&mut self, x: f64) {
        if !x.is_finite() || x <= 0.0 {
            return;
        }
        if self.n_samples == 0 {
            self.mean = x;
        } else {
            self.mean = self.mean * (1.0 - EWMA_ALPHA) + x * EWMA_ALPHA;
        }
        self.n_samples = self.n_samples.saturating_add(1);
    }

    /// p75 aproximado (mean * 1.15). `None` se < MIN_SAMPLES amostras.
    fn p75(&self) -> Option<f64> {
        if (self.n_samples as usize) < MIN_SAMPLES {
            return None;
        }
        if !self.mean.is_finite() || self.mean <= 0.0 {
            return None;
        }
        Some(self.mean * P75_BUFFER)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GasOracle {
    by_venue: HashMap<VenueKey, VenueEwma>,
}

/// `VenueKind` tem `CurveStable { n_coins }` (não-Eq em HashMap direto via serde
/// sem tag); serializamos uma chave estável. Mantém 1:1 com `VenueKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
enum VenueKey {
    UniV2,
    UniV3,
    QuickSwapV2,
    QuickSwapV3,
    SushiV2,
    CurveStable { n_coins: u8 },
    BalancerWeighted,
    Unknown,
}

impl From<VenueKind> for VenueKey {
    fn from(v: VenueKind) -> Self {
        match v {
            VenueKind::UniV2 => VenueKey::UniV2,
            VenueKind::UniV3 => VenueKey::UniV3,
            VenueKind::QuickSwapV2 => VenueKey::QuickSwapV2,
            VenueKind::QuickSwapV3 => VenueKey::QuickSwapV3,
            VenueKind::SushiV2 => VenueKey::SushiV2,
            VenueKind::CurveStable { n_coins } => VenueKey::CurveStable { n_coins },
            VenueKind::BalancerWeighted => VenueKey::BalancerWeighted,
            VenueKind::Unknown => VenueKey::Unknown,
        }
    }
}

impl GasOracle {
    /// Carrega do path se configurado; se ausente/inválido, começa vazio
    /// (fail-safe: perfil estático continua válido). Nunca panic.
    pub fn load(path: Option<PathBuf>) -> Self {
        let Some(p) = path else { return Self::default() };
        match std::fs::read_to_string(&p) {
            Ok(s) => match serde_json::from_str::<GasOracle>(&s) {
                Ok(o) => {
                    debug!("GasOracle carregado de {:?}", p);
                    o
                }
                Err(e) => {
                    warn!("GasOracle JSON inválido em {:?}: {} — usando vazio", p, e);
                    Self::default()
                }
            },
            Err(_) => {
                debug!("GasOracle arquivo ausente em {:?} — iniciando vazio", p);
                Self::default()
            }
        }
    }

    /// Persiste em disco. Falha só loga warn (não aborta execução).
    pub fn save(&self, path: Option<&PathBuf>) {
        let Some(p) = path else { return };
        match serde_json::to_string_pretty(self) {
            Ok(s) => {
                if let Err(e) = std::fs::write(p, s) {
                    warn!("GasOracle falha ao salvar em {:?}: {}", p, e);
                }
            }
            Err(e) => warn!("GasOracle falha ao serializar: {}", e),
        }
    }

    /// Atribui `actual_total_gas` da tx proporcionalmente ao perfil estático de
    /// cada hop, e atualiza o EWMA por venue. `venues` = venues dos hops (sem
    /// overhead/base, que são compartilhados — só swaps são calibrados por venue).
    pub fn record_route(&mut self, venues: &[VenueKind], actual_total_gas: u64) {
        if venues.is_empty() || actual_total_gas == 0 {
            return;
        }
        let total_weight: u64 = venues.iter().map(|v| swap_gas_units(*v)).sum();
        if total_weight == 0 {
            return;
        }
        for v in venues {
            let weight = swap_gas_units(*v) as f64;
            let frac = weight / total_weight as f64;
            let attributed = actual_total_gas as f64 * frac;
            let key = VenueKey::from(*v);
            let ewma = self.by_venue.entry(key).or_insert_with(VenueEwma::new);
            ewma.record(attributed);
        }
    }

    /// p75 calibrado por venue, se disponível (≥20 amostras).
    pub fn p75(&self, v: VenueKind) -> Option<f64> {
        self.by_venue.get(&VenueKey::from(v)).and_then(|e| e.p75())
    }

    /// Estimativa de gas de um swap: `max(estático, ewma_p75)` se p75 disponível,
    /// senão só estático. Nunca abaixo do estático (fail-safe até ≥20 amostras).
    pub fn swap_gas_estimate(&self, v: VenueKind) -> u64 {
        let static_units = swap_gas_units(v);
        match self.p75(v) {
            Some(p75) if p75.is_finite() && p75 > 0.0 => {
                // max(estático, ewma_p75) — conservador.
                static_units.max(p75 as u64)
            }
            _ => static_units,
        }
    }

    /// Número de amostras por venue (para telemetria/debug).
    pub fn samples(&self, v: VenueKind) -> u64 {
        self.by_venue
            .get(&VenueKey::from(v))
            .map(|e| e.n_samples)
            .unwrap_or(0)
    }
}

/// Estimativa calibrada de gas de uma rota: base + overhead + Σ por hop, onde
/// cada swap usa `oracle.swap_gas_estimate(v)` = `max(estático, ewma_p75)`.
/// Venues sem ≥20 amostras caem no estático (fail-safe). `Unknown` continua
/// superestimado via `swap_gas_units` e emite `warn!` em `classify_step`.
pub fn estimate_gas_units_calibrated(
    steps: &[ArbitrageStep],
    provider: Option<FlashloanProvider>,
    oracle: &GasOracle,
) -> u64 {
    let mut total = TX_BASE_GAS;
    if let Some(p) = provider {
        total = total.saturating_add(flashloan_overhead_gas(p));
    }
    for s in steps {
        let v = classify_step(s);
        let swap = oracle.swap_gas_estimate(v);
        let hop = swap.checked_add(CALLDATA_GAS_PER_HOP).unwrap_or(u64::MAX);
        total = total.saturating_add(hop);
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::gas_profile::VenueKind;

    #[test]
    fn uncalibrated_venue_returns_static() {
        let o = GasOracle::default();
        assert_eq!(o.swap_gas_estimate(VenueKind::UniV2), 105_000);
        assert_eq!(o.p75(VenueKind::UniV2), None);
    }

    #[test]
    fn below_min_samples_still_static() {
        let mut o = GasOracle::default();
        for _ in 0..19 {
            o.record_route(&[VenueKind::UniV2], 120_000);
        }
        // 19 amostras < MIN_SAMPLES(20) → p75 None → estático.
        assert_eq!(o.swap_gas_estimate(VenueKind::UniV2), 105_000);
    }

    #[test]
    fn at_min_samples_uses_max_static_p75() {
        let mut o = GasOracle::default();
        // Registra 25x gas real alto (150k atribuído a UniV2 num rota só-V2).
        for _ in 0..25 {
            o.record_route(&[VenueKind::UniV2], 150_000);
        }
        let est = o.swap_gas_estimate(VenueKind::UniV2);
        // p75 ≈ mean*1.15. mean converge perto de 150k → p75 ~172k > estático 105k.
        assert!(est > 105_000, "deve usar p75 calibrado: {}", est);
        assert!(est >= 150_000, "p75 deve ser >= mean: {}", est);
    }

    #[test]
    fn never_below_static_even_with_low_real() {
        let mut o = GasOracle::default();
        // gas real baixo (80k) — p75 < estático → max mantém estático.
        for _ in 0..25 {
            o.record_route(&[VenueKind::UniV2], 80_000);
        }
        assert_eq!(
            o.swap_gas_estimate(VenueKind::UniV2),
            105_000,
            "nunca abaixo do estático"
        );
    }

    #[test]
    fn proportional_attribution_multi_venue() {
        let mut o = GasOracle::default();
        // Rota V2(105k) + V3(165k), total real 300k. V2 frac = 105/270.
        o.record_route(&[VenueKind::UniV2, VenueKind::UniV3], 300_000);
        assert_eq!(o.samples(VenueKind::UniV2), 1);
        assert_eq!(o.samples(VenueKind::UniV3), 1);
    }

    #[test]
    fn load_save_roundtrip() {
        let mut o = GasOracle::default();
        for _ in 0..25 {
            o.record_route(&[VenueKind::UniV3], 200_000);
        }
        let tmp = std::env::temp_dir().join("gas_oracle_test_r2.json");
        o.save(Some(&tmp));
        let loaded = GasOracle::load(Some(tmp.clone()));
        assert_eq!(loaded.samples(VenueKind::UniV3), 25);
        let _ = std::fs::remove_file(tmp);
    }
}