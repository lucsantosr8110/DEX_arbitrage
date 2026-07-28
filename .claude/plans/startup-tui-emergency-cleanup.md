# Plano: TUI sobe no startup + saída graciosa em emergência

## Diagnóstico

O usuário executou o bot (compilou em release), pressionou `Ctrl+C`/`q` várias vezes e viu:

```
🛑 Saída de emergência: runtime tokio não respondeu em 5s.
//// tui não subiu. verifique correija, saida graciosa tbm nao ocorreu
```

A branch `fix/execution-safety-r2` já corrige o auto-death de 180s (`main.rs:684-756`), mas a **TUI só é spawnada na linha 478**, *depois* de:

- carregar config
- conectar HTTP (`connect_http_with_fallback`)
- conectar WS (`connect_ws`)
- construir `DexManager`
- inicializar `Bot`
- iniciar health-checker
- opcionalmente rodar `replay_scan`

Se qualquer uma dessas etapas travar (RPC half-open, rate-limit, WS sem resposta) ou demorar, o usuário fica olhando uma tela preta. Pressionar `Ctrl+C` dispara o listener antecipado (`main.rs:303-318`), que seta `EMERGENCY_SHUTDOWN` **imediatamente**. O watchdog (`emergency_shutdown.rs:18-27`) espera 5s e mata o processo com `process::exit(130)` — sem executar o `Drop` da `TuiApp`, deixando o terminal preso no alternate screen/raw mode.

Além disso, o join da thread da TUI (`main.rs:808-810`) é bloqueante sem timeout, então um TUI travado também pode pendurar o shutdown.

## Objetivo

1. TUI deve subir **logo no início** do startup e mostrar progresso das etapas de inicialização.
2. Operações longas de startup devem ter **timeout defensivo** e escutar shutdown.
3. `Ctrl+C`/`q` durante startup/TUI devem tentar **shutdown gracioso primeiro**; emergência só depois de uma janela maior.
4. Saída de emergência deve **restaurar o terminal** antes de `process::exit`.
5. Join da thread TUI deve ter **timeout** para não travar o shutdown.

## Arquivos a alterar

### 1. `src/tui.rs`

#### 1.1 Novo campo de status de startup em `TuiState`
Adicionar:

```rust
pub startup_phase: String,
pub startup_done: bool,
pub startup_error: Option<String>,
```

Inicializar default: `startup_phase: "Inicializando...".into()`, `startup_done: false`, `startup_error: None`.

#### 1.2 Tela de splash/loading
Criar método `draw_splash(&self, f: &mut Frame)` que, enquanto `startup_done == false`, exibe:
- título do bot
- fase atual (`startup_phase`)
- uptime desde startup
- instrução "Pressione 'q' para sair"

Alterar `run_inner` para chamar `draw_splash` quando `startup_done` for falso, e só depois desenhar o dashboard completo.

#### 1.3 Helpers de atualização de estado
Adicionar métodos em `TuiState` (ou funções free) para setar fase:

```rust
pub fn set_startup_phase(&mut self, phase: &str)
pub fn mark_startup_done(&mut self)
pub fn mark_startup_error(&mut self, err: String)
```

#### 1.4 `spawn_tui` retorna handle + sender de estado (opcional)
Manter `spawn_tui` retornando `JoinHandle<()>`, mas garantir que a thread responda ao broadcast de shutdown mesmo antes do dashboard estar pronto (já responde — OK).

### 2. `src/main.rs`

#### 2.1 Mover spawn da TUI para o início
Logo após criar `shutdown_tx` (linha 298):

```rust
let tui_state = Arc::new(std::sync::RwLock::new(tui::TuiState::default()));
let tui_enabled = !flashloan_bot::core::paper_validation::env_paper_flag();
let tui_handle = if tui_enabled {
    Some(tui::spawn_tui(tui_state.clone(), shutdown_tx.clone()))
} else {
    None
};
```

Remover a duplicação atual nas linhas 472-482.

#### 2.2 Reportar progresso de startup na TUI
Criar helper local:

```rust
fn tui_phase(state: &Arc<RwLock<TuiState>>, phase: &str) {
    if let Ok(mut s) = state.write() {
        s.startup_phase = phase.into();
    }
}
```

Chamar `tui_phase(&tui_state, "carregando config")` etc. antes de cada operação longa.

#### 2.3 Timeout + shutdown check nas operações de startup
Para cada etapa bloqueante, usar padrão:

```rust
let mut sd_rx = shutdown_tx.subscribe();
tokio::select! {
    _ = sd_rx.recv() => {
        info!("🔌 Shutdown durante startup — abortando inicialização.");
        return graceful_cleanup(tui_handle, tui_state);
    }
    res = RpcProvider::connect_http_with_fallback(...) => res?
}
```

Aplicar a:
- `connect_http_with_fallback` (linha 376)
- `connect_ws` (linha 388)
- `DexManager::new` (linha 397)
- `Bot::init_with_engine` / `Bot::new_with_engine` (linha 425-435)
- `start_health_checker` (linha 405) — se demorar

Cada operação já deve ter timeout interno; o `select!` adiciona a possibilidade de cancelar via shutdown.

#### 2.4 Marcar startup concluído
Antes da mensagem `Sistema pronto` (linha 760):

```rust
if let Ok(mut s) = tui_state.write() {
    s.startup_done = true;
}
```

#### 2.5 Timeout no join da TUI
Substituir:

