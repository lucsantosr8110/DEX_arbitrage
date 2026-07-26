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
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

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
    pub recent_opps: Vec<String>,
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
    pub pair: String,
    pub tui_spread_pct: f64,
    /// None = sem reverse cotado (cycle_rate indisponível).
    pub cycle_rate: Option<f64>,
    /// None = sem 2-hop.
    pub net_usd: Option<f64>,
    pub executable: bool,
    pub has_curve_leg: bool,
    pub outlier: Option<String>,
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
            recent_opps: Vec::new(),
        }
    }
}

// ============================================================
// TUI App
// ============================================================

pub struct TuiApp {
    state: Arc<RwLock<TuiState>>,
}

impl TuiApp {
    pub fn new(state: Arc<RwLock<TuiState>>) -> Self {
        Self { state }
    }

    pub async fn run(&self) -> io::Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let mut last_tick = Instant::now();
        let tick_rate = Duration::from_millis(250);

        loop {
            terminal.draw(|f| self.draw(f))?;

            let timeout = tick_rate
                .checked_sub(last_tick.elapsed())
                .unwrap_or(Duration::ZERO);

            if crossterm::event::poll(timeout)? {
                if let Event::Key(key) = event::read()? {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => break,
                        _ => {}
                    }
                }
            }

            if last_tick.elapsed() >= tick_rate {
                last_tick = Instant::now();
            }
        }

        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;
        Ok(())
    }

    fn draw(&self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5),   // Header (border + 3 lines de conteúdo)
                Constraint::Length(6),   // Status (border + 4 lines de conteúdo)
                Constraint::Min(10),     // Center: Prices | TopSpreads (split horizontal)
                Constraint::Length(4),   // Footer (border + 2 lines de conteúdo)
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
            Line::from(vec![
                Span::styled("  DEX ARBITRAGE BOT v1.0", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled("  Polygon", Style::default().fg(Color::Yellow)),
                Span::raw(" | "),
                Span::styled("Flashloan", Style::default().fg(Color::Green)),
            ]),
            Line::from(vec![
                Span::raw("  Press "),
                Span::styled("q", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::raw(" to quit"),
            ]),
        ])
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
        f.render_widget(header, area);
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let state = self.blocking_read();

        let uptime_str = format!("{:02}:{:02}:{:02}",
            state.uptime.as_secs() / 3600,
            (state.uptime.as_secs() % 3600) / 60,
            state.uptime.as_secs() % 60
        );

        let status_items = vec![
            ListItem::new(Line::from(vec![
                Span::styled("  Status: ", Style::default().fg(Color::Gray)),
                Span::styled("RUNNING", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::raw(" | "),
                Span::raw(format!("Uptime: {}", uptime_str)),
            ])),
            ListItem::new(Line::from(vec![
                Span::styled("  DEXes: ", Style::default().fg(Color::Gray)),
                Span::styled(format!("{}", state.dex_count), Style::default().fg(Color::Cyan)),
                Span::raw(" | "),
                Span::raw(format!("Pares: {}", state.pairs_count)),
                Span::raw(" | "),
                Span::raw(format!("Ciclos: {}", state.cycle_count)),
            ])),
            ListItem::new(Line::from(vec![
                Span::styled("  Econ: ", Style::default().fg(Color::Gray)),
                Span::styled(format!("gross={}", state.gross_positive), Style::default().fg(if state.gross_positive > 0 { Color::Green } else { Color::Red })),
                Span::raw(" | "),
                Span::styled(format!("net+={}", state.net_positive), Style::default().fg(if state.net_positive > 0 { Color::Green } else { Color::Yellow })),
                Span::raw(" | "),
                Span::styled(format!("net=${:.2}", state.net_usd_total), Style::default().fg(if state.net_usd_total > 0.0 { Color::Green } else if state.net_usd_total < 0.0 { Color::Red } else { Color::Gray })),
                Span::raw(" | "),
                Span::styled(format!("neg={}", state.negative_cycles), Style::default().fg(Color::Gray)),
            ])),
        ];

        let status_list = List::new(status_items)
            .block(Block::default().borders(Borders::ALL).title(" Status ").border_style(Style::default().fg(Color::Yellow)));
        f.render_widget(status_list, area);
    }

    fn draw_prices(&self, f: &mut Frame, area: Rect) {
        let state = self.blocking_read();

        let header = Row::new(vec![
            Cell::from("Pair").style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
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
                let max = venue_prices.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
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
                Cell::from(format!("{:.2}%", spread)).style(  // Reduzir para 2 casas decimais
                    if spread > 0.5 { Style::default().fg(Color::Red) }
                    else if spread > 0.1 { Style::default().fg(Color::Yellow) }
                    else { Style::default().fg(Color::Gray) }
                ),
                Cell::from(fmt_opt_net(p.net_usd)).style(
                    if matches!(p.net_usd, Some(n) if n > 0.0) { Style::default().fg(Color::Green) }
                    else if matches!(p.net_usd, Some(n) if n < 0.0) { Style::default().fg(Color::Red) }
                    else { Style::default().fg(Color::Gray) }
                ),
            ]));
        }

        let widths = [
            Constraint::Length(14),     // Pair
            Constraint::Length(13),     // QuickSwap
            Constraint::Length(13),     // SushiSwap
            Constraint::Length(13),     // Curve
            Constraint::Length(13),     // UniV3
            Constraint::Length(10),     // Spread%
            Constraint::Length(9),      // Net$
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .block(Block::default().borders(Borders::ALL).title(" Preços Cross-DEX ").border_style(Style::default().fg(Color::Green)));

        f.render_widget(table, area);
    }

    fn draw_top_spreads(&self, f: &mut Frame, area: Rect) {
        let state = self.blocking_read();

        let header = Row::new(vec![
            Cell::from("Pair").style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Cell::from("Spread%").style(Style::default().fg(Color::Red)),
            Cell::from("cyc").style(Style::default().fg(Color::Yellow)),
            Cell::from("Net$").style(Style::default().fg(Color::Green)),
            Cell::from("exec").style(Style::default().fg(Color::Gray)),
        ]);

        let mut rows: Vec<Row> = Vec::new();
        for t in &state.top_spreads {
            let pair_display = if t.pair.chars().count() > 10 {
                format!("{}..", t.pair.chars().take(10).collect::<String>())
            } else {
                t.pair.clone()
            };
            let cyc = match t.cycle_rate {
                Some(c) if c.is_finite() => format!("{:.4}", c),
                _ => "N/A".to_string(),
            };
            let exec = if t.has_curve_leg {
                "C".to_string() // perna Curve (vitrine)
            } else if t.executable {
                "y".to_string()
            } else {
                "n".to_string()
            };
            rows.push(Row::new(vec![
                Cell::from(pair_display),
                Cell::from(format!("{:.2}%", t.tui_spread_pct)),
                Cell::from(cyc),
                Cell::from(fmt_opt_net(t.net_usd)),
                Cell::from(exec),
            ]));
        }

        let widths = [
            Constraint::Length(13), // Pair
            Constraint::Length(9),  // Spread%
            Constraint::Length(9),  // cyc
            Constraint::Length(9),  // Net$
            Constraint::Length(6),  // exec
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .block(Block::default().borders(Borders::ALL).title(" Top Spreads ").border_style(Style::default().fg(Color::Magenta)));

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
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Gray)));
        f.render_widget(footer, area);
    }

    fn blocking_read(&self) -> TuiState {
        self.state.read().expect("tui state lock poisoned").clone()
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

/// Spawn the TUI in a background thread
pub fn spawn_tui(state: Arc<RwLock<TuiState>>) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let app = TuiApp::new(state);
            if let Err(e) = app.run().await {
                eprintln!("TUI error: {:?}", e);
            }
        });
    });
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

    #[test]
    fn fmt_opt_net_tiers() {
        assert_eq!(fmt_opt_net(Some(0.94)), "$0.94");
        assert_eq!(fmt_opt_net(Some(-0.03)), "$-0.03");
        assert_eq!(fmt_opt_net(Some(0.0)), "$0.00");
        assert_eq!(fmt_opt_net(None), "-");
        assert_eq!(fmt_opt_net(Some(f64::NAN)), "-");
    }
}
