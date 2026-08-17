// ============================================================
// src/tui.rs — Terminal User Interface for DEX Arbitrage Bot
// ============================================================
// Dashboard em tempo real para o operador
// ============================================================

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, List, ListItem, Paragraph, Row, Table},
    Frame, Terminal,
};
use std::{
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, RwLock,
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::broadcast;

// ============================================================
// TUI State
// ============================================================

#[derive(Clone, Debug)]
pub struct TuiState {
    pub running: bool,
    pub uptime: Duration,
    pub cycle_count: u64,
    pub dex_count: usize,
    pub pairs_count: usize,
    pub gross_positive: u32,
    pub net_positive: u32,
    pub negative_cycles: u32,
    /// Soma USD dos ciclos com net projetado > 0 no último scan (oportunidade real).
    pub net_usd_total: f64,
    pub last_prices: Vec<PriceRow>,
    /// Top-N spreads por Spread% desc (espelha log `[TOPSPREAD]`, sem TVL).
    pub top_spreads: Vec<TopSpreadRow>,
    /// Instant do último scan de preços recebido (set em update_tui_state).
    /// None = nenhum scan chegou ainda. Usado p/ mostrar "último scan: Ns atrás"
    /// e detectar radar morto (WS caiu, sem novos blocos).
    pub last_update: Option<Instant>,
    /// Contador de render ticks (incrementado no loop do TUI a cada 250ms).
    /// Indicador de heartbeat: se parar de piscar, o próprio TUI travou.
    pub render_tick: u64,
    /// Instant em que o processo subiu. Setado uma vez no spawn do TUI;
    /// o loop de render computa `uptime = start.elapsed()` a cada tick,
    /// então o uptime avança mesmo se o bot_task travar (preço parar de
    /// chegar). Antes o uptime só era atualizado em update_tui_state, na
    /// thread do bot_task — se ela bloqueava em process_prices().await,
    /// o uptime congelava junto.
    pub start: Instant,
    /// Fase atual de inicialização exibida no splash screen.
    pub startup_phase: String,
    /// Startup finalizado — a partir daqui mostra dashboard completo.
    pub startup_done: bool,
    /// Erro fatal durante startup (ex.: RPC indisponível).
    pub startup_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PriceRow {
    pub pair: String,
    pub quickswap: Option<f64>,
    pub sushiswap: Option<f64>,
    pub curve: Option<f64>,
    pub uniswap_v3: Option<f64>,
    /// Net projetado (USD) do melhor 2-hop do par. None = sem adj cycle.
    pub net_usd: Option<f64>,
}

/// Linha do painel Top Spreads (sem TVL — TVL só no log `[TOPSPREAD]`).
#[derive(Clone, Debug)]
pub struct TopSpreadRow {
    /// Number of executable legs represented by this row (2L or 3L).
    pub hop_count: usize,
    pub pair: String,
    pub tui_spread_pct: f64,
    pub buy_dex: String,
    pub sell_dex: String,
    /// Optional complete venue chain for canonical routes (for example U→Q→S).
    pub legs_label: Option<String>,
    /// None = sem reverse cotado (cycle_rate indisponível).
    pub cycle_rate: Option<f64>,
    /// None = route economics unavailable.
    pub net_usd: Option<f64>,
    /// Quanto falta para o net projetado virar positivo (0 se já lucrativo).
    pub distance_to_profit: f64,
    pub executable: bool,
    pub has_curve_leg: bool,
    pub outlier: Option<String>,
    /// Economic waterfall (USD, mesma conversão de `net_usd`):
    /// `gross_pnl_usd` já vem pós-fee do AMM (a cotação real de
    /// `first_touch_quotes`/reuse local já embute o fee do pool — nunca
    /// subtraído de novo aqui). `gas_cost_usd`/`flashloan_cost_usd` são
    /// estimativas proporcionais (`config.gas_cost_ppb`/`flashloan_cost_ppb`),
    /// não gas real nem premium Aave on-chain. `net_usd` acima já é
    /// `gross - gas - flashloan` (única dedução, sem double-count).
    /// `wrapper_cost_usd`/direct-vs-flashloan-vs-wrapper split não são
    /// computados por `StatefulRouteEvaluator` — permanecem `None`
    /// (COMPONENT_UNAVAILABLE), nunca fabricados como 0.
    pub gross_pnl_usd: Option<f64>,
    pub gas_cost_usd: Option<f64>,
    pub flashloan_cost_usd: Option<f64>,
}

/// Causa dominante de uma rota não-positiva. Determinística: dado o mesmo
/// `(gross_pnl_usd, gas_cost_usd, flashloan_cost_usd, net_usd)`, sempre
/// devolve a mesma classe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegativeCause {
    /// net_usd > 0 — não há causa negativa a classificar.
    Positive,
    /// gross_pnl_usd <= 0: nem antes de custos a rota fecha.
    NoGrossSpread,
    /// gross > 0 mas net <= 0, e gas_cost_usd é o maior componente de custo.
    GasDominates,
    /// gross > 0 mas net <= 0, e flashloan_cost_usd é o maior componente.
    FlashloanFeeDominates,
    /// gross > 0, net <= 0, mas gas/flashloan indisponíveis ou não
    /// suficientes para explicar sozinhos — não inventa uma causa.
    Other,
}

impl NegativeCause {
    pub fn as_str(self) -> &'static str {
        match self {
            NegativeCause::Positive => "POSITIVE",
            NegativeCause::NoGrossSpread => "NO_GROSS_SPREAD",
            NegativeCause::GasDominates => "GAS_DOMINATES",
            NegativeCause::FlashloanFeeDominates => "FLASHLOAN_FEE_DOMINATES",
            NegativeCause::Other => "NET_NON_POSITIVE_OTHER",
        }
    }
}

