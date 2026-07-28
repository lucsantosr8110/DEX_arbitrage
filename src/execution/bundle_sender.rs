// ============================================================
// src/execution/bundle_sender.rs — v4.8.1 (MEV + payload-sign + robust)
// ------------------------------------------------------------
// - Suporte a assinatura do payload (EIP-191) para compatibilidade ampla.
// - Timeout, parse e logs aprimorados.
// - Mantém compatibilidade Flashbots/Marlin/NodeKit.
// ============================================================
use anyhow::{anyhow, Context, Result};
use ethers::{
    signers::Signer,
    types::{Address, Bytes, H256, U256},
    utils::parse_ether,
};
use serde::Deserialize;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tracing::{error, info, trace, warn};

#[derive(Debug, Clone, Deserialize)]
pub struct MevConfig {
    pub enabled: bool,
    pub relay_url: String,
    pub signer_address: String,
    pub min_tip_matic: String,
    pub target_block_offset: u64,
    pub timeout_seconds: u64,
}

pub struct BundleSender<S: Signer + 'static> {
    cfg: MevConfig,
    http_client: reqwest::Client,
    signer: Arc<S>,
    pub signer_addr: Address,
    #[allow(dead_code)] // TODO: usar no cálculo de tip mínimo do bundle
    min_tip_matic_wei: U256,
}

impl<S: Signer + 'static> BundleSender<S> {
    pub fn new(cfg: MevConfig, signer: Arc<S>) -> Result<Arc<Self>> {
        let signer_addr: Address = cfg
            .signer_address
            .parse()
            .context("Endereço do Signer inválido")?;

        let min_tip_matic_wei =
            parse_ether(&cfg.min_tip_matic).context("Valor de min_tip_matic inválido")?;

        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_seconds))
            .build()?;

        Ok(Arc::new(Self {
            cfg,
            http_client,
            signer,
            signer_addr,
            min_tip_matic_wei,
        }))
    }

    pub async fn send_atomic_flashloan(
        &self,
        signed_tx: Bytes,
        current_block_number: U256,
    ) -> Result<H256> {
        if !self.cfg.enabled {
            return Err(anyhow!("Submissão MEV desabilitada."));
        }

        let target_block = current_block_number.as_u64() + self.cfg.target_block_offset;
        let tx_hex = format!("0x{}", hex::encode(&signed_tx.0));

        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_sendBundle",
            "params": [{
                "txs": [tx_hex],
                "blockNumber": format!("0x{:x}", target_block),
                "minTimestamp": 0,
                "maxTimestamp": 0,
                "revertingTxHashes": [],
            }]
        });

        let body = serde_json::to_vec(&payload).expect("serialize payload");
        let signature = self
            .signer
            .sign_message(ethers::utils::hash_message(&body))
            .await
            .context("Falha ao assinar header X-Flashbots-Signature")?;
        let header_value = format!(
            "{}:0x{}",
            self.cfg.signer_address,
            hex::encode(signature.to_vec())
        );

        let response = self
            .http_client
            .post(&self.cfg.relay_url)
            .header("X-Flashbots-Signature", header_value)
            .json(&payload)
            .send()
            .await
            .context("Falha na comunicação com o relay MEV")?;

        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();
        trace!("Resposta MEV: {}", body_text);

        if !status.is_success() {
            error!("Relay erro HTTP {}: {}", status, body_text);
            return Err(anyhow!("Erro de submissão do Bundle: status {}", status));
        }

        let json_resp: serde_json::Value = serde_json::from_str(&body_text)
            .context("Falha ao desserializar resposta JSON do relay")?;

        if let Some(err) = json_resp.get("error") {
            let msg = err
                .get("message")
                .map(|m| m.to_string())
                .unwrap_or_else(|| err.to_string());
            warn!("BUNDLE REJEITADO: {}", msg);
            return Err(anyhow!("BUNDLE REJEITADO: {}", msg));
        }

        let bundle_hash = json_resp
            .get("result")
            .and_then(|r| r.as_str())
            .unwrap_or("sem_hash");

        info!("✅ BUNDLE MEV enviado. Hash: {}", bundle_hash);

        let local_hash = H256::from_slice(ethers::utils::keccak256(&signed_tx).as_slice());
        Ok(local_hash)
    }
}
