# Frontend Operator Console — plano de construção

## Objetivo

Construir um console web premium, responsivo e operacionalmente seguro, pareado com o runtime
canônico do bot. O navegador não recalcula preços, spreads, custos ou lucro: ele apresenta DTOs
produzidos pelo mesmo snapshot autoritativo que alimenta TUI, logs e gates de segurança.

O primeiro release é read-only e voltado a dry run. Controles mutáveis e qualquer caminho live
ficam bloqueados até existirem autenticação, auditoria e confirmação explícita no backend.

## Diagnóstico atual

- `src/frontend.rs` expõe `/status`, `/config` e preços mockados; não recebe o estado canônico.
- `src/api/mod.rs` é uma segunda API, desconectada, que lê e sobrescreve TOML diretamente.
- Nenhum dos dois servidores é iniciado por `main.rs`.
- `src/frontend/src/App.jsx` mostra apenas configuração e pode revelar RPC completo.
- O frontend não tem TypeScript, testes, lint, build ou contrato versionado.
- `src/frontend/index.html` aponta para uma entrada incorreta (`/frontend/main.jsx`).
- `general.enable_frontend` e `api.enabled` estão desligados nas configurações atuais.
- A fonte de dados útil já existe no runtime: `TuiState`, `CanonicalDiscoveryResult`,
  `RoundEvidence`, `DiscoveryStats`, eventos `CANONICAL_*` e métricas Prometheus.

## Princípios obrigatórios

1. Uma fonte de verdade: `OperatorSnapshot`, produzido no backend após cada rodada canônica.
2. Mesma economia: net, gross, custos e executabilidade vêm prontos do Rust.
3. Proveniência visível: anchor, block hash abreviado, idade, route key, pools e fee tiers.
4. Segurança fail-closed: UI nunca habilita execução; apenas mostra o estado dos gates.
5. Segredos nunca serializados: RPCs, chaves, tokens e valores de `.env` são redigidos.
6. Read-only por padrão; comandos separados de consultas e protegidos por política própria.
7. Valores financeiros acompanhados de unidade, precisão e estado `unknown/stale`, sem usar zero
   como substituto de dado ausente.
8. Snapshot versionado e monotônico (`schema_version`, `sequence`, `generated_at`).

## Arquitetura proposta

```text
Canonical discovery / gates / execution
                  |
                  v
        OperatorStatePublisher
       (Arc<RwLock<Snapshot>> + watch)
          |          |          |
          v          v          v
         TUI      REST/SSE   Prometheus
                    |
                    v
          React Operator Console
```

### Backend

- Substituir as APIs protótipo por um único módulo `src/operator_api/` em Axum.
- Extrair tipos de apresentação de `tui.rs` para `src/operator_state.rs`; TUI e API consomem os
  mesmos tipos, sem dependência da API na renderização terminal.
- Usar `tokio::sync::watch` para o snapshot atual e `broadcast` somente para eventos efêmeros.
- REST entrega estado inicial, detalhes e histórico paginado.
- SSE entrega atualizações unidirecionais (`snapshot`, `round`, `alert`, `shutdown`) com ID,
  keep-alive e retomada. WebSocket não é necessário no release read-only.
- Prometheus continua responsável por séries agregadas; o frontend não deve consultar nem
  interpretar logs de texto como API.
- Servir `dist/` pelo mesmo Axum em produção; Vite faz proxy de `/api` durante desenvolvimento.

### Frontend

- React 19 + Vite + TypeScript estrito.
- CSS variables e componentes próprios; biblioteca de ícones leve. Evitar framework visual que
  imponha aparência genérica.
- Store externa pequena para snapshot/SSE, conectada ao React por `useSyncExternalStore`.
- TanStack Query apenas para REST/histórico; não duplicar o snapshot SSE em outro cache.
- Recharts ou uPlot para séries pequenas; tabelas virtualizadas somente se o histórico exigir.
- Validação runtime dos DTOs com Zod gerado/espelhado do schema OpenAPI.

## Contrato API v1

Base: `/api/v1`. Todos os timestamps em UTC ISO-8601; valores atômicos como strings; valores de
apresentação incluem unidade explícita.

### Consultas

- `GET /health` — processo, API, worker canônico e idade do último snapshot.
- `GET /snapshot` — visão completa inicial.
- `GET /rounds?cursor=&limit=` — histórico resumido de rodadas.
- `GET /rounds/{sequence}` — detalhes, estatísticas e rejeições da rodada.
- `GET /routes/{route_id}` — pernas, DEX, pools, fee tier, amounts e economia.
- `GET /config/public` — somente configuração operacional redigida e allowlisted.
- `GET /metrics/summary?window=1h` — agregados prontos para cards e gráficos.
- `GET /events` — SSE.

