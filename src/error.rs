// src/error.rs

use thiserror::Error;

/// Erros semânticos do bot, usados para logs claros, métricas e tomadas de decisão.
#[derive(Debug, Error)]
pub enum BotError {
    #[error("Simulação rejeitada pela blockchain: {0}")]
    SimulationRejected(String),

    #[error("Falha ao obter cotação da DEX: {0}")]
    DexUnavailable(String),

    #[error("Custo de gás estimado excedeu o limite: {0}")]
    GasTooHigh(String),

    #[error("Oportunidade rejeitada pelo RiskManager: {0}")]
    RiskRejected(String),

    #[error("Erro de configuração: {0}")]
    ConfigurationError(String),

    #[error("Erro de Rede ou RPC: {0}")]
    RpcError(String),

    #[error("Gorjeta de Prioridade calculada é maior que o lucro líquido permitido: {0}")]
    PriorityTipTooHigh(String),

    #[error("Erro na conversão de tipos U256/f64: {0}")]
    ConversionError(String),

    #[error("Erro genérico: {0}")]
    Generic(String),
}
