// ============================================================
// src/infra/rotating_http_client.rs
// ============================================================
// Cliente JSON-RPC HTTP com failover/rotacao de endpoints em runtime.
// Cada chamada comeca pelo endpoint atual e, se receber rate-limit (429),
// timeout ou erro de conexao, avanca para o proximo da lista. Isso evita
// que o bot fique pendurado quando um provedor com API key (Alchemy,
// QuickNode, Infura) bate o limite, sem precisar reiniciar o processo.
// ============================================================

use async_trait::async_trait;
use ethers::providers::{JsonRpcClient, JsonRpcError, ProviderError, RpcError};
use reqwest::{Client, Error as ReqwestError, StatusCode};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use std::{
    fmt::Debug,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};
use thiserror::Error;
use tokio::time::timeout;
use tracing::{info, warn};

/// Erros do cliente HTTP rotativo.
#[derive(Error, Debug)]
pub enum RotatingClientError {
    #[error("reqwest error: {0}")]
    Reqwest(#[from] ReqwestError),

    #[error("JSON-RPC error: {0:?}")]
    JsonRpc(JsonRpcError),

    #[error("deserialization error: {err}. response: {text}")]
    Serde {
        err: serde_json::Error,
        text: String,
    },

    #[error("all endpoints exhausted. last: {0}")]
    Exhausted(String),
}

impl From<RotatingClientError> for ProviderError {
    fn from(src: RotatingClientError) -> Self {
        match src {
            RotatingClientError::Reqwest(err) => ProviderError::HTTPError(err),
            _ => ProviderError::JsonRpcClientError(Box::new(src)),
        }
    }
}

impl RpcError for RotatingClientError {
    fn as_error_response(&self) -> Option<&JsonRpcError> {
        if let RotatingClientError::JsonRpc(err) = self {
            Some(err)
        } else {
            None
        }
    }

    fn as_serde_error(&self) -> Option<&serde_json::Error> {
        if let RotatingClientError::Serde { err, .. } = self {
            Some(err)
        } else {
            None
        }
    }
}

/// Resposta JSON-RPC generica usada para decidir entre resultado/erro.
#[derive(Debug, serde::Deserialize)]
struct JsonRpcResponse<R> {
    jsonrpc: Option<String>,
    id: Option<Value>,
    result: Option<R>,
    error: Option<JsonRpcError>,
}

/// Cliente HTTP que rotaciona endpoints a cada falha de transporte/rate-limit.
#[derive(Debug)]
pub struct RotatingHttpClient {
    endpoints: Vec<url::Url>,
    client: Client,
    /// Indice do proximo endpoint a tentar. Avancado atomicamente para
    /// distribuir carga e escapar rapidamente de provedores com problema.
    current: AtomicUsize,
    request_timeout: Duration,
    id: AtomicU64,
}

impl RotatingHttpClient {
    pub fn new(endpoints: Vec<url::Url>, request_timeout: Duration) -> Result<Self, ReqwestError> {
        if endpoints.is_empty() {
            // Nao ha endpoints; o primeiro request falha de forma limpa.
            return Ok(Self {
                endpoints,
                client: Client::builder().timeout(request_timeout).build()?,
                current: AtomicUsize::new(0),
                request_timeout,
                id: AtomicU64::new(1),
            });
        }

        let client = Client::builder().timeout(request_timeout).build()?;
        Ok(Self {
            endpoints,
            client,
            current: AtomicUsize::new(0),
            request_timeout,
            id: AtomicU64::new(1),
        })
    }

    /// Cria a partir de strings de endpoint. Endpoints vazios/invalidos sao ignorados.
    pub fn from_strings(
        endpoints: &[String],
        request_timeout: Duration,
    ) -> Result<Self, anyhow::Error> {
        let mut urls = Vec::new();
        for s in endpoints {
            if s.trim().is_empty() || s.starts_with("${") {
                continue;
            }
            match s.parse::<url::Url>() {
                Ok(u) => urls.push(u),
                Err(e) => warn!("ignorando endpoint RPC invalido {}: {}", s, e),
            }
        }
        if urls.is_empty() {
            anyhow::bail!("nenhum endpoint HTTP utilizavel para RotatingHttpClient");
        }
        Ok(Self::new(urls, request_timeout).map_err(|e| anyhow::anyhow!(e))?)
    }