### DTO principal

```text
OperatorSnapshot
  schema_version, sequence, generated_at
  runtime: mode, dry_run, phase, uptime, started_at, shutdown_state
  safety: signer_present, broadcaster_present, wrapper_enabled,
          simulate_before_execute, economics_consistent, mainnet_blocked
  chain: chain_id, network, head_block, anchor_block, anchor_hash, confirmations, data_age_ms
  universe: tokens[], venues[], quote_concurrency, route_limit, discovery_timeout_ms
  round: duration_ms, quotes, edges, cycles_detected, routes_ranked,
         routes_evaluated, gross_positive, economically_positive,
         stable, risk_approved, selected, timeouts
  prices: PriceQuoteView[]
  routes: RouteView[]
  rpc: RpcHealthView[]
  alerts: OperatorAlert[]
```

`RouteView` precisa distinguir `route_kind = two_leg | triangular` e expor:

- rota e pernas ordenadas;
- DEX, protocolo, pool e fee tier por perna;
- amount in/out por perna, cycle rate e anchor;
- gross USD/%, flashloan fee, gas estimado, outros buffers, net USD/%;
- distância para lucro, executável, estabilidade e decisão de cada gate;
- motivo preciso quando rejeitada;
- `authoritative = true` apenas para economia derivada de evidência sequencial canônica.

Rotas 2L diagnósticas devem carregar `authoritative = false` enquanto não participarem do gate
canônico. A UI apresenta essa diferença claramente e nunca mistura os contadores de segurança.

## Experiência do operador

### 1. Overview

- Barra superior fixa: Polygon, `DRY RUN`, worker, SSE, idade do dado e bloco anchor.
- KPIs: rodadas, melhor net, gross positivas, net positivas, latência p50/p95, RPC health.
- Waterfall da melhor rota: bruto → fee flashloan → gás → buffers → líquido.
- Gráfico de 1h: melhor gross e melhor net por rodada, com linha de break-even.
- Alertas acionáveis: snapshot stale, timeout, reorg, divergência econômica, RPC degradado.

### 2. Mercado

- Matriz token/par × DEX com preço, idade, direção e fee tier.
- Spread bruto separado de round-trip e net.
- Filtros por token, DEX, freshness e outlier; tooltip explica qualquer descarte.
- Nunca pintar spread bruto de verde se o líquido for negativo.

### 3. Rotas

- Tabela única com badges `2L` e `3L`, net como ordenação padrão.
- Colunas: rota, venues, gross, custos, net, distância, anchor, idade e status.
- Drawer de detalhes com diagrama das pernas e amounts encadeados.
- Toggle “somente autoritativas”; rotas 2L diagnósticas recebem badge de diagnóstico.

### 4. Pipeline

- Funil: quotes → edges → ciclos → top 32 → re-quote → econômica → estabilidade → risco.
- Latência por etapa e taxa de sucesso por DEX.
- Ranking das rejeições; clicar mostra amostras e route IDs.
- Exibir explicitamente `750 ranked / 32 evaluated`, evitando interpretação errada do teto.

### 5. Infraestrutura

- RPCs identificados por alias e hash, nunca URL; latência, erro, rate limit e último sucesso.
- Estado de WS, Prometheus, worker, fila, memória e uptime.
- Timeline de timeouts, reorgs e shutdown.

### 6. Segurança e configuração

- Checklist permanente: dry run, signer ausente, broadcaster ausente, wrapper desligado,
  simulação antes da execução, chain ID e contratos verificados.
- Configuração inicialmente read-only, dividida em Runtime, Discovery, Economics e Safety.
- Valores secretos aparecem como `configured/not configured`, nunca conteúdo ou prefixo.
- Um eventual editor usa campos allowlisted, validação backend, diff, versão esperada,
  confirmação e audit log; nunca recebe nem sobrescreve o TOML inteiro.

## Direção visual premium

- Tema escuro grafite (`#080B10`) com superfícies azul-ardósia e bordas de baixo contraste.
- Ciano para dados/atividade, âmbar para atenção, vermelho para bloqueios; verde reservado para
  lucro líquido confirmado ou gate aprovado.
- Tipografia: sans humanista para interface e mono tabular para preços, hashes e latências.
- Densidade profissional: grid de 12 colunas, cards discretos, tabelas compactas e amplo espaço
  nas áreas de decisão. Sem gradientes neon, emojis como ícones ou glassmorphism excessivo.
