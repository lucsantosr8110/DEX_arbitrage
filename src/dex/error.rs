// ============================================================
// src/dex/error.rs — v3.7.5 (Erros unificados DEX Layer)
// ============================================================

use thiserror::Error;

/// Erros unificados para camada DEX (manager, radar, adapters)
#[derive(Debug, Error)]
pub enum DexError {
    #[error("Falha ao conectar à DEX: {0}")]
    ConnectionError(String),

    #[error("Erro ao obter preço da DEX {0}: {1}")]
    PriceFetchError(String, String),

    #[error("Contrato DEX inválido: {0}")]
    InvalidContract(String),

    #[error("DEX bloqueada pelo Circuit Breaker: {0}")]
    CircuitBreakerActive(String),

    #[error("Timeout ao consultar DEX {0}")]
    Timeout(String),

    #[error("Erro interno: {0}")]
    InternalError(String),
}

pub type DexResult<T> = Result<T, DexError>;