    fn is_rate_limit(msg: &str) -> bool {
        let m = msg.to_lowercase();
        m.contains("429")
            || m.contains("rate limit")
            || m.contains("too many requests")
            || m.contains("capacity")
            || m.contains("exceeded")
            || m.contains("limit reached")
            || m.contains("request limit")
    }

    /// Erro do PROVEDOR (auth/capacidade/tenant) — recuperável rotacionando
    /// para o próximo endpoint. Diferente de rate-limit puro: cobre chaves
    /// desativadas, tenant desabilitado, 403/401, erros JSON-RPC de provider
    /// (-32051 etc.). Antes só 429/rate-limit rotacionavam; um endpoint paid
    /// com "API key disabled, tenant disabled, -32051, 403" devolvia um body
    /// `{"error":"string"}` (shape não-padrão) que falhava a desserialização
    /// como struct → Serde error NÃO-recuperável → retorno imediato sem
    /// rotacionar → `current` travava naquele endpoint e TODA chamada seguinte
    /// morria no mesmo lugar (loop de ciclos falhando pra sempre, radar nunca
    /// recuperava). Tratar como recuperável aqui faz o cliente avançar.
    fn is_provider_error(msg: &str) -> bool {
        let m = msg.to_lowercase();
        m.contains("api key")
            || m.contains("api-key")
            || m.contains("disabled")
            || m.contains("tenant")
            || m.contains("unauthorized")
            || m.contains("forbidden")
            || m.contains("not allowed")
            || m.contains("invalid api")
            || m.contains("access denied")
            || m.contains("-32051")
            || m.contains("-32000")
            || m.contains("-32001")
            || m.contains("-32002")
            || m.contains("-32003")
            || m.contains("-32004")
            || m.contains("-32005")
            || m.contains("-32042")
            || m.contains("-32043")
            || m.contains("rate limit")
            || m.contains("429")
            || m.contains("too many requests")
            || m.contains("capacity")
            || m.contains("exceeded")
    }

    /// Status HTTP que indica problema do provedor (rotacionar, não hard-fail).
    fn is_recoverable_status(status: StatusCode) -> bool {
        matches!(
            status,
            StatusCode::TOO_MANY_REQUESTS
                | StatusCode::UNAUTHORIZED
                | StatusCode::FORBIDDEN
                | StatusCode::BAD_GATEWAY
                | StatusCode::SERVICE_UNAVAILABLE
                | StatusCode::GATEWAY_TIMEOUT
        )
    }

    fn is_recoverable_transport(err: &ReqwestError) -> bool {
        err.is_timeout()
            || err.is_connect()
            || err.is_request()
            || err.status() == Some(StatusCode::TOO_MANY_REQUESTS)
            || err.status() == Some(StatusCode::SERVICE_UNAVAILABLE)
            || err.status() == Some(StatusCode::BAD_GATEWAY)
            || err.status() == Some(StatusCode::GATEWAY_TIMEOUT)
    }
}

#[async_trait]
impl JsonRpcClient for RotatingHttpClient {
    type Error = RotatingClientError;

    async fn request<T: Debug + Serialize + Send + Sync, R: DeserializeOwned + Send>(
        &self,
        method: &str,
        params: T,
    ) -> Result<R, Self::Error> {
        if self.endpoints.is_empty() {
            return Err(RotatingClientError::Exhausted(
                "nenhum endpoint configurado".into(),
            ));
        }

        let n = self.endpoints.len();
        let start_idx = self.current.load(Ordering::Relaxed);
        let id = self.id.fetch_add(1, Ordering::SeqCst);
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": id,
        });

        let mut last_err: Option<String> = None;