- Motion de 120–180 ms apenas para mudança de estado; respeitar `prefers-reduced-motion`.
- Desktop first (1440 px), funcional em 1024 px e modo compacto de consulta em mobile.
- Acessibilidade WCAG AA, navegação por teclado, foco visível e cor nunca como único sinal.

## Segurança da API

- Bind padrão `127.0.0.1`; exposição externa exige configuração explícita.
- Release 1 sem endpoints mutáveis.
- CORS allowlist, limite de payload, timeout, rate limit e headers defensivos.
- Se houver acesso remoto: TLS no reverse proxy e autenticação forte; sessão curta e auditável.
- CSP estrita e assets locais; sem analytics ou CDN de terceiros no console operacional.
- Nunca reutilizar token de leitura para comandos.
- Comandos futuros (`pause`, `resume`, `shutdown`) usam POST, CSRF, idempotency key, confirmação
  em duas etapas e registro de ator/horário/resultado. “Ativar live” não será comando da UI.

## Roadmap

### Fase 0 — contrato e segurança

- Remover/arquivar as duas APIs protótipo e o `src/App.jsx` duplicado.
- Criar `operator_state` serializável, redaction tests e snapshot fixtures.
- Separar contadores canônicos dos diagnósticos 2L.
- Definir OpenAPI v1 e golden JSON.

Aceite: um teste prova que nenhum campo sensível aparece; snapshot reproduz TUI e
`CANONICAL_ROUND_COMPLETE` para a mesma rodada.

### Fase 1 — backend read-only

- Implementar `GET /health`, `/snapshot`, `/config/public` e SSE.
- Publicar snapshot atomicamente ao final da rodada.
- Integrar lifecycle ao shutdown e bind configurável.
- Testes de handlers, reconnect SSE, stale state e backpressure.

Aceite: 3 horas de dry run sem leak, estado monotônico, shutdown limpo e zero broadcast.

### Fase 2 — shell visual e overview

- Migrar frontend para TypeScript e corrigir Vite/build.
- Implementar design tokens, layout, overview, skeletons, empty/error/stale states.
- Conectar REST inicial + SSE e mostrar estado de conexão.

Aceite: valores do overview coincidem com snapshot/log em fixture e runtime real.

### Fase 3 — mercado e rotas

- Matriz de preços, tabela 2L/3L, filtros e drawer de pernas.
- Waterfall econômico e distinção autoritativa/diagnóstica.
- Testes de unidade, acessibilidade e Playwright para reconexão e dados stale.

Aceite: nenhuma oportunidade líquida negativa é visualmente classificada como lucro.

### Fase 4 — pipeline, histórico e alertas

- Ring buffer backend de rodadas; endpoints paginados.
- Gráficos de net/gross/latência, funil e rejeições.
- Alertas locais com severidade, deduplicação e acknowledge somente visual.

Aceite: 24 horas de dados limitados por retenção, sem crescimento ilimitado de memória.

### Fase 5 — empacotamento operacional

- Build Vite incorporado/servido pelo Axum; Docker multi-stage.
- Healthchecks, CSP, reverse proxy opcional e documentação de operação.
- Atualizar Compose sem conflito entre Grafana e console.

Aceite: um comando sobe bot + console; assets têm cache por hash e API não tem cache.

### Fase 6 — controles seguros, somente após auditoria

- Pause/resume/shutdown com RBAC, CSRF, idempotência e audit trail.
- Editor allowlisted com diff e validação; alterações live continuam fora de escopo.

Aceite: testes negativos provam que sessão read-only, origem inválida ou estado stale não
executam comandos.

## Estratégia de testes

- Rust: unitários dos DTOs, redaction, invariantes econômicas e handlers Axum.
- Contract: golden JSON/OpenAPI entre Rust e TypeScript.
- Frontend: Vitest + Testing Library; axe para acessibilidade.
- E2E: Playwright com SSE gravado e cenários reconnect, timeout, reorg e shutdown.
- Operacional: canários headless e TUI simultâneos, comparando sequence/anchor/counters.
- Performance: snapshot abaixo de 250 KB, atualização visual abaixo de 100 ms e SSE limitado a
  uma publicação por rodada, com coalescing para clientes lentos.

## Ordem recomendada de implementação

Começar pelas Fases 0 e 1. Construir primeiro a UI sobre a API mockaria novamente os dados e
repetiria o problema atual. O primeiro PR deve conter somente contrato, publisher e API read-only;
o segundo, shell visual e overview; o terceiro, mercado/rotas; o quarto, histórico/alertas.

