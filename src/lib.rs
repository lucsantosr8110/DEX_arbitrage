// src/lib.rs

pub mod api;
pub mod config;
pub mod contracts;
pub mod core;
pub mod dex;
pub mod emergency_shutdown;
pub mod execution; // ✅ Este deve existir
pub mod infra; // ✅ Este deve existir
pub mod tui;
pub mod utils;
pub use dex::DexContract;

use ethers::{middleware::SignerMiddleware, prelude::*, providers::Provider};
use std::sync::Arc;

// Cliente HTTP com failover/rotacao de endpoints em runtime.
pub use crate::infra::rotating_http_client::RotatingHttpClient;

// Definição do middleware principal (SignerMiddleware sobre Provider)
// É necessário para enviar transações (execução de arbitragem)
pub type AppMiddleware =
    SignerMiddleware<Arc<Provider<RotatingHttpClient>>, Wallet<k256::ecdsa::SigningKey>>;

// Re-export para facilitar o uso em outros módulos
///pub use crate::error::BotError;
pub use crate::infra::rpc_provider::RpcProvider; // ✅ Re-exporta o BotError
