// ============================================================
// src/infra/rpc_provider.rs — v4.0.0 (Runtime HTTP endpoint rotation)
// ============================================================
//
// ✅ HTTP com rotacao/failover em runtime via RotatingHttpClient
// ✅ WebSocket com fallback linear e timeout defensivo
// ✅ Detecta erros 429, timeout e desconexões
// ✅ Métricas: rpc_rate_limit, rpc_timeout, rpc_failover_count
// ✅ Compatível com AppMiddleware e DexManager
// ============================================================

use anyhow::{anyhow, Context, Result};
use dotenvy::from_path;
use ethers::{
    middleware::{Middleware, SignerMiddleware},
    providers::{Provider, Ws},
    signers::{LocalWallet, Signer},
};
use std::{
    sync::Arc,
    time::Duration,
};
use tokio::time::{sleep, Instant};
// ⚠️ 'error' removido pois não estava sendo usado
use tracing::{info, warn};

use crate::{config::NetworkConfig, infra::metrics, infra::rotating_http_client::RotatingHttpClient, AppMiddleware};

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
    // 🌐 HTTP simples (um endpoint ou lista do config)
    // ============================================================
    pub async fn connect_http(cfg: &NetworkConfig, private_key: &str) -> Result<Arc<AppMiddleware>> {
        if let Err(e) = from_path(".env") {
            warn!("⚠️ .env não encontrado: {:?}", e);
        }

        let endpoints: Vec<String> = if is_usable_endpoint(&cfg.rpc_url) {
            vec![cfg.rpc_url.clone()]
        } else if let Some(list) = &cfg.rpc_endpoints {
            list.iter().filter(|u| is_usable_endpoint(u)).cloned().collect()
        } else {
            return Err(anyhow!("❌ Nenhum RPC configurado no bloco [network]"));
        };

        if endpoints.is_empty() {
            return Err(anyhow!("❌ Nenhum endpoint RPC utilizável no config.toml"));
        }

        Self::build_http_client(cfg, private_key, &endpoints, "connect_http").await
    }

    // ============================================================
    // 🔁 HTTP com failover/rotacao em runtime
    // ============================================================
    pub async fn connect_http_with_fallback(
        cfg: &NetworkConfig,
        private_key: &str,
        endpoints: &[String],
    ) -> Result<Arc<AppMiddleware>> {
        let usable: Vec<String> = endpoints.iter().filter(|u| is_usable_endpoint(u)).cloned().collect();
        if usable.is_empty() {
            return Err(anyhow!("❌ Nenhum endpoint RPC utilizável para fallback"));
        }

        info!(
            target: "rpc_provider",
            "{} | 🌐 HTTP fallback com {} endpoint(s) utilizável(eis)",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"),
            usable.len()
        );

        Self::build_http_client(cfg, private_key, &usable, "connect_http_with_fallback").await
    }

    // ============================================================
    // 🔗 Constroi AppMiddleware com RotatingHttpClient
    // ============================================================
    async fn build_http_client(
        cfg: &NetworkConfig,
        private_key: &str,
        endpoints: &[String],
        source: &str,
    ) -> Result<Arc<AppMiddleware>> {
        let start = Instant::now();
        let rpc_timeout = Duration::from_millis(cfg.timeout_ms.max(1000));

        let transport = RotatingHttpClient::from_strings(
            &endpoints.to_vec(),
            rpc_timeout,
        )
        .with_context(|| format!("❌ Falha ao construir RotatingHttpClient a partir de {}", source))?;

        let provider = Provider::new(transport)
            .interval(Duration::from_millis(cfg.timeout_ms.max(1000)));

        // Sanity-check: pelo menos um endpoint deve responder chain_id.
        let chain_id_res = tokio::time::timeout(
            Duration::from_secs(5),
            provider.get_chainid(),
        )
        .await;
        let chain_id = match chain_id_res {
            Ok(Ok(id)) => id,
            Ok(Err(e)) => {
                return Err(anyhow!(
                    "❌ Falha ao obter chain_id do RotatingHttpClient: {:?}",
                    e
                ))
            }
            Err(_) => return Err(anyhow!("⏱️ Timeout ao obter chain_id do RotatingHttpClient")),
        };

        let wallet: LocalWallet = private_key.parse()?;
        let wallet = wallet.with_chain_id(chain_id.as_u64());
        let client = Arc::new(SignerMiddleware::new(Arc::new(provider), wallet));

        let elapsed = start.elapsed().as_millis();
        info!(
            target: "rpc_provider",
            "{} | 🌐 HTTP conectado (chain_id={}, {}ms, endpoints={})",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"),
            chain_id,
            elapsed,
            endpoints.len()
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
        cfg: &NetworkConfig,
        endpoints: &[String],
    ) -> Result<Arc<Provider<Ws>>> {
        if endpoints.is_empty() {
            return Err(anyhow!("❌ Nenhum WebSocket disponível após fallback."));
        }

        // Timeout defensivo para conexão + chain_id: evita hang em WS
        // half-open / rate-limit que não fecha o handshake.
        let handshake_timeout = Duration::from_millis(cfg.timeout_ms.max(3000));

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

            match tokio::time::timeout(handshake_timeout, Provider::<Ws>::connect(url.clone())).await {
                Ok(Ok(provider)) => {
                    let chain_id = match tokio::time::timeout(
                        Duration::from_secs(5),
                        provider.get_chainid(),
                    )
                    .await
                    {
                        Ok(Ok(id)) => id,
                        Ok(Err(e)) => {
                            warn!("⚠️ WS {} conectou mas chain_id falhou: {:?}", url, e);
                            metrics::observe_exec_latency_ms(
                                start_t.elapsed().as_millis() as f64,
                                "rpc_ws_fallback_fail",
                            );
                            sleep(Duration::from_millis(300)).await;
                            continue;
                        }
                        Err(_) => {
                            warn!("⏱️ WS {} conectou mas chain_id timeout", url);
                            metrics::observe_exec_latency_ms(
                                start_t.elapsed().as_millis() as f64,
                                "rpc_ws_fallback_fail",
                            );
                            sleep(Duration::from_millis(300)).await;
                            continue;
                        }
                    };
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
                Ok(Err(e)) => {
                    warn!("⚠️ Falha ao conectar WS em {}: {:?}", url, e);
                    metrics::observe_exec_latency_ms(
                        start_t.elapsed().as_millis() as f64,
                        "rpc_ws_fallback_fail",
                    );
                    sleep(Duration::from_millis(300)).await;
                }
                Err(_) => {
                    warn!(
                        "⏱️ Timeout ao conectar WS em {} (>{}ms) — próximo endpoint.",
                        url, handshake_timeout.as_millis()
                    );
                    metrics::observe_exec_latency_ms(
                        start_t.elapsed().as_millis() as f64,
                        "rpc_ws_fallback_timeout",
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