//! ===========================================================
//! 🌐 AppMiddleware — cliente Ethereum unificado
//! -----------------------------------------------------------
//! Define o tipo de middleware padrão usado em todo o projeto.
//! Este tipo combina:
//! - Provider HTTP (RPC)
//! - Carteira LocalWallet (assinatura de transações)
//! - Compatibilidade com `DexManager<M: Middleware>`
//! ===========================================================

use std::sync::Arc;
use ethers::{
    middleware::SignerMiddleware,
    providers::Provider,
    providers::Http,
    signers::LocalWallet,
};

/// Tipo padrão usado em toda a aplicação.
/// Implementa `Middleware` e é compatível com o `DexManager<M: Middleware>`.
pub type AppMiddleware = SignerMiddleware<Provider<Http>, LocalWallet>;

impl AppMiddleware {
    /// Cria um novo `AppMiddleware` com provider e carteira.
    pub fn new(provider: Arc<Provider<Http>>, wallet: LocalWallet) -> Self {
        SignerMiddleware::new(provider, wallet)
    }
}

