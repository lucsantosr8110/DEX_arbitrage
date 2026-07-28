# Plano: Evitar TUI freeze via timeout em RPC e ciclo do radar + fallback

## Problema
A TUI carregou e travou após o ciclo 15. O log mostra que o último scan foi `scan=15` e depois houve silêncio até o shutdown forçado. A causa é rate-limit do QuickNode (`15/second request limit reached`), que fez chamadas RPC ficarem presas em `.await` sem timeout. O `select!` do radar não conseguia verificar shutdown ou timeout de bloco enquanto o ciclo inteiro estava pendurado.

## Objetivo
Adicionar timeout defensivo em todas as chamadas RPC síncronas que podem congelar, e garantir que o radar retorne `Err` quando o ciclo travar, para que `main.rs` faça failover/reconexão automática.

## Arquivos a alterar

### 1. `src/infra/rpc_provider.rs`
- `connect_single` HTTP deve criar `reqwest::Client` com timeout igual a `cfg.timeout_ms` (ou fallback sensato) e usar `Http::new_with_client(url, client)` em vez de `Provider::<Http>::try_from`.
- Isso evita que qualquer chamada HTTP subjacente fique pendurada indefinidamente quando o provedor não responde (rate-limit, socket half-open).

### 2. `src/core/gas.rs`
- `latest_base_fee`: envolver `client.get_block(...).await` em `tokio::time::timeout(Duration::from_secs(...))`.
  - Timeout = `cfg.timeout_ms` convertido para segundos, mínimo 5s.
  - Em timeout: `warn!`, métrica `rpc_call_timeout`, retornar `Ok(None)` para fallback ao cache/default.
- `fetch_polygon_oracle_ttl`: envolver `self.http.get(&url).send().await` em timeout (já existe timeout de 3s no reqwest, mas adicionar timeout tokio externo para consistência).
  - Em timeout/erro: warn + `Ok(None)`, usar fallback RPC.

### 3. `src/core/flashloan.rs`
- `current_flashloan_fee_pct`: envolver `call().await` em `tokio::time::timeout`.
  - Timeout = 10s.
  - Em timeout/erro: warn + métrica `rpc_call_timeout`, retornar `fallback_pct`.
- Garantir que cache TTL continue funcionando; se on-chain falhar, não tenta a cada ciclo.

### 4. `src/dex/radar.rs`
- `start_high_hit_rate_radar`: envolver `execute_radar_cycle(...).await` em `tokio::time::timeout`.
  - Timeout = `BLOCK_TIMEOUT` (20s) ou um pouco menos, ex: 15s.
  - Se estourar: logar erro, enviar alerta Telegram, retornar `Err(anyhow!("radar cycle timeout"))`.
  - O loop externo em `main.rs` já reconecta WS no próximo endpoint quando `start_high_hit_rate_radar` retorna `Err`.
- Adicionar métrica `radar_cycle_timeout`.

### 5. `src/main.rs` (ajustes menores)
- `EXEC_TIMEOUT` de 60s no `bot_task` já existe; manter.
- Verificar se `shutdown_timeout` default de 180s é suficiente para o novo fluxo; manter configurável.
- Opcional: adicionar log quando radar inicia reconexão após timeout.

## Detalhes técnicos

### Padrão de timeout
Criar helper interno (se necessário) ou usar inline `tokio::time::timeout`. Preferir inline para manter clareza nos pontos críticos.

### Valores de timeout
- HTTP RPC geral: `cfg.timeout_ms` (default 2000ms). Usar `Duration::from_millis(cfg.timeout_ms.max(1000))`.
- Bloco/radar ciclo: 15s (menor que `BLOCK_TIMEOUT` de 20s).
- Chamadas on-chain pontuais (Aave fee): 10s.
- Oracle HTTP externo: manter 3s do reqwest + timeout tokio de 5s.

### Fallback
- Timeout/falha RPC pontual → retorna valor default/cache (não propaga erro fatal).
- Timeout no ciclo do radar → propaga `Err` para `main.rs`, que reconecta WS no próximo endpoint.
- Provider HTTP com timeout de reqwest → conexões não penduram; failover do boot já existe em `connect_http_with_fallback`.

## Testes / verificação
1. `cargo check` após alterações.
2. Rodar bot em modo paper e forçar desconexão/rede lenta (ex: limitar interface) para verificar que ciclo não trava por mais de 15-20s.
3. Verificar logs por mensagens de timeout e reconexão.

## Riscos
- Timeout muito agressivo em RPC lenta pode gerar falsos positivos. Default 2000ms é conservador para Polygon.
- Mudar `Provider::<Http>::try_from` para `Http::new_with_client` pode alterar comportamento de URL parsing. Verificar compatibilidade com URL atual.