/// Classifica a causa dominante de uma rota não-positiva. Pura, sem RPC,
/// sem estado — mesma entrada sempre produz a mesma saída.
pub fn classify_negative_cause(
    gross_pnl_usd: Option<f64>,
    gas_cost_usd: Option<f64>,
    flashloan_cost_usd: Option<f64>,
    net_usd: Option<f64>,
) -> NegativeCause {
    let Some(net) = net_usd else {
        return NegativeCause::Other;
    };
    if net > 0.0 {
        return NegativeCause::Positive;
    }
    let Some(gross) = gross_pnl_usd else {
        return NegativeCause::Other;
    };
    if gross <= 0.0 {
        return NegativeCause::NoGrossSpread;
    }
    match (gas_cost_usd, flashloan_cost_usd) {
        (Some(gas), Some(flashloan)) if gas > 0.0 || flashloan > 0.0 => {
            if gas >= flashloan {
                NegativeCause::GasDominates
            } else {
                NegativeCause::FlashloanFeeDominates
            }
        }
        _ => NegativeCause::Other,
    }
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            running: true,
            uptime: Duration::ZERO,
            cycle_count: 0,
            dex_count: 4,
            pairs_count: 0,
            gross_positive: 0,
            net_positive: 0,
            negative_cycles: 0,
            net_usd_total: 0.0,
            last_prices: Vec::new(),
            top_spreads: Vec::new(),
            last_update: None,
            render_tick: 0,
            start: Instant::now(),
            startup_phase: "Inicializando...".into(),
            startup_done: false,
            startup_error: None,
        }
    }
}

impl TuiState {
    pub fn set_startup_phase(&mut self, phase: &str) {
        self.startup_phase = phase.into();
    }

    pub fn mark_startup_done(&mut self) {
        self.startup_done = true;
    }

    pub fn mark_startup_error(&mut self, err: String) {
        self.startup_error = Some(err);
    }
}

// ============================================================
// TUI App
// ============================================================

pub struct TuiApp {
    state: Arc<RwLock<TuiState>>,
    shutdown_tx: broadcast::Sender<()>,
    event_rx: mpsc::Receiver<Event>,
    reader_shutdown: Arc<AtomicBool>,
    reader_handle: Option<thread::JoinHandle<()>>,
}

