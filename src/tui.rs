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
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Row, Table, TableState},
    Frame, Terminal,
};
use std::{
    io,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;

use crate::config::Config;

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
    pub venue_fee_adjusted: u32,
    pub negative_cycles: u32,
    pub last_prices: Vec<PriceRow>,
    pub recent_opps: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct PriceRow {
    pub pair: String,
    pub quickswap: Option<f64>,
    pub sushiswap: Option<f64>,
    pub curve: Option<f64>,
    pub uniswap_v3: Option<f64>,
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
            venue_fee_adjusted: 0,
            negative_cycles: 0,
            last_prices: Vec::new(),
            recent_opps: Vec::new(),
        }
    }
}

// ============================================================
// TUI App
// ============================================================

pub struct TuiApp {
    state: Arc<RwLock<TuiState>>,
    start_time: Instant,
}

impl TuiApp {
    pub fn new(state: Arc<RwLock<TuiState>>) -> Self {
        Self {
            state,
            start_time: Instant::now(),
        }
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
                Constraint::Length(3),   // Header
                Constraint::Length(6),   // Status
                Constraint::Min(8),     // Prices
                Constraint::Length(4),   // Footer
            ])
            .split(f.area());

        self.draw_header(f, chunks[0]);
        self.draw_status(f, chunks[1]);
        self.draw_prices(f, chunks[2]);
        self.draw_footer(f, chunks[3]);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("  DEX ARBITRAGE BOT", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw("  |  "),
                Span::styled("Dashboard v1.0", Style::default().fg(Color::Gray)),
            ]),
            Line::from(vec![
                Span::styled("  Polygon Mainnet", Style::default().fg(Color::Yellow)),
                Span::raw("  |  "),
                Span::styled("Flashloan Mode", Style::default().fg(Color::Green)),
                Span::raw("  |  Press "),
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
                Span::raw(format!("  |  Uptime: {}", uptime_str)),
            ])),
            ListItem::new(Line::from(vec![
                Span::styled("  DEXes: ", Style::default().fg(Color::Gray)),
                Span::styled(format!("{}", state.dex_count), Style::default().fg(Color::Cyan)),
                Span::raw(format!("  |  Pares: {}  |  Ciclos: {}", state.pairs_count, state.cycle_count)),
            ])),
            ListItem::new(Line::from(vec![
                Span::styled("  Econômico: ", Style::default().fg(Color::Gray)),
                Span::styled(format!("gross={}", state.gross_positive), Style::default().fg(if state.gross_positive > 0 { Color::Green } else { Color::Red })),
                Span::raw("  |  "),
                Span::styled(format!("fee_adj={}", state.venue_fee_adjusted), Style::default().fg(if state.venue_fee_adjusted > 0 { Color::Green } else { Color::Yellow })),
                Span::raw("  |  "),
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
            Cell::from("UniswapV3").style(Style::default().fg(Color::Blue)),
            Cell::from("Spread%").style(Style::default().fg(Color::Red)),
        ]);

        let mut rows: Vec<Row> = Vec::new();
        for p in &state.last_prices {
            let spread = if let (Some(q), Some(s)) = (p.quickswap, p.sushiswap) {
                ((s - q).abs() / q * 100.0)
            } else {
                0.0
            };

            rows.push(Row::new(vec![
                Cell::from(p.pair.clone()),
                Cell::from(format_opt(p.quickswap)),
                Cell::from(format_opt(p.sushiswap)),
                Cell::from(format_opt(p.curve)),
                Cell::from(format_opt(p.uniswap_v3)),
                Cell::from(format!("{:.4}%", spread)).style(
                    if spread > 0.5 { Style::default().fg(Color::Red) }
                    else if spread > 0.1 { Style::default().fg(Color::Yellow) }
                    else { Style::default().fg(Color::Gray) }
                ),
            ]));
        }

        let widths = [
            Constraint::Length(12),
            Constraint::Length(14),
            Constraint::Length(14),
            Constraint::Length(14),
            Constraint::Length(14),
            Constraint::Length(10),
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .block(Block::default().borders(Borders::ALL).title(" Preços Cross-DEX ").border_style(Style::default().fg(Color::Green)));

        f.render_widget(table, area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let footer = Paragraph::new(Line::from(vec![
            Span::styled("  Alchemy RPC", Style::default().fg(Color::Cyan)),
            Span::raw("  |  "),
            Span::styled("Aave V3 Flashloan", Style::default().fg(Color::Green)),
            Span::raw("  |  "),
            Span::styled("0.04% Curve", Style::default().fg(Color::Yellow)),
            Span::raw("  |  "),
            Span::styled("0.01%-1% UniswapV3", Style::default().fg(Color::Blue)),
        ]))
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Gray)));
        f.render_widget(footer, area);
    }

    fn blocking_read(&self) -> TuiState {
        // Simplified - in real impl would use async
        TuiState::default()
    }
}

fn format_opt(v: Option<f64>) -> String {
    match v {
        Some(p) if p > 100.0 => format!("{:.2}", p),
        Some(p) => format!("{:.8}", p),
        None => "-".to_string(),
    }
}

use ratatui::widgets::Cell;

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