        for offset in 0..n {
            let idx = (start_idx + offset) % n;
            let url = &self.endpoints[idx];

            let fut = self.client.post(url.as_ref()).json(&payload).send();

            let res = match timeout(self.request_timeout, fut).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    let msg = e.to_string();
                    if Self::is_recoverable_transport(&e) {
                        warn!(
                            "RPC[{}] falha de transporte ({}) — rotacionando...",
                            idx, msg
                        );
                        self.current.store((idx + 1) % n, Ordering::Relaxed);
                        last_err = Some(msg);
                        continue;
                    }
                    return Err(RotatingClientError::Reqwest(e));
                }
                Err(_) => {
                    warn!(
                        "RPC[{}] timeout externo (>{}ms) — rotacionando...",
                        idx,
                        self.request_timeout.as_millis()
                    );
                    self.current.store((idx + 1) % n, Ordering::Relaxed);
                    last_err = Some(format!("timeout externo em {}", url));
                    continue;
                }
            };

            let status = res.status();
            let body = match res.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    if Self::is_recoverable_transport(&e) || status.is_server_error() {
                        warn!(
                            "RPC[{}] erro ao ler resposta ({}) — rotacionando...",
                            idx, e
                        );
                        self.current.store((idx + 1) % n, Ordering::Relaxed);
                        last_err = Some(e.to_string());
                        continue;
                    }
                    return Err(RotatingClientError::Reqwest(e));
                }
            };

            // Status de provedor (429/401/403/5xx de gateway) → rotaciona.
            // Antes só 429 rotacionava; 403 "API key disabled" ficava preso.
            if Self::is_recoverable_status(status) {
                warn!(
                    "RPC[{}] HTTP {} — erro de provedor, rotacionando...",
                    idx,
                    status.as_u16()
                );
                self.current.store((idx + 1) % n, Ordering::Relaxed);
                last_err = Some(format!("HTTP {}", status.as_u16()));
                continue;
            }

            let text = String::from_utf8_lossy(&body);
            let parsed: JsonRpcResponse<Value> = match serde_json::from_slice(&body) {
                Ok(r) => r,
                Err(err) => {
                    // Body não desserializa como struct JsonRpcResponse. Dois
                    // casos: (a) provedor devolveu erro em shape não-padrão
                    // (ex.: `{"error":"API key disabled, tenant disabled,
                    // -32051, 403"}` — error como STRING) → recuperável,
                    // rotaciona; (b) bug real de protocolo → hard-fail.
                    if Self::is_provider_error(&text) {
                        warn!(
                            "RPC[{}] resposta de erro não-padrão ({} bytes) — rotacionando...",
                            idx,
                            body.len()
                        );
                        self.current.store((idx + 1) % n, Ordering::Relaxed);
                        last_err = Some(text.chars().take(200).collect());
                        continue;
                    }
                    return Err(RotatingClientError::Serde {
                        err,
                        text: text.into_owned(),
                    });
                }
            };

            if let Some(err) = parsed.error {
                let msg = format!("{:?}", err);
                // Rate-limit OU erro de provedor (auth/tenant/capacidade) →
                // rotaciona. Erro de método (revert, insufficient funds, etc.)
                // é genuíno e falha igual em todos os endpoints → hard-fail.
                if Self::is_rate_limit(&msg) || Self::is_provider_error(&msg) {
                    warn!(
                        "RPC[{}] erro JSON-RPC de provedor ({}) — rotacionando...",
                        idx, msg
                    );
                    self.current.store((idx + 1) % n, Ordering::Relaxed);
                    last_err = Some(msg);
                    continue;
                }
                return Err(RotatingClientError::JsonRpc(err));
            }

            if let Some(result) = parsed.result {
                let r =
                    serde_json::from_value(result).map_err(|err| RotatingClientError::Serde {
                        err,
                        text: text.into_owned(),
                    })?;
                return Ok(r);
            }

            // Resposta inesperada (sem result nem error): trata como erro nao-recuperavel.
            return Err(RotatingClientError::Serde {
                err: serde::de::Error::custom("resposta JSON-RPC sem result nem error"),
                text: text.into_owned(),
            });
        }

        Err(RotatingClientError::Exhausted(
            last_err.unwrap_or_else(|| "nenhum endpoint respondeu".into()),
        ))
    }
}
