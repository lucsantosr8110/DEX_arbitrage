// ============================================================
// src/infra/rpc_provider.rs — v3.9.9 (Produção — Fallback + Rate Limit Protection)
// ============================================================
//
// ✅ Multi-endpoint HTTP + WS (Alchemy → Infura → Público)
// ✅ Detecta erros 429, timeout e desconexões
// ✅ Backoff exponencial com throttle automático
// ✅ Métricas: rpc_rate_limit, rpc_timeout, rpc_failover_count
// ✅ Compatível com AppMiddleware e DexManager
// ============================================================

use anyhow::{anyhow, Result};
use dotenvy::from_path;
use ethers::{
    middleware::{Middleware, SignerMiddleware},
    providers::{Http, Provider, Ws},
    signers::{LocalWallet, Signer},
};
use std::{
    sync::Arc,
    time::Duration,
};
use tokio::time::{sleep, Instant};
// ⚠️ 'error' removido pois não estava sendo usado
use tracing::{info, warn}; 

use crate::{config::NetworkConfig, infra::metrics, AppMiddleware};

pub struct RpcProvider;

/// Um endpoint é inutilizável se estiver vazio ou se ainda for um placeholder
/// `${VAR}` — o expansor de `Config::from_file` mantém o literal quando a variável
/// não existe no `.env`. Pular aqui evita uma tentativa de conexão garantidamente
/// perdida (~300 ms) a cada boot de quem não usa provedor privado.
pub fn is_usable_endpoint(url: &str) -> bool {
    let u = url.trim();
    !u.is_empty() && !u.starts_with("${")
}

impl RpcProvider {
    // ============================================================
    // 🌐 HTTP simples
    // ============================================================
    pub async fn connect_http(cfg: &NetworkConfig, private_key: &str) -> Result<Arc<AppMiddleware>> {
        if let Err(e) = from_path(".env") {
            warn!("⚠️ .env não encontrado: {:?}", e);
        }

        let rpc_url = if is_usable_endpoint(&cfg.rpc_url) {
            cfg.rpc_url.clone()
        } else if let Some(list) = &cfg.rpc_endpoints {
            list.iter()
                .find(|u| is_usable_endpoint(u))
                .ok_or_else(|| anyhow!("❌ Nenhum endpoint RPC utilizável no config.toml"))?
                .to_string()
        } else {
            return Err(anyhow!("❌ Nenhum RPC configurado no bloco [network]"));
        };

        Self::connect_single(&rpc_url, cfg, private_key).await
    }

    // ============================================================
    // 🔁 HTTP com fallback inteligente
    // ============================================================
    pub async fn connect_http_with_fallback(
        cfg: &NetworkConfig,
        private_key: &str,
        endpoints: &[String],
    ) -> Result<Arc<AppMiddleware>> {
        let mut last_err: Option<anyhow::Error> = None;
        let mut backoff = Duration::from_millis(500);

        for (i, url) in endpoints.iter().enumerate() {
            if !is_usable_endpoint(url) {
                continue;
            }

            let start = Instant::now();
            info!(
                target: "rpc_provider",
                "{} | 🌐 Tentando RPC[{}]: {}",
                chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"),
                i + 1,
                url
            );

            match Self::connect_single(url, cfg, private_key).await {
                Ok(client) => {
                    let elapsed = start.elapsed().as_millis();
                    info!(
                        target: "rpc_provider",
                        "{} | ✅ RPC conectado com sucesso: {} ({}ms)",
                        chrono::Utc::now().format("%Y-m-%d %H:%M:%S"),
                        url,
                        elapsed
                    );
                    metrics::observe_exec_latency_ms(elapsed as f64, "rpc_http_fallback_ok");
                    return Ok(client);
                }
                Err(e) => {
                    let msg = e.to_string().to_lowercase();

                    // 🚫 Trata limites e timeouts
                    if msg.contains("429") || msg.contains("too many requests") {
                        warn!("🚫 RPC {} bloqueado por rate-limit (429). Aplicando cooldown.", url);
                        metrics::inc_counter("rpc_rate_limit");
                        sleep(backoff).await;
                        backoff *= 2;
                    } else if msg.contains("timeout") || msg.contains("deadline") {
                        warn!("⏱️ Timeout em {}. Tentando próximo endpoint…", url);
                        metrics::inc_counter("rpc_timeout");
                    } else {
                        warn!("⚠️ Falha geral em {}: {}", url, e);
                    }

                    metrics::observe_exec_latency_ms(
                        start.elapsed().as_millis() as f64,
                        "rpc_http_fallback_fail",
                    );

                    last_err = Some(e);
                    metrics::inc_counter("rpc_failover_count");
                    sleep(Duration::from_millis(300)).await;
                }
            }
        }

        Err(anyhow!(
            "❌ Nenhum RPC HTTP disponível após fallback. Último erro: {:?}",
            last_err
        ))
    }

