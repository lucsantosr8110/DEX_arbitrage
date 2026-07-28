//! Ledger de finalidade de lucro (B7).
//!
//! B9: money — aritmética checked/saturating, casts sem truncation.
#![deny(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
//!
//! Um tx confirmado com 1 confirmação NÃO é lucro final: a Polygon reorga
//! (raramente, mas reorga). Contabilização separada:
//!   - **provisory**: lucro/prejuízo de txs incluídas mas ainda não final
//!   - **final**: lucro/prejuízo de txs com ≥ `profit_confirmations` blocos
//!
//! Estado por trade: `Pending → Included(block) → Confirmed(n) → Final`.
//! `promote(current_block, profit_confirmations)` avança os estados.
//!
//! `ProfitSource`:
//!   - `Realized { final_ }` — lucro/prejuízo medido do receipt (evento decode).
//!   - `Estimated` — fallback do gross teórico do finder (A1: receipt sem evento
//!     decodificável). **NÃO** alimenta o circuit breaker de perda: uma perda
//!     estimada pode ser ruído de modelagem, não perda real on-chain.
//!
//! Reorg: se o blockHash mudar ou a tx desaparecer da chain, o trade provisório
//! é revertido (removido do provisory), `error!` + métrica `reorg_reverted_trades`.
//!
//! Circuit breaker de perda: conta apenas perdas `Realized { final_: true }`
//! consecutivas; ≥ `loss_breaker_threshold` → tripped (kill switch consome).

use ethers::types::H256;
use std::collections::HashMap;
use tracing::error;

/// Origem do PnL de um trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfitSource {
    /// PnL medido do receipt (evento de lucro decodificado). `final_` = já
    /// finalizado (≥ profit_confirmations).
    Realized { final_: bool },
    /// PnL estimado do gross teórico do finder (A1 fallback). NÃO alimenta o
    /// circuit breaker de perda — pode ser ruído de modelagem.
    Estimated,
}

/// Estado de finalidade de um trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeFinality {
    /// Tx broadcast, ainda sem receipt.
    Pending,
    /// Receipt confirmado no bloco `block` (1 confirmação).
    Included { block: u64 },
    /// `n` confirmações acumuladas, ainda < `profit_confirmations`.
    Confirmed { n: u64 },
    /// ≥ `profit_confirmations` confirmações — irreversível na prática.
    Final,
}

#[derive(Debug, Clone)]
pub struct TradeEntry {
    pub tx_hash: H256,
    pub inclusion_block: u64,
    pub state: TradeFinality,
    /// Lucro assinado em USD: > 0 profit, < 0 loss.
    pub profit_usd: f64,
    pub source: ProfitSource,
}

/// Ledger de trades com finalidade + circuit breaker de perda.
pub struct ProfitLedger {
    trades: HashMap<H256, TradeEntry>,
    provisory_profit_usd: f64,
    final_profit_usd: f64,
    reorg_reverted_trades: u64,
    consecutive_final_losses: u32,
    loss_breaker_threshold: u32,
    breaker_tripped: bool,
}

impl ProfitLedger {
    pub fn new(loss_breaker_threshold: u32) -> Self {
        Self {
            trades: HashMap::new(),
            provisory_profit_usd: 0.0,
            final_profit_usd: 0.0,
            reorg_reverted_trades: 0,
            consecutive_final_losses: 0,
            loss_breaker_threshold,
            breaker_tripped: false,
        }
    }

    /// B7: ajusta o threshold do circuit breaker de perda (init via config).
    pub fn set_loss_breaker_threshold(&mut self, t: u32) {
        self.loss_breaker_threshold = t;
    }

    /// Registra tx incluída (receipt status 1) no bloco `block`. Estado inicial
    /// `Included` (1 confirmação). PnL entra no provisory. Não conta no breaker
    /// (só final conta).
    pub fn record_included(
        &mut self,
        tx_hash: H256,
        block: u64,
        profit_usd: f64,
        source: ProfitSource,
    ) {
        // Subtrai provisory antigo se re-gravação do mesmo hash.
        if let Some(prev) = self.trades.get(&tx_hash) {
            if !matches!(prev.state, TradeFinality::Final) {
                self.provisory_profit_usd -= prev.profit_usd;
            } else {
                self.final_profit_usd -= prev.profit_usd;
            }
        }
        self.provisory_profit_usd += profit_usd;
        self.trades.insert(
            tx_hash,
            TradeEntry {
                tx_hash,
                inclusion_block: block,
                state: TradeFinality::Included { block },
                profit_usd,
                source,
            },
        );
    }