impl Drop for TuiApp {
    fn drop(&mut self) {
        self.reader_shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.reader_handle.take() {
            let _ = h.join();
        }
    }
}

impl TuiApp {
    pub fn new(state: Arc<RwLock<TuiState>>, shutdown_tx: broadcast::Sender<()>) -> Self {
        let (event_tx, event_rx) = mpsc::channel::<Event>();
        let reader_shutdown = Arc::new(AtomicBool::new(false));
        let sd = reader_shutdown.clone();

        let handle = thread::spawn(move || loop {
            if sd.load(Ordering::Relaxed) {
                break;
            }
            match crossterm::event::poll(Duration::from_millis(100)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(ev) => {
                        if event_tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        });

        Self {
            state,
            shutdown_tx,
            event_rx,
            reader_shutdown,
            reader_handle: Some(handle),
        }
    }

    pub fn run(&self, shutdown_rx: &mut broadcast::Receiver<()>) -> io::Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let last_tick = Instant::now();
        let tick_rate = Duration::from_millis(250);

        let result = self.run_inner(&mut terminal, shutdown_rx, last_tick, tick_rate);

        // Restaura o terminal EM QUALQUER CASO (erro ou saída normal).
        // Antes o terminal só era restaurado no Ok(()); se terminal.draw()
        // falhava, o raw mode ficava ligado pra sempre e o usuário tinha
        // que resetar o terminal manualmente.
        let mut cleanup = || -> io::Result<()> {
            disable_raw_mode()?;
            execute!(
                terminal.backend_mut(),
                LeaveAlternateScreen,
                DisableMouseCapture
            )?;
            terminal.show_cursor()?;
            Ok(())
        };

        // cleanup ignorando erro do result — queremos restaurar o terminal
        // mesmo se o loop principal já tiver falhado.
        let _ = cleanup();

        result
    }

    fn run_inner(
        &self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        shutdown_rx: &mut broadcast::Receiver<()>,
        mut last_tick: Instant,
        tick_rate: Duration,
    ) -> io::Result<()> {
        loop {
            // Shutdown check via broadcast (sinal do Ctrl+C no main).
            match shutdown_rx.try_recv() {
                Ok(_) | Err(broadcast::error::TryRecvError::Closed) => return Ok(()),
                Err(broadcast::error::TryRecvError::Lagged(_))
                | Err(broadcast::error::TryRecvError::Empty) => {}
            }

            // Heartbeat + uptime: computado na thread da TUI, independente
            // do bot_task. Antes o uptime só atualizava em update_tui_state,
            // que só é chamado pelo bot_task — se process_prices() travasse,
            // o uptime congelava junto.
            if let Ok(mut s) = self.state.try_write() {
                s.render_tick = s.render_tick.wrapping_add(1);
                s.uptime = s.start.elapsed();
            }

            // Draw: splash durante startup, dashboard depois.
            let startup_done = self.state.read().map(|s| s.startup_done).unwrap_or(false);
            let _ = terminal.draw(|f| {
                if startup_done {
                    self.draw(f);
                } else {
                    self.draw_splash(f);
                }
            });

            // Leitura NÃO-BLOQUEANTE de eventos via canal mpsc.
            // A thread dedicada (spawnada em TuiApp::new) lê do crossterm
            // e coloca no canal. A TUI principal apenas try_recv — nunca
            // bloqueia em event::read().
            loop {
                match self.event_rx.try_recv() {
                    Ok(Event::Key(key)) => {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => {
                                crate::emergency_shutdown::request_emergency_shutdown();
                                let _ = self.shutdown_tx.send(());
                                return Ok(());
                            }
                            KeyCode::Char('c')
                                if key.modifiers.contains(event::KeyModifiers::CONTROL) =>
                            {
                                crate::emergency_shutdown::request_emergency_shutdown();
                                let _ = self.shutdown_tx.send(());
                                return Ok(());
                            }
                            _ => {} // ignora outras teclas
                        }
                    }
                    Ok(_) => {
                        // Evento não-tecla (Resize, Mouse, FocusGained, FocusLost):
                        // apenas descarta e continua.
                    }
                    Err(mpsc::TryRecvError::Empty) => break, // sem eventos por agora
                    Err(mpsc::TryRecvError::Disconnected) => {
                        break;
                    }
                }
            }

            // Dorme o restante do tick. Usamos thread::sleep (bloqueante)
            // em vez de tokio::time::sleep porque a TUI não precisa de
            // async — todo I/O é síncrono. O event reader thread roda em
            // paralelo e acumula eventos no canal mpsc; nenhum se perde
            // durante o sono.
            let elapsed = last_tick.elapsed();
            if elapsed < tick_rate {
                thread::sleep(tick_rate - elapsed);
            }
            last_tick = Instant::now();
        }
    }