    // ============================================================
    // 🔗 Conecta um endpoint HTTP (com carteira)
    // ============================================================
    async fn connect_single(
        rpc_url: &str,
        cfg: &NetworkConfig,
        private_key: &str,
    ) -> Result<Arc<AppMiddleware>> {
        let start = Instant::now();
        let provider = Provider::<Http>::try_from(rpc_url.to_string())?
            .interval(Duration::from_millis(cfg.timeout_ms.max(1000)));

        // Tenta obter chain_id com timeout manual
        let chain_id_res = tokio::time::timeout(Duration::from_secs(5), provider.get_chainid()).await;
        let chain_id = match chain_id_res {
            Ok(Ok(id)) => id,
            Ok(Err(e)) => return Err(anyhow!("❌ Falha ao obter chain_id de {}: {:?}", rpc_url, e)),
            Err(_) => return Err(anyhow!("⏱️ Timeout ao obter chain_id de {}", rpc_url)),
        };

        let wallet: LocalWallet = private_key.parse()?;
        let wallet = wallet.with_chain_id(chain_id.as_u64());
        let client = Arc::new(SignerMiddleware::new(Arc::new(provider), wallet));

        let elapsed = start.elapsed().as_millis();
        info!(
            target: "rpc_provider",
            "{} | 🌐 HTTP conectado: {} ({}ms, chain_id={})",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"),
            rpc_url,
            elapsed,
            chain_id
        );
        metrics::observe_exec_latency_ms(elapsed as f64, "rpc_http_connect");

        Ok(client)
    }

    // ============================================================
    // 📡 WS (Wrapper Principal) - ESTA FUNÇÃO ESTAVA FALTANDO
    // ============================================================
    pub async fn connect_ws(
        cfg: &NetworkConfig,
    ) -> Result<Arc<Provider<Ws>>> {
        info!(
            target: "rpc_provider",
            "{} | 📡 Iniciando conexão WebSocket com fallback...",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S")
        );

        // Pega os endpoints WS da configuração
        let endpoints = cfg.ws_endpoints
            .as_deref() // Converte Option<Vec<String>> para Option<&[String]>
            .unwrap_or_else(|| {
                warn!("⚠️ Nenhum endpoint 'ws_endpoints' definido, usando array vazio.");
                &[] // Retorna um slice vazio se for None
            });

        if endpoints.is_empty() {
            return Err(anyhow!("❌ Nenhum endpoint WebSocket (ws_endpoints) configurado no config.toml"));
        }

        // Chama a função de fallback existente com os endpoints
        // ⚠️ 'cfg' foi prefixado com '_' pois não estava sendo usado aqui
        Self::connect_ws_with_fallback(cfg, endpoints).await
    }


    // ============================================================
    // 📡 WS com fallback
    // ============================================================
    pub async fn connect_ws_with_fallback(
        _cfg: &NetworkConfig, // ⚠️ 'cfg' não estava sendo usado, adicionado '_'
        endpoints: &[String],
    ) -> Result<Arc<Provider<Ws>>> {
        if endpoints.is_empty() {
            return Err(anyhow!("❌ Nenhum WebSocket disponível após fallback."));
        }

        // Tentativa linear na ordem exata da lista. Sem round-robin —
        // o primeiro endpoint que funcionar vence. Se o topo da lista
        // (ex: Alchemy) cair, o failover tenta o próximo (QuickNode) e
        // assim sucessivamente. Na reconexão, a ordem é a mesma — se o
        // primeiro recuperou, ele vence de novo; se ainda estiver fora,
        // o failover avança na fila novamente.
        for (i, url) in endpoints.iter().enumerate() {
            if !is_usable_endpoint(url) {
                continue;
            }

            let start_t = Instant::now();
            info!(
                target: "rpc_provider",
                "{} | 🔁 Tentando WS[{}]: {}",
                chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"),
                i + 1,
                url
            );

            match Provider::<Ws>::connect(url.clone()).await {
                Ok(provider) => {
                    let chain_id = provider.get_chainid().await.unwrap_or_default();
                    let elapsed = start_t.elapsed().as_millis();
                    info!(
                        target: "rpc_provider",
                        "{} | 📡 WS conectado (chain_id={}, {}ms)",
                        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"),
                        chain_id,
                        elapsed
                    );
                    metrics::observe_exec_latency_ms(elapsed as f64, "rpc_ws_fallback_ok");
                    return Ok(Arc::new(provider));
                }
                Err(e) => {
                    warn!("⚠️ Falha ao conectar WS em {}: {:?}", url, e);
                    metrics::observe_exec_latency_ms(
                        start_t.elapsed().as_millis() as f64,
                        "rpc_ws_fallback_fail",
                    );
                    sleep(Duration::from_millis(300)).await;
                }
            }
        }

        Err(anyhow!("❌ Nenhum WebSocket disponível após fallback."))
    }
}

// ============================================================
// 🔒 Enum seguro de conexões RPC
// ============================================================
pub enum RpcConnection {
    Http(Arc<AppMiddleware>),
    WebSocket(Arc<Provider<Ws>>),
}