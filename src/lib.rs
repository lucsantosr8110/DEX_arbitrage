// src/lib.rs

pub mod api;
pub mod config;
pub mod contracts;
pub mod core;
pub mod dex;
pub mod execution;  // ✅ Este deve existir
pub mod infra;      // ✅ Este deve existir
pub mod tui;
pub mod utils;
pub use dex::DexContract;

use ethers::{
    middleware::SignerMiddleware,
    prelude::*,
    providers::{Http, Provider},
};
use std::sync::Arc;

// Definição do middleware principal (SignerMiddleware sobre Provider)
// É necessário para enviar transações (execução de arbitragem)
pub type AppMiddleware = SignerMiddleware<Arc<Provider<Http>>, Wallet<k256::ecdsa::SigningKey>>;

// Re-export para facilitar o uso em outros módulos
///pub use crate::error::BotError;
pub use crate::infra::rpc_provider::RpcProvider; // ✅ Re-exporta o BotError