    /// Avança estados dados `current_block`. `Included → Confirmed(n)` quando
    /// n = current_block − inclusion_block; `Confirmed → Final` quando
    /// n ≥ profit_confirmations. No Final: move PnL de provisory → final e
    /// atualiza o circuit breaker de perda (só Realized final).
    pub fn promote(&mut self, current_block: u64, profit_confirmations: u64) {
        let hash_list: Vec<H256> = self.trades.keys().copied().collect();
        for h in hash_list {
            let entry = match self.trades.get_mut(&h) {
                Some(e) => e,
                None => continue,
            };
            let n = current_block.saturating_sub(entry.inclusion_block);
            let new_state = match entry.state {
                TradeFinality::Pending => TradeFinality::Pending,
                TradeFinality::Included { .. } | TradeFinality::Confirmed { .. } => {
                    if n >= profit_confirmations {
                        TradeFinality::Final
                    } else if n > 0 {
                        TradeFinality::Confirmed { n }
                    } else {
                        TradeFinality::Included {
                            block: entry.inclusion_block,
                        }
                    }
                }
                TradeFinality::Final => TradeFinality::Final,
            };
            let became_final = matches!(new_state, TradeFinality::Final)
                && !matches!(entry.state, TradeFinality::Final);
            entry.state = new_state;
            if became_final {
                // Move PnL provisory → final.
                self.provisory_profit_usd -= entry.profit_usd;
                self.final_profit_usd += entry.profit_usd;
                entry.source = match entry.source {
                    ProfitSource::Realized { .. } => ProfitSource::Realized { final_: true },
                    ProfitSource::Estimated => ProfitSource::Estimated,
                };
                // Circuit breaker: SÓ Realized final conta.
                if matches!(entry.source, ProfitSource::Realized { final_: true }) {
                    if entry.profit_usd < 0.0 {
                        self.consecutive_final_losses =
                            self.consecutive_final_losses.saturating_add(1);
                        if self.consecutive_final_losses >= self.loss_breaker_threshold {
                            self.breaker_tripped = true;
                        }
                    } else {
                        self.consecutive_final_losses = 0;
                    }
                }
            }
        }
    }

    /// Reorg: reverte um trade provisório (tx sumiu ou blockHash mudou).
    /// Remove do provisory, `error!`, incrementa `reorg_reverted_trades`.
    /// Trades Final NÃO são revertidos (≥ confirmations — irreversível).
    pub fn revert(&mut self, tx_hash: H256) -> bool {
        let entry = match self.trades.get(&tx_hash) {
            Some(e) => e,
            None => return false,
        };
        if matches!(entry.state, TradeFinality::Final) {
            // Final não reverte por reorg rasa.
            return false;
        }
        let profit = entry.profit_usd;
        let block = entry.inclusion_block;
        self.provisory_profit_usd -= profit;
        self.trades.remove(&tx_hash);
        self.reorg_reverted_trades = self.reorg_reverted_trades.saturating_add(1);
        // Reseta contagem de perdas consecutivas — uma reorg invalida a série.
        self.consecutive_final_losses = 0;
        error!(
            tx_hash = ?tx_hash, inclusion_block = block, profit_usd = profit,
            "🔄 REORG: trade provisório revertido (tx desapareceu / blockHash mudou)"
        );
        crate::infra::metrics::inc_reorg_reverted_trades();
        true
    }

    /// Contadores públicos (telemetria).
    pub fn provisory_profit_usd(&self) -> f64 {
        self.provisory_profit_usd
    }
    pub fn final_profit_usd(&self) -> f64 {
        self.final_profit_usd
    }
    pub fn reorg_reverted_trades(&self) -> u64 {
        self.reorg_reverted_trades
    }
    pub fn breaker_tripped(&self) -> bool {
        self.breaker_tripped
    }
    pub fn len(&self) -> usize {
        self.trades.len()
    }
    pub fn is_empty(&self) -> bool {
        self.trades.is_empty()
    }
    pub fn state_of(&self, tx_hash: H256) -> Option<TradeFinality> {
        self.trades.get(&tx_hash).map(|e| e.state)
    }
}

/// B7 — veredito de reorg puro (testável sem RPC).
/// Reorg se o blockHash do bloco `N` mudou desde a última observação OU se a tx
/// que estava inclusa desapareceu (não está mais no bloco `N`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReorgVerdict {
    NoReorg,
    BlockHashChanged,
    TxDisappeared,
}