    fn draw(&self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5), // Header (border + 3 lines de conteúdo)
                Constraint::Length(6), // Status (border + 4 lines de conteúdo)
                Constraint::Min(10),   // Center: Prices | TopSpreads (split horizontal)
                Constraint::Length(4), // Footer (border + 2 lines de conteúdo)
            ])
            .split(f.area());

        self.draw_header(f, chunks[0]);
        self.draw_status(f, chunks[1]);

        // Centro: preços à esquerda, top spreads à direita (cabe terminal 80x24).
        let center = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(chunks[2]);
        self.draw_prices(f, center[0]);
        self.draw_top_spreads(f, center[1]);

        self.draw_footer(f, chunks[3]);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let header = Paragraph::new(vec![
            Line::from(vec![Span::styled(
                "  DEX ARBITRAGE BOT v1.0",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(vec![
                Span::styled("  Polygon", Style::default().fg(Color::Yellow)),
                Span::raw(" | "),
                Span::styled("Flashloan", Style::default().fg(Color::Green)),
            ]),
            Line::from(vec![
                Span::raw("  Press "),
                Span::styled(
                    "q",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" to quit"),
            ]),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );
        f.render_widget(header, area);
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let state = self.blocking_read();

        let uptime_str = format!(
            "{:02}:{:02}:{:02}",
            state.uptime.as_secs() / 3600,
            (state.uptime.as_secs() % 3600) / 60,
            state.uptime.as_secs() % 60
        );

        // Heartbeat: pisca a cada render tick (4 ticks/s). Parado = TUI travou.
        let beat = if state.render_tick % 2 == 0 {
            "●"
        } else {
            "○"
        };
        let beat_color = if state.render_tick % 2 == 0 {
            Color::Green
        } else {
            Color::DarkGray
        };

        // Idade do último scan: quanto faz que o radar entregou preços. Se crescer
        // além de ~normal (segundos), radar stallou (WS morto / RPC rate-limit).
        let scan_age = match state.last_update {
            Some(t) => {
                let secs = t.elapsed().as_secs();
                if secs < 60 {
                    format!("{}s", secs)
                } else {
                    format!("{}m{}s", secs / 60, secs % 60)
                }
            }
            None => "--".to_string(),
        };
        // >15s sem scan = suspeito (block time Polygon ~2s); pinta de vermelho.
        let scan_age_color = match state.last_update {
            Some(t) if t.elapsed().as_secs() > 15 => Color::Red,
            Some(_) => Color::Green,
            None => Color::Yellow,
        };

        let status_items = vec![
            ListItem::new(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    beat,
                    Style::default().fg(beat_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" Status: ", Style::default().fg(Color::Gray)),
                Span::styled(
                    "RUNNING",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" | "),
                Span::raw(format!("Uptime: {}", uptime_str)),
                Span::raw(" | "),
                Span::styled("scan: ", Style::default().fg(Color::Gray)),
                Span::styled(scan_age, Style::default().fg(scan_age_color)),
            ])),
            ListItem::new(Line::from(vec![
                Span::styled("  DEXes: ", Style::default().fg(Color::Gray)),
                Span::styled(
                    format!("{}", state.dex_count),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(" | "),
                Span::raw(format!("Pares: {}", state.pairs_count)),
                Span::raw(" | "),
                Span::raw(format!("Ciclos: {}", state.cycle_count)),
            ])),
            ListItem::new(Line::from(vec![
                Span::styled("  Econ: ", Style::default().fg(Color::Gray)),
                Span::styled(
                    format!("gross={}", state.gross_positive),
                    Style::default().fg(if state.gross_positive > 0 {
                        Color::Green
                    } else {
                        Color::Red
                    }),
                ),
                Span::raw(" | "),
                Span::styled(
                    format!("net+={}", state.net_positive),
                    Style::default().fg(if state.net_positive > 0 {
                        Color::Green
                    } else {
                        Color::Yellow
                    }),
                ),
                Span::raw(" | "),
                Span::styled(
                    format!("net=${:.2}", state.net_usd_total),
                    Style::default().fg(if state.net_usd_total > 0.0 {
                        Color::Green
                    } else if state.net_usd_total < 0.0 {
                        Color::Red
                    } else {
                        Color::Gray
                    }),
                ),
                Span::raw(" | "),
                Span::styled(
                    format!("neg={}", state.negative_cycles),
                    Style::default().fg(Color::Gray),
                ),
            ])),
        ];

        let status_list = List::new(status_items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Status ")
                .border_style(Style::default().fg(Color::Yellow)),
        );
        f.render_widget(status_list, area);
    }

    fn draw_prices(&self, f: &mut Frame, area: Rect) {
        let state = self.blocking_read();

        let header = Row::new(vec![
            Cell::from("Pair").style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Cell::from("QuickSwap").style(Style::default().fg(Color::Green)),
            Cell::from("SushiSwap").style(Style::default().fg(Color::Magenta)),
            Cell::from("Curve").style(Style::default().fg(Color::Yellow)),
            Cell::from("UniV3").style(Style::default().fg(Color::Blue)),
            Cell::from("Spread%").style(Style::default().fg(Color::Red)),
            Cell::from("Net$").style(Style::default().fg(Color::Cyan)),
        ]);

        let mut rows: Vec<Row> = Vec::new();
        for p in &state.last_prices {
            let venue_prices: Vec<f64> = [p.quickswap, p.sushiswap, p.curve, p.uniswap_v3]
                .into_iter()
                .flatten()
                .filter(|v| *v > 0.0)
                .collect();

            let spread = if venue_prices.len() >= 2 {
                let min = venue_prices.iter().cloned().fold(f64::INFINITY, f64::min);
                let max = venue_prices
                    .iter()
                    .cloned()
                    .fold(f64::NEG_INFINITY, f64::max);
                (max - min) / min * 100.0
            } else {
                0.0
            };

            // Truncar pair se for muito longo (char-safe, evita panic em UTF-8 multi-byte)
            let pair_display = if p.pair.chars().count() > 12 {
                format!("{}...", p.pair.chars().take(12).collect::<String>())
            } else {
                p.pair.clone()
            };

            rows.push(Row::new(vec![
                Cell::from(pair_display),
                Cell::from(format_opt(p.quickswap)),
                Cell::from(format_opt(p.sushiswap)),
                Cell::from(format_opt(p.curve)),
                Cell::from(format_opt(p.uniswap_v3)),
                Cell::from(format!("{:.2}%", spread)).style(
                    // Reduzir para 2 casas decimais
                    if spread > 0.5 {
                        Style::default().fg(Color::Red)
                    } else if spread > 0.1 {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                ),
                Cell::from(fmt_opt_net(p.net_usd)).style(
                    if matches!(p.net_usd, Some(n) if n > 0.0) {
                        Style::default().fg(Color::Green)
                    } else if matches!(p.net_usd, Some(n) if n < 0.0) {
                        Style::default().fg(Color::Red)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                ),
            ]));
        }

        let widths = [
            Constraint::Length(14), // Pair
            Constraint::Length(13), // QuickSwap
            Constraint::Length(13), // SushiSwap
            Constraint::Length(13), // Curve
            Constraint::Length(13), // UniV3
            Constraint::Length(10), // Spread%
            Constraint::Length(9),  // Net$
        ];

        let table = Table::new(rows, widths).header(header).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Preços Cross-DEX ")
                .border_style(Style::default().fg(Color::Green)),
        );

        f.render_widget(table, area);
    }

    fn draw_top_spreads(&self, f: &mut Frame, area: Rect) {
        let state = self.blocking_read();

        let header = Row::new(vec![
            Cell::from("Tipo").style(Style::default().fg(Color::Magenta)),
            Cell::from("Pair").style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Cell::from("Legs").style(Style::default().fg(Color::Yellow)),
            Cell::from("Gross%").style(Style::default().fg(Color::Red)),
            Cell::from("Net$").style(Style::default().fg(Color::Green)),
            Cell::from("Dist$").style(Style::default().fg(Color::Gray)),
            Cell::from("E").style(Style::default().fg(Color::Gray)),
        ]);

        // Ordena por proximidade do lucro: positivos no topo, depois menor distância.
        let mut sorted: Vec<&TopSpreadRow> = state.top_spreads.iter().collect();
        sorted.sort_by(|a, b| {
            a.distance_to_profit
                .partial_cmp(&b.distance_to_profit)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    let na = a.net_usd.unwrap_or(f64::NEG_INFINITY);
                    let nb = b.net_usd.unwrap_or(f64::NEG_INFINITY);
                    nb.partial_cmp(&na).unwrap_or(std::cmp::Ordering::Equal)
                })
        });

        let mut rows: Vec<Row> = Vec::new();
        for t in sorted {
            let pair_display = if t.pair.chars().count() > 10 {
                format!("{}..", t.pair.chars().take(10).collect::<String>())
            } else {
                t.pair.clone()
            };
            let gross = match t.cycle_rate {
                Some(c) if c.is_finite() => (c - 1.0) * 100.0,
                _ => 0.0,
            };
            let exec = if t.has_curve_leg {
                "C".to_string() // perna Curve (vitrine)
            } else if t.executable {
                "y".to_string()
            } else {
                "n".to_string()
            };
            let net_color = if t.net_usd.map(|n| n > 0.0).unwrap_or(false) {
                Color::Green
            } else if t.net_usd.map(|n| n < 0.0).unwrap_or(false) {
                Color::Red
            } else {
                Color::Gray
            };
            let dist_color = if t.distance_to_profit <= 0.0 {
                Color::Green
            } else if t.distance_to_profit < 0.5 {
                Color::Yellow
            } else {
                Color::Red
            };
            rows.push(Row::new(vec![
                Cell::from(format!("{}L", t.hop_count)),
                Cell::from(pair_display),
                Cell::from(t.legs_label.clone().unwrap_or_else(|| {
                    format!("{}→{}", abbrev_venue(&t.buy_dex), abbrev_venue(&t.sell_dex))
                })),
                Cell::from(format!("{:.2}%", gross)),
                Cell::from(fmt_opt_net(t.net_usd)).style(Style::default().fg(net_color)),
                Cell::from(format!("{:.3}", t.distance_to_profit))
                    .style(Style::default().fg(dist_color)),
                Cell::from(exec),
            ]));
        }

        let widths = [
            Constraint::Length(5),  // Tipo
            Constraint::Length(12), // Pair
            Constraint::Length(9),  // Legs
            Constraint::Length(7),  // Gross%
            Constraint::Length(7),  // Net$
            Constraint::Length(7),  // Dist$
            Constraint::Length(3),  // E
        ];

        let table = Table::new(rows, widths).header(header).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Top Combos (dist p/ lucro) ")
                .border_style(Style::default().fg(Color::Magenta)),
        );

        f.render_widget(table, area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let footer = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("  Alchemy RPC", Style::default().fg(Color::Cyan)),
                Span::raw(" | "),
                Span::styled("Aave V3", Style::default().fg(Color::Green)),
            ]),
            Line::from(vec![
                Span::styled("  Curve 0.04%", Style::default().fg(Color::Yellow)),
                Span::raw(" | "),
                Span::styled("UniV3 0.01-1%", Style::default().fg(Color::Blue)),
            ]),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Gray)),
        );
        f.render_widget(footer, area);
    }

    /// Splash screen exibida durante a inicialização, antes do dashboard.
    /// Mostra a fase atual, uptime e instruções — dá feedback imediato ao
    /// operador quando RPC/DexManager/Bot ainda estão subindo.
    fn draw_splash(&self, f: &mut Frame) {
        let state = self.blocking_read();

        let area = f.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(20),
                Constraint::Percentage(60),
                Constraint::Percentage(20),
            ])
            .split(area);

        let uptime_str = format!(
            "{:02}:{:02}:{:02}",
            state.uptime.as_secs() / 3600,
            (state.uptime.as_secs() % 3600) / 60,
            state.uptime.as_secs() % 60
        );

        let phase_color = if state.startup_error.is_some() {
            Color::Red
        } else {
            Color::Yellow
        };

        let status_text = match &state.startup_error {
            Some(err) => format!("Erro: {}", err),
            None => state.startup_phase.clone(),
        };

        let body = Paragraph::new(vec![
            Line::from(vec![Span::styled(
                "  ⚡ DEX Arbitrage Bot",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Status: ", Style::default().fg(Color::Gray)),
                Span::styled(
                    "INICIALIZANDO",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Fase:   ", Style::default().fg(Color::Gray)),
                Span::styled(status_text, Style::default().fg(phase_color)),
            ]),
            Line::from(vec![
                Span::styled("  Uptime: ", Style::default().fg(Color::Gray)),
                Span::raw(uptime_str),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::raw("  Pressione "),
                Span::styled(
                    "q",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" ou "),
                Span::styled(
                    "Esc",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" para sair"),
            ]),
        ])
        .alignment(ratatui::layout::Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Inicializando ")
                .border_style(Style::default().fg(Color::Cyan)),
        );

        f.render_widget(body, chunks[1]);
    }

    fn blocking_read(&self) -> TuiState {
        match self.state.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

fn format_opt(v: Option<f64>) -> String {
    match v {
        Some(p) if p > 100.0 => format!("{:.2}", p),
        Some(p) => format!("{:.8}", p),
        None => "-".to_string(),
    }
}

/// Formata net USD: Some(v) → "$X.XX" (2 casas), None → "-".
fn fmt_opt_net(v: Option<f64>) -> String {
    match v {
        Some(n) if n.is_finite() => format!("${:.2}", n),
        _ => "-".to_string(),
    }
}

/// Abrevia nome de DEX para caber na coluna Legs.
fn abbrev_venue(v: &str) -> String {
    let s = v.to_ascii_lowercase();
    if s.contains("quickswap") {
        "Q".to_string()
    } else if s.contains("sushiswap") {
        "S".to_string()
    } else if s.contains("uniswap_v3") || s.contains("uniswap") {
        "U".to_string()
    } else if s.contains("curve") {
        "C".to_string()
    } else {
        v.chars().take(3).collect::<String>()
    }
}

/// Chave canônica de par (tokens sorted, direção-agnóstica): "WETH-USDC" ==
/// "USDC-WETH" == "usdc-weth". Usado p/ casar adj_cycles (canonical) com
/// PriceRow (direcional) sem depender da direção do label.
pub fn norm_pair(pair: &str) -> String {
    let (a, b) = match pair.split_once('-') {
        Some(ab) => ab,
        None => return pair.to_string(),
    };
    let au = a.to_ascii_uppercase();
    let bu = b.to_ascii_uppercase();
    if au <= bu {
        format!("{}-{}", au, bu)
    } else {
        format!("{}-{}", bu, au)
    }
}

// ============================================================
// TUI Spawner
// ============================================================

/// Spawn the TUI in a background thread. A TUI roda 100% síncrona
/// (crossterm + ratatui + thread::sleep) — não precisa de runtime tokio.
/// O event reader thread roda em paralelo e acumula eventos no canal mpsc.
///
/// Retorna o JoinHandle da thread da TUI. O main() faz join nele no shutdown
/// para garantir que o terminal seja restaurado (disable_raw_mode +
/// LeaveAlternateScreen) ANTES do processo sair. Sem este join, se o processo
/// morresse (ou o runtime abortasse as tasks) com a TUI ainda em raw mode, o
/// terminal do usuário ficava preso no alternate screen — parecia travado,
/// nenhum comando respondia, precisava `reset`. O join espera a TUI sair do
/// run_inner (que ela faz ao receber o broadcast de shutdown), momento em
/// que o cleanup já rodou.
pub fn spawn_tui(
    state: Arc<RwLock<TuiState>>,
    shutdown_tx: broadcast::Sender<()>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut sd_rx = shutdown_tx.subscribe();
        let app = TuiApp::new(state, shutdown_tx);
        if let Err(e) = app.run(&mut sd_rx) {
            // Não usar eprintln! durante raw mode (terminal alternate screen).
            // O erro é logado no arquivo via tracing; aqui só ignoramos.
            let _ = e;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_pair_canonico_direcao_agnostica() {
        assert_eq!(norm_pair("WETH-USDC"), "USDC-WETH");
        assert_eq!(norm_pair("USDC-WETH"), "USDC-WETH");
        assert_eq!(norm_pair("usdc-weth"), "USDC-WETH");
        assert_eq!(norm_pair("USDC-USDT"), "USDC-USDT");
        // par sem '-' → retorna como está (não panic).
        assert_eq!(norm_pair("USDC"), "USDC");
    }

    // ---- classify_negative_cause ----

    #[test]
    fn negative_cause_classification_is_deterministic() {
        // Same inputs called twice must produce identical output — no
        // hidden state, no ordering dependency.
        let inputs = (Some(0.5), Some(0.6), Some(0.1), Some(-0.2));
        let a = classify_negative_cause(inputs.0, inputs.1, inputs.2, inputs.3);
        let b = classify_negative_cause(inputs.0, inputs.1, inputs.2, inputs.3);
        assert_eq!(a, b);
        assert_eq!(a, NegativeCause::GasDominates);
    }

    #[test]
    fn negative_cause_positive_when_net_positive() {
        let cause = classify_negative_cause(Some(1.0), Some(0.3), Some(0.1), Some(0.6));
        assert_eq!(cause, NegativeCause::Positive);
        assert_eq!(cause.as_str(), "POSITIVE");
    }

    #[test]
    fn negative_cause_no_gross_spread_when_gross_non_positive() {
        let cause = classify_negative_cause(Some(-0.1), Some(0.0), Some(0.0), Some(-0.1));
        assert_eq!(cause, NegativeCause::NoGrossSpread);
    }

    #[test]
    fn negative_cause_gas_dominates_when_gas_exceeds_flashloan() {
        let cause = classify_negative_cause(Some(1.0), Some(0.8), Some(0.3), Some(-0.1));
        assert_eq!(cause, NegativeCause::GasDominates);
    }

    #[test]
    fn negative_cause_flashloan_dominates_when_flashloan_exceeds_gas() {
        let cause = classify_negative_cause(Some(1.0), Some(0.2), Some(0.9), Some(-0.1));
        assert_eq!(cause, NegativeCause::FlashloanFeeDominates);
    }

    #[test]
    fn negative_cause_other_when_components_unavailable() {
        // gross positive, net negative, but no gas/flashloan breakdown to
        // point at — must not fabricate a specific cause.
        let cause = classify_negative_cause(Some(1.0), None, None, Some(-0.1));
        assert_eq!(cause, NegativeCause::Other);
        assert_eq!(cause.as_str(), "NET_NON_POSITIVE_OTHER");
    }

    #[test]
    fn negative_cause_other_when_net_unavailable() {
        let cause = classify_negative_cause(Some(1.0), Some(0.5), Some(0.5), None);
        assert_eq!(cause, NegativeCause::Other);
    }

    #[test]
    fn fmt_opt_net_tiers() {
        assert_eq!(fmt_opt_net(Some(0.94)), "$0.94");
        assert_eq!(fmt_opt_net(Some(-0.03)), "$-0.03");
        assert_eq!(fmt_opt_net(Some(0.0)), "$0.00");
        assert_eq!(fmt_opt_net(None), "-");
        assert_eq!(fmt_opt_net(Some(f64::NAN)), "-");
    }
}
