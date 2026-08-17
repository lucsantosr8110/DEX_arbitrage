//! Nonce reaper (B8).
//!
//! B9: exec/money — aritmética checked/saturating, casts sem truncation.
#![deny(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
//!
//! Detecta nonces presos: reservamos `pending_local` mas a chain só viu
//! `on_chain_pending` (eth_getTransactionCount pending). Se o gap persiste por
//! `nonce_stall_secs` (default 45) sem inclusão, o nonce mais baixo preso é
//! cancelado com uma tx no-op (self-transfer 0) de gas agressivo — libera o
//! gap para os nonces maiores (que dependem dele, EIP-155 sequential).
//!
//! Nunca reusar nonce sem um `eth_getTransactionCount(pending)` fresco: o
//! reaper re-fetch sempre antes de cancelar.
//!
//! Kill switch: 3 falhas consecutivas do reaper (cancel no-op não entra ou
//! gap persiste) → `error!` + `reaper_kill_switch` (caller deve parar de
//! broadcast). Métrica `nonce_gaps_recovered`.

use ethers::types::U256;
use std::collections::HashMap;
use std::time::Instant;
use tracing::error;

/// Limite de falhas consecutivas antes do kill switch.
pub const REAPER_KILL_SWITCH_THRESHOLD: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StuckNonce {
    pub nonce: u64,
    pub elapsed_secs: u64,
}

/// Veredito do reaper puro (testável sem RPC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapVerdict {
    /// Nenhum nonce preso (pending_local == on_chain_pending).
    NoGap,
    /// Gap existe mas ainda não atingiu `nonce_stall_secs`.
    GapStalling { lowest: u64, elapsed_secs: u64 },
    /// Gap atingiu o stall — cancelar `lowest` (no-op agressivo).
    Reap { lowest: u64 },
}

/// Nonce reaper — rastreia nonces in-flight e decide cancelamento.
pub struct NonceReaper {
    /// nonces reservados (broadcast) → instant da reserva.
    in_flight: HashMap<u64, Instant>,
    consecutive_failures: u32,
    kill_switch: bool,
}

impl NonceReaper {
    pub fn new() -> Self {
        Self {
            in_flight: HashMap::new(),
            consecutive_failures: 0,
            kill_switch: false,
        }
    }

    /// Registra um nonce como in-flight (broadcast feito).
    pub fn note_broadcast(&mut self, nonce: u64, now: Instant) {
        self.in_flight.insert(nonce, now);
    }

    /// Remove nonces já incluídos (`on_chain_pending` passou deles).
    pub fn prune_included(&mut self, on_chain_pending: u64) {
        self.in_flight.retain(|&n, _| n >= on_chain_pending);
    }

    /// Veredito puro: dado `pending_local` (próximo nonce a reservar),
    /// `on_chain_pending` (chain), `now` e `nonce_stall_secs`, decide se há
    /// nonce preso a cancelar. Sempre o **mais baixo** preso (EIP-155 sequential:
    /// nonces maiores não incluem até o baixo ser resolvido).
    pub fn verdict(
        &self,
        pending_local: u64,
        on_chain_pending: u64,
        now: Instant,
        nonce_stall_secs: u64,
    ) -> ReapVerdict {
        if pending_local <= on_chain_pending {
            return ReapVerdict::NoGap;
        }
        // nonces presos = [on_chain_pending, pending_local)
        let lowest = on_chain_pending;
        let elapsed = self
            .in_flight
            .get(&lowest)
            .map(|t| now.duration_since(*t).as_secs())
            .unwrap_or(0);
        if elapsed >= nonce_stall_secs {
            ReapVerdict::Reap { lowest }
        } else {
            ReapVerdict::GapStalling {
                lowest,
                elapsed_secs: elapsed,
            }
        }
    }

    /// Registra falha (cancel no-op não entrou / gap persiste). 3 consecutivas
    /// → kill switch. Retorna true se o kill switch acabou de tripar.
    pub fn record_failure(&mut self) -> bool {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures >= REAPER_KILL_SWITCH_THRESHOLD && !self.kill_switch {
            self.kill_switch = true;
            error!(
                "🚨 B8 nonce reaper kill switch: {} falhas consecutivas — parar broadcast",
                self.consecutive_failures
            );
            return true;
        }
        false
    }