pub fn detect_reorg(
    prev_block_hash: Option<H256>,
    cur_block_hash: H256,
    tx_present: bool,
) -> ReorgVerdict {
    if !tx_present {
        return ReorgVerdict::TxDisappeared;
    }
    match prev_block_hash {
        Some(prev) if prev != cur_block_hash => ReorgVerdict::BlockHashChanged,
        _ => ReorgVerdict::NoReorg,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(b: u8) -> H256 {
        H256::repeat_byte(b)
    }

    #[test]
    fn b7_finality_promotion_to_final_after_confirmations() {
        let mut led = ProfitLedger::new(3);
        led.record_included(h(1), 100, 5.0, ProfitSource::Realized { final_: false });
        assert_eq!(
            led.state_of(h(1)),
            Some(TradeFinality::Included { block: 100 })
        );
        assert!((led.provisory_profit_usd() - 5.0).abs() < 1e-12);
        assert!(led.final_profit_usd().abs() < 1e-12);

        // 1 confirmação → Confirmed(1), ainda provisory
        led.promote(101, 24);
        assert_eq!(led.state_of(h(1)), Some(TradeFinality::Confirmed { n: 1 }));
        assert!((led.provisory_profit_usd() - 5.0).abs() < 1e-12);

        // 24 confirmações → Final
        led.promote(124, 24);
        assert_eq!(led.state_of(h(1)), Some(TradeFinality::Final));
        assert!(led.provisory_profit_usd().abs() < 1e-12);
        assert!((led.final_profit_usd() - 5.0).abs() < 1e-12);
    }

    #[test]
    fn b7_reorg_reverts_provisory_not_final() {
        let mut led = ProfitLedger::new(3);
        led.record_included(h(2), 200, 10.0, ProfitSource::Realized { final_: false });
        // Reorg antes de final → reverte
        assert!(led.revert(h(2)));
        assert_eq!(led.reorg_reverted_trades(), 1);
        assert!(led.is_empty());
        assert!(led.provisory_profit_usd().abs() < 1e-12);

        // Trade final não reverte
        led.record_included(h(3), 300, 7.0, ProfitSource::Realized { final_: false });
        led.promote(324, 24);
        assert_eq!(led.state_of(h(3)), Some(TradeFinality::Final));
        assert!(!led.revert(h(3)), "final não reverte por reorg rasa");
        assert_eq!(led.reorg_reverted_trades(), 1);
    }

    #[test]
    fn b7_estimated_loss_does_not_feed_loss_breaker() {
        let mut led = ProfitLedger::new(2);
        // 2 perdas Estimated finalizadas → breaker NÃO tripa
        for i in 0..2u8 {
            let bh = h(10 + i);
            led.record_included(bh, 1000 + i as u64, -5.0, ProfitSource::Estimated);
            led.promote(1000 + i as u64 + 24, 24);
            assert_eq!(led.state_of(bh), Some(TradeFinality::Final));
        }
        assert!(!led.breaker_tripped(), "Estimated não alimenta breaker");

        // 2 perdas Realized finalizadas → breaker tripa
        for i in 0..2u8 {
            let bh = h(20 + i);
            led.record_included(
                bh,
                2000 + i as u64,
                -3.0,
                ProfitSource::Realized { final_: false },
            );
            led.promote(2000 + i as u64 + 24, 24);
        }
        assert!(
            led.breaker_tripped(),
            "Realized final consecutivo tripa breaker"
        );
    }

    #[test]
    fn b7_detect_reorg_verdicts() {
        let a = h(0xAA);
        let b = h(0xBB);
        // tx ausente → TxDisappeared (precede hash)
        assert_eq!(detect_reorg(Some(a), a, false), ReorgVerdict::TxDisappeared);
        // hash mudou → BlockHashChanged
        assert_eq!(
            detect_reorg(Some(a), b, true),
            ReorgVerdict::BlockHashChanged
        );
        // sem prev hash, tx presente → NoReorg (primeira observação)
        assert_eq!(detect_reorg(None, a, true), ReorgVerdict::NoReorg);
        // hash igual, tx presente → NoReorg
        assert_eq!(detect_reorg(Some(a), a, true), ReorgVerdict::NoReorg);
    }

    #[test]
    fn b7_realized_profit_resets_consecutive_losses() {
        let mut led = ProfitLedger::new(3);
        led.record_included(h(1), 10, -5.0, ProfitSource::Realized { final_: false });
        led.promote(10 + 24, 24);
        assert_eq!(led.state_of(h(1)), Some(TradeFinality::Final));
        assert!(!led.breaker_tripped());
        // lucro final reseta a contagem
        led.record_included(h(2), 40, 5.0, ProfitSource::Realized { final_: false });
        led.promote(40 + 24, 24);
        // agora 2 perdas seguidas precisam de novo → não tripa com 1
        led.record_included(h(3), 80, -5.0, ProfitSource::Realized { final_: false });
        led.promote(80 + 24, 24);
        assert!(
            !led.breaker_tripped(),
            "lucro resetou série; 1 perda não tripa (threshold 3)"
        );
    }
}