```rust
if let Some(handle) = tui_handle {
    let _ = handle.join();
}
```

por:

```rust
if let Some(handle) = tui_handle {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = handle.join();
        let _ = tx.send(());
    });
    if rx.recv_timeout(Duration::from_secs(5)).is_err() {
        warn!("⚠️ TUI não finalizou em 5s — prosseguindo com shutdown forçado.");
    }
}
```

Isso evita que TUI travado pendure o shutdown indefinidamente.

#### 2.6 Listener antecipado de Ctrl+C: shutdown primeiro, emergência depois
Alterar o listener independente (linhas 303-318) para **não setar `EMERGENCY_SHUTDOWN` imediatamente**:

```rust
std::thread::spawn(move || {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build();
    if let Ok(rt) = rt {
        rt.block_on(async {
            let _ = tokio::signal::ctrl_c().await;
            warn!("🛑 Ctrl+C antecipado — solicitando shutdown gracioso.");
            let _ = shutdown_tx.send(());
            // Só força emergência se graceful não conseguir finalizar.
            tokio::time::sleep(Duration::from_secs(8)).await;
            emergency_shutdown::request_emergency_shutdown();
        });
    }
});
```

Dessa forma, um simples `Ctrl+C` no terminal inicia o shutdown normal; se o runtime estiver realmente preso, 8s depois o watchdog de emergência entra.

#### 2.7 Helper de cleanup no startup
Adicionar função auxiliar para restaurar terminal quando shutdown ocorre durante startup:

```rust
fn graceful_cleanup(
    tui_handle: Option<std::thread::JoinHandle<()>>,
    tui_state: Arc<RwLock<tui::TuiState>>,
) -> Result<()> {
    if let Ok(mut s) = tui_state.write() {
        s.startup_error = Some("Shutdown solicitado durante inicialização".into());
    }
    if let Some(handle) = tui_handle {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || { let _ = handle.join(); let _ = tx.send(()); });
        let _ = rx.recv_timeout(Duration::from_secs(3));
    }
    Ok(())
}
```

### 3. `src/emergency_shutdown.rs`

#### 3.1 Restaurar terminal antes de `process::exit`
Adicionar dependência de `crossterm` (já é dependência via `tui.rs`). Antes de `process::exit(130)`:

```rust
use crossterm::{execute, terminal::{disable_raw_mode, LeaveAlternateScreen}, event::DisableMouseCapture};

// ... no watchdog, antes do exit:
let _ = disable_raw_mode();
let _ = execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
```

A ordem deve ser: `disable_raw_mode`, depois `LeaveAlternateScreen` + `DisableMouseCapture`.

#### 3.2 (Opcional) Tornar watchdog mais tolerante
Aumentar o sleep de 5s para 8s para dar tempo de TUI fazer cleanup. O watchdog deve ser último recurso, não 5s rígido.

### 4. `src/core/paper_validation.rs` (se existir)

Garantir que `env_paper_flag()` seja importável no `main.rs` no ponto onde TUI é spawnada. Já é usado na linha 474, então OK.

## Detalhes técnicos

### Concorrência TUI thread vs main thread
A `TuiState` fica em `Arc<RwLock<TuiState>>`. A main thread escreve `startup_phase`; a TUI thread lê a cada 250ms. `RwLock` é suficiente.

### Shutdown durante startup
`shutdown_tx` é criado cedo. A TUI thread já está rodando e escuta no broadcast. Se usuário pressionar `q`, a TUI envia broadcast e sai; a main, ao fazer `select!` com `sd_rx.recv()`, aborta o startup. A TUI thread fará cleanup ao sair do `run_inner`.

### Timeout no join da TUI
`recv_timeout` está disponível na std. Se TUI não sair em 5s, o processo continua; o watchdog de emergência (se ativado) eventualmente mata, mas terminal pode já ter sido restaurado pela TUI até lá.

### Cleanup no emergency
Mesmo que o watchdog mate o processo, ele tenta restaurar o terminal. Como as funções do crossterm são idempotentes, chamar `disable_raw_mode` sem raw mode ligado é seguro.

## Riscos / mitigações

| Risco | Mitigação |
|-------|-----------|
| TUI mostra splash eternamente se RPC travar | Timeout nas operações + shutdown escutável; erro aparece em `startup_error` |
| Cores/terminal do crossterm usados em thread separada sem sincronização | Operações são globais do terminal; risco baixo; usamos `let _ =` para ignorar erros |
| `recv_timeout` quebra compilação? | Método estável desde Rust 1.12 |
| Mudança de fluxo do early Ctrl+C faz emergência demorar | Manter watchdog ativo; TUI `q`/`Esc` ainda chamam `request_emergency_shutdown()` imediatamente |

## Testes / verificação

1. `cargo check` e `cargo build --release`.
2. Executar em modo paper (`PAPER_VALIDATION=1`) — deve ser headless, sem TUI; Ctrl+C deve sair graciosamente.
3. Executar em modo normal sem RPC configurado — TUI deve aparecer rapidamente com splash e mostrar fase "conectando HTTP", depois erro claro.
4. Executar com RPC válido — verificar que TUI mostra fases e depois dashboard.
5. Pressionar `q`/`Esc`/`Ctrl+C` em cada fase (splash, dashboard) — terminal deve voltar ao estado normal, sem `reset` manual.
6. Simular runtime travado (ex: breakpoint no radar) e pressionar Ctrl+C — deve aparecer mensagem de emergência, mas terminal restaurado.
