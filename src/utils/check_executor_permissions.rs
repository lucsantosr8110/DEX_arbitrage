// ============================================================
// src/utils/check_executor_permissions.rs — (Corrigido para E0599)
// ============================================================

use crate::contracts::FlashloanExecutor; // CORRIGIDO: Importa diretamente o struct
use crate::AppMiddleware;
use anyhow::{anyhow, Context, Result};
use ethers::prelude::*;
use std::sync::Arc;
use tracing::{info, warn};

// Endereço do contrato Executor — mantido como referência; a função recebe o endereço via parâmetro.
#[allow(dead_code)]
const EXECUTOR_ADDRESS: &str = "0xc9bF35C5fF835aF08d1cc48dF114Af0e0D6b6B33";

pub async fn check_permissions(
    middleware: Arc<AppMiddleware>,
    executor_address: Address,
) -> Result<()> {
    // 1. Verificar se o executor existe e é um contrato
    let code = middleware
        .get_code(executor_address, None)
        .await
        .context("Falha ao obter o código do executor. Verifique a conexão RPC ou o endereço.")?;

    if code.is_empty() {
        return Err(anyhow!(
            "O endereço do executor ({:?}) não contém código de contrato. Verifique o endereço no config.toml",
            executor_address
        ));
    }

    // 2. Tenta interagir com o contrato (para verificar se o ABI está correto)
    let exec = FlashloanExecutor::new(executor_address, middleware.clone());

    info!(
        "🔬 Verificando permissões e estado do Executor {:?}...",
        executor_address
    );

    // Tentativa de chamar uma função 'view' simples para confirmar que o ABI é válido.
    // O seu ABI (FlashloanExecutorV4_4_2.json) não tem 'transfer_start_time'.
    // Usaremos 'paused' para o teste.
    let is_paused = exec.paused().call().await.context(
        "Falha ao chamar a função 'paused' do Executor. Verifique se o ABI (FlashloanExecutorV4_4_2.json) está correto."
    )?;

    if is_paused {
        warn!("⚠️ O Executor está no estado PAUSED. O bot não poderá executar transações.");
    }

    // Tentativa de chamar outra função que não existe no ABI
    // let _ = exec.transfer_start_time().call().await.ok(); // Comentado para corrigir E0599

    // 3. Verifica se o owner é o bot ou um endereço conhecido (opcional)
    let contract_owner = exec.owner().call().await.context(
        "Falha ao obter o owner do Executor. Verifique se a função 'owner' está no ABI.",
    )?;

    if contract_owner != middleware.address() {
        info!(
            "🔑 Owner do Contrato: {:?} | Bot Address: {:?}",
            contract_owner,
            middleware.address()
        );
        warn!("⚠️ O Owner do Executor não corresponde ao endereço da sua carteira. As transações devem falhar a menos que a permissão de 'executor' tenha sido dada a {:?}.", middleware.address());
    } else {
        info!("✅ Endereço da carteira do Bot é o Owner do Executor.");
    }

    info!("✅ Verificação de permissões do Executor concluída.");

    Ok(())
}