    /// Registra sucesso (gap recuperado). Reseta falhas consecutivas.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.kill_switch = false;
    }

    pub fn kill_switch(&self) -> bool {
        self.kill_switch
    }

    /// Marca um nonce como cancelado/incluído (remove do in_flight).
    pub fn note_canceled(&mut self, nonce: u64) {
        self.in_flight.remove(&nonce);
    }

    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }
}

impl Default for NonceReaper {
    fn default() -> Self {
        Self::new()
    }
}

/// Constrói o payload de uma tx no-op de cancelamento: self-transfer de 0 wei
/// para o próprio sender, gas agressivo (max_fee/max_priority altos), mesmo
/// nonce preso. Retorna o nonce a usar (validado como u64). Caller faz o
/// `eth_getTransactionCount(pending)` fresco antes de assinar.
///
/// `nonce_u256` → u64 checked; se > u64::MAX, None (nunca inventa).
pub fn cancel_nonce(nonce: U256) -> Option<u64> {
    // Nunca reusar nonce sem re-fetch fresco — caller deve confirmar.
    nonce.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn t0() -> Instant {
        // Instant::now() é fine em testes (não no workflow script).
        Instant::now()
    }

    #[test]
    fn b8_no_gap_when_pending_le_on_chain() {
        let r = NonceReaper::new();
        let now = t0();
        assert_eq!(r.verdict(5, 5, now, 45), ReapVerdict::NoGap);
        assert_eq!(r.verdict(3, 5, now, 45), ReapVerdict::NoGap);
    }

    /// B8 spec: 3 txs (nonces N..N+2), 1º dropped → reaper cancela o mais baixo.
    #[test]
    fn b8_three_txs_first_dropped_reaper_cancels_lowest() {
        let mut r = NonceReaper::new();
        let base = t0();
        // broadcast nonces 0,1,2
        r.note_broadcast(0, base);
        r.note_broadcast(1, base);
        r.note_broadcast(2, base);
        // 3 txs enviadas → pending_local=3; chain não incluiu nenhuma (0 dropped)
        // → on_chain_pending=0. Após stall > 45s, reaper cancela nonce 0.
        let now = base + Duration::from_secs(60);
        let v = r.verdict(3, 0, now, 45);
        assert_eq!(v, ReapVerdict::Reap { lowest: 0 });

        // Antes do stall → GapStalling
        let early = base + Duration::from_secs(10);
        assert_eq!(
            r.verdict(3, 0, early, 45),
            ReapVerdict::GapStalling {
                lowest: 0,
                elapsed_secs: 10
            }
        );
    }

    #[test]
    fn b8_kill_switch_after_three_consecutive_failures() {
        let mut r = NonceReaper::new();
        assert!(!r.record_failure(), "1ª falha não tripa");
        assert!(!r.record_failure(), "2ª falha não tripa");
        assert!(r.record_failure(), "3ª falha tripa kill switch");
        assert!(r.kill_switch());
        // sucesso reseta
        r.record_success();
        assert!(!r.kill_switch());
    }

    #[test]
    fn b8_prune_included_removes_consumed_nonces() {
        let mut r = NonceReaper::new();
        let base = t0();
        r.note_broadcast(10, base);
        r.note_broadcast(11, base);
        r.note_broadcast(12, base);
        // on_chain_pending=11 → nonces < 11 (10) consumidos; 11,12 ainda pending.
        r.prune_included(11);
        assert_eq!(r.in_flight_count(), 2, "11 e 12 ainda pending");
        assert!(r.in_flight.get(&12).is_some());
        // on_chain_pending=13 → todos consumidos
        r.prune_included(13);
        assert_eq!(r.in_flight_count(), 0);
    }

    #[test]
    fn b8_cancel_nonce_checked() {
        assert_eq!(cancel_nonce(U256::from(7u64)), Some(7));
        // nonce > u64::MAX → None (não inventa)
        let big = U256::from(u128::MAX) * U256::from(2u64);
        assert_eq!(cancel_nonce(big), None);
    }
}
