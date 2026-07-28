//! ===========================================================
//! 🌐 AppMiddleware — cliente Ethereum unificado
//! -----------------------------------------------------------
//! Define o tipo de middleware padrão usado em todo o projeto.
//! Este tipo combina:
//! - Provider HTTP rotativo (RPC com failover em runtime)
//! - Carteira LocalWallet (assinatura de transações)
//! - Compatibilidade com `DexManager<M: Middleware>`
//! ===========================================================

use std::sync::Arc;
use ethers::{
    middleware::SignerMiddleware,
    providers::Provider,
    signers::LocalWallet,
};

use crate::infra::rotating_http_client::RotatingHttpClient;

/// Tipo padrão usado em toda a aplicação.
/// Implementa `Middleware` e é compatível com o `DexManager<M: Middleware>`.
pub type AppMiddleware = SignerMiddleware<Provider<RotatingHttpClient>, LocalWallet>;

impl AppMiddleware {
    /// Cria um novo `AppMiddleware` com provider e carteira.
    pub fn new(provider: Arc<Provider<RotatingHttpClient>>, wallet: LocalWallet) -> Self {
        SignerMiddleware::new(provider, wallet)
    }
}
