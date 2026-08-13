// ============================================================
// src/main.rs — v4.8.4-HYBRID-SAFE (CORRIGIDO TYPE ERROR)
// ============================================================

use anyhow::{Context, Result};
use ethers::{
    providers::{Http, Middleware, Provider, Ws},
    types::{Address, H256, U256},
};
use futures::future;
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{debug, error, info, warn, Level};
use tracing_subscriber::{
    filter::LevelFilter,
    fmt::{self, writer::MakeWriterExt},
    prelude::*,
};

use flashloan_bot::{
    config::{Config, DiscoveryEngine},
    core::flashloan::ArbitrageClient,
    core::{
        bot::{execute_opportunity_standalone, should_try_next_opp, Bot},
        c2b_round::RoundEvidence,
        c2b_shadow_service::{should_schedule_anchor, CanonicalC2BOpportunitySource},
        canonical_adapters::PinnedQuoteRecord,
        canonical_discovery::{
            CanonicalDiscoveryConfig, CanonicalDiscoveryProfile, CanonicalDiscoveryService,
            CanonicalToken,
        },
        canonical_simulation::CanonicalSimulationClient,
        executable_call::Venue,
        execution_profile::{ExecutionProfile, MAIN_PENDING_DRY_RUN_PROFILE},
        phase2d_anchor::AnchorBlock,
        risk::{CanonicalRiskConfig, RiskManager},
        route_artifact::StructuralRoute,
    },
    dex::{
        circuit_breaker::DexCircuitBreaker,
        manager::DexManager,
        radar::{
            compute_top_spreads, extract_edges, start_high_hit_rate_radar, AdjCostParams,
            TopSpreadInfo,
        },
    },
    emergency_shutdown::{self},
    // execution:: imports removidos: ExecutionEngine/MevConfig/gwei eram codigo morto
    infra::{
        metrics,
        rpc_provider::{is_usable_endpoint, RpcProvider},
        try_serve_metrics_with_fallback,
    },
    tui,
    utils::telegram::TelegramNotifier,
};

/// Detecta modo headless (sem TUI): PAPER_VALIDATION, BOT_NO_TUI=1 ou
/// BOT_TUI=0 desligam a interface. Usado tanto no setup de logs quanto no
/// spawn da TUI para manter comportamento único.
fn headless_mode() -> bool {
    fn env_true(name: &str) -> bool {
        std::env::var(name)
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    }
    fn env_false(name: &str) -> bool {
        std::env::var(name)
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            })
            .unwrap_or(false)
    }
    env_true("BOT_NO_TUI")
        || env_false("BOT_TUI")
        || flashloan_bot::core::paper_validation::env_paper_flag()
}

// ============================================================
// 0️⃣.5 FUNÇÃO AUXILIAR PARA LOG DE CONFIGURAÇÃO
// ============================================================
fn log_config_snapshot(config: &Config) {
    info!("═══════════════════════════════════════════════════════════════════");
    info!("⚙️  CONFIGURAÇÃO DO BOT");
    info!("═══════════════════════════════════════════════════════════════════");
    info!(
        "  Versão: {}",
        config.general.version.as_deref().unwrap_or("unknown")
    );
    info!(
        "  Modo: {} | Dry Run: {}",
        if config.flashloan.enabled {
            "FLASHLOAN"
        } else {
            "DIRECT"
        },
        config.execution.dry_run
    );
    info!(
        "  Gas: priority={:.1} gwei, max={} gwei",
        config.gas.priority_gwei, config.gas.max_gwei
    );
    info!(
        "  Min Profit: ${:.4} | Min Spread: {}%",
        config
            .arbitrage
            .min_profit_absolute
            .parse::<f64>()
            .unwrap_or(0.0),
        config.arbitrage.min_spread_percent
    );
    info!("  Capital: ${}", config.flashloan.capital_usd);
    info!("═══════════════════════════════════════════════════════════════════");
}

/// Atualiza o estado compartilhado da TUI com os preços e a economia do ciclo atual.
fn update_tui_state(
    tui_state: &Arc<std::sync::RwLock<tui::TuiState>>,
    adj_cost: &AdjCostParams,
    prices: &HashMap<String, HashMap<String, f64>>,
    top_n: usize,
    cycle_count: u64,
) {
    let (_, _, economics, adj_cycles) = extract_edges(prices, adj_cost);

    // Net USD agregado: soma dos ciclos com net projetado > 0 (oportunidade real).
    let net_usd_total: f64 = adj_cycles
        .iter()
        .map(|c| c.net_profit_usd)
        .filter(|n| *n > 0.0)
        .sum();

    // Map canônico par → melhor net (p/ coluna Net$ da tabela de preços).
    // adj_cycles é deduped por canonical pair; norm_pair casa direção-agnóstica.
    let mut net_by_pair: HashMap<String, f64> = HashMap::new();
    for c in &adj_cycles {
        let key = tui::norm_pair(&c.pair);
        net_by_pair
            .entry(key)
            .and_modify(|v| {
                if c.net_profit_usd > *v {
                    *v = c.net_profit_usd;
                }
            })
            .or_insert(c.net_profit_usd);
    }

    let mut rows: HashMap<String, tui::PriceRow> = HashMap::new();
    for (dex, dex_map) in prices {
        for (pair, price) in dex_map {
            let row = rows.entry(pair.clone()).or_insert_with(|| tui::PriceRow {
                pair: pair.clone(),
                quickswap: None,
                sushiswap: None,
                curve: None,
                uniswap_v3: None,
                net_usd: net_by_pair.get(&tui::norm_pair(pair)).copied(),
            });
            match dex.as_str() {
                "QuickSwap" => row.quickswap = Some(*price),
                "SushiSwap" => row.sushiswap = Some(*price),
                "Curve" => row.curve = Some(*price),
                "UniswapV3" => row.uniswap_v3 = Some(*price),
                _ => {}
            }
        }
    }
    let mut last_prices: Vec<tui::PriceRow> = rows.into_values().collect();
    last_prices.sort_by(|a, b| a.pair.cmp(&b.pair));

    // Top-N spreads (sync, sem TVL) — espelha o log [TOPSPREAD].
    let top_spreads: Vec<tui::TopSpreadRow> = compute_top_spreads(prices, adj_cost, top_n)
        .into_iter()
        .map(top_spread_row_from_info)
        .collect();

    if let Ok(mut state) = tui_state.write() {
        state.running = true;
        state.cycle_count = cycle_count;
        state.dex_count = prices.len();
        state.pairs_count = last_prices.len();
        state.gross_positive = economics.gross_positive as u32;
        state.net_positive = economics.net_positive as u32;
        state.negative_cycles = economics.negative_cycles_found as u32;
        state.net_usd_total = net_usd_total;
        state.top_spreads = top_spreads;
        state.last_prices = last_prices;
        state.last_update = Some(std::time::Instant::now());
    }
}

/// Converte quotes canônicos reais em linhas de preço para a TUI.
/// A TUI é apresentação בלבד: nenhuma decisão de execução é tomada aqui.
fn canonical_price_rows(
    quotes: &[PinnedQuoteRecord],
    tokens: &[CanonicalToken],
) -> Vec<tui::PriceRow> {
    let min_roundtrip = std::env::var("CANONICAL_TUI_MIN_ROUNDTRIP_BPS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value <= 10_000)
        .unwrap_or(9_000) as f64
        / 10_000.0;
    let max_roundtrip = std::env::var("CANONICAL_TUI_MAX_ROUNDTRIP_BPS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value >= 10_000)
        .unwrap_or(10_500) as f64
        / 10_000.0;
    canonical_price_rows_with_bounds(quotes, tokens, min_roundtrip, max_roundtrip)
}

fn canonical_price_rows_with_bounds(
    quotes: &[PinnedQuoteRecord],
    tokens: &[CanonicalToken],
    min_roundtrip: f64,
    max_roundtrip: f64,
) -> Vec<tui::PriceRow> {
    let token_meta: HashMap<Address, (&str, u8)> = tokens
        .iter()
        .map(|token| (token.address, (token.symbol.as_str(), token.decimals)))
        .collect();
    // Multiple V3 fee tiers may exist for the same directed pair. Keep the
    // executable quote with the greatest output instead of whichever tier
    // happened to be inserted first.
    let mut best_rates: HashMap<(Address, Address, Venue), f64> = HashMap::new();

    for quote in quotes {
        let Some((_, decimals_in)) = token_meta.get(&quote.token_in) else {
            continue;
        };
        let Some((_, decimals_out)) = token_meta.get(&quote.token_out) else {
            continue;
        };
        let Ok(amount_in) = quote.amount_in.to_string().parse::<f64>() else {
            continue;
        };
        let Ok(amount_out) = quote.amount_out.to_string().parse::<f64>() else {
            continue;
        };
        let amount_in = amount_in / 10f64.powi(*decimals_in as i32);
        let amount_out = amount_out / 10f64.powi(*decimals_out as i32);
        if amount_in <= 0.0 || !amount_in.is_finite() || !amount_out.is_finite() {
            continue;
        }
        let price = amount_out / amount_in;
        if price > 0.0 && price.is_finite() {
            best_rates
                .entry((quote.token_in, quote.token_out, quote.venue))
                .and_modify(|current| *current = current.max(price))
                .or_insert(price);
        }
    }

    // A single-direction quote from a dust pool can be technically valid
    // while being useless as a market price. Require its best reverse quote
    // on the same venue to produce a sane round-trip ratio. Presentation
    // only: execution/economics continue to use exact integer quote chains.
    let mut rows: HashMap<String, tui::PriceRow> = HashMap::new();
    let mut filtered_outliers = 0usize;
    for ((token_in, token_out, venue), price) in &best_rates {
        let Some(reverse) = best_rates.get(&(*token_out, *token_in, *venue)) else {
            filtered_outliers += 1;
            continue;
        };
        let roundtrip = price * reverse;
        if !roundtrip.is_finite() || roundtrip < min_roundtrip || roundtrip > max_roundtrip {
            filtered_outliers += 1;
            continue;
        }
        let Some((symbol_in, _)) = token_meta.get(token_in) else {
            continue;
        };
        let Some((symbol_out, _)) = token_meta.get(token_out) else {
            continue;
        };
        let pair = format!("{}/{}", symbol_in, symbol_out);
        let row = rows.entry(pair.clone()).or_insert_with(|| tui::PriceRow {
            pair,
            quickswap: None,
            sushiswap: None,
            curve: None,
            uniswap_v3: None,
            net_usd: None,
        });
        match venue {
            Venue::QuickSwap => row.quickswap = Some(*price),
            Venue::SushiSwap => row.sushiswap = Some(*price),
            Venue::Curve => row.curve = Some(*price),
            Venue::UniswapV3 => row.uniswap_v3 = Some(*price),
        }
    }
    tracing::info!(
        target: "canonical_discovery",
        raw_quotes = quotes.len(),
        best_directed_rates = best_rates.len(),
        filtered_outliers,
        min_roundtrip,
        max_roundtrip,
        "canonical TUI quote normalization complete"
    );

    let mut rows: Vec<_> = rows.into_values().collect();
    rows.sort_by(|a, b| a.pair.cmp(&b.pair));
    rows
}

fn canonical_tui_economics(
    rows: &mut [tui::PriceRow],
    cost: &AdjCostParams,
    top_n: usize,
) -> (Vec<tui::TopSpreadRow>, f64, u32, u32) {
    let mut prices: HashMap<String, HashMap<String, f64>> = HashMap::new();
    for row in rows.iter() {
        let pair = row.pair.replace('/', "-");
        for (venue, price) in [
            ("QuickSwap", row.quickswap),
            ("SushiSwap", row.sushiswap),
            ("Curve", row.curve),
            ("UniswapV3", row.uniswap_v3),
        ] {
            if let Some(price) = price.filter(|value| value.is_finite() && *value > 0.0) {
                prices
                    .entry(venue.to_string())
                    .or_default()
                    .insert(pair.clone(), price);
            }
        }
    }

    let (_, _, economics, adj_cycles) = extract_edges(&prices, cost);
    let ranked = compute_top_spreads(&prices, cost, top_n);
    let mut net_by_pair: HashMap<String, f64> = HashMap::new();
    for combo in &ranked {
        if let Some(net) = combo.net_usd {
            let key = tui::norm_pair(&combo.pair);
            net_by_pair
                .entry(key)
                .and_modify(|current| *current = current.max(net))
                .or_insert(net);
        }
    }
    for row in rows {
        row.net_usd = net_by_pair
            .get(&tui::norm_pair(&row.pair.replace('/', "-")))
            .copied();
    }

    for (rank, combo) in ranked.iter().enumerate() {
        let leg1 = combo.leg1.as_ref();
        let leg2 = combo.leg2.as_ref();
        tracing::info!(
            target: "canonical_discovery",
            rank = rank + 1,
            pair = %combo.pair,
            buy_dex = %combo.buy_dex,
            sell_dex = %combo.sell_dex,
            leg1 = %leg1.map(|leg| format!(
                "{}:{}>{}@{:.12}",
                leg.venue, leg.token_in, leg.token_out, leg.rate
            )).unwrap_or_default(),
            leg2 = %leg2.map(|leg| format!(
                "{}:{}>{}@{:.12}",
                leg.venue, leg.token_in, leg.token_out, leg.rate
            )).unwrap_or_default(),
            cycle_rate = combo.cycle_rate.unwrap_or_default(),
            gross_pct = combo.gross_pct.unwrap_or_default(),
            net_usd = combo.net_usd.unwrap_or_default(),
            distance_to_profit = combo.distance_to_profit,
            executable = combo.executable,
            "CANONICAL_DIRECT_PAIR_COMBO"
        );
    }
    let top_spreads: Vec<tui::TopSpreadRow> =
        ranked.into_iter().map(top_spread_row_from_info).collect();
    let net_usd_total = adj_cycles
        .iter()
        .map(|cycle| cycle.net_profit_usd)
        .filter(|net| *net > 0.0)
        .sum();
    tracing::info!(
        target: "canonical_discovery",
        top_combos = top_spreads.len(),
        rows_with_net = net_by_pair.len(),
        net_usd_total,
        net_positive = economics.net_positive,
        negative_cycles = economics.negative_cycles_found,
        "canonical direct-pair TUI economics complete"
    );
    (
        top_spreads,
        net_usd_total,
        economics.net_positive as u32,
        economics.negative_cycles_found as u32,
    )
}

fn u256_ratio(numerator: U256, denominator: U256) -> Option<f64> {
    if denominator.is_zero() {
        return None;
    }
    let numerator = numerator.to_string().parse::<f64>().ok()?;
    let denominator = denominator.to_string().parse::<f64>().ok()?;
    let ratio = numerator / denominator;
    ratio.is_finite().then_some(ratio)
}

fn venue_abbreviation(venue: &str) -> &'static str {
    match venue {
        "QuickSwap" => "Q",
        "SushiSwap" => "S",
        "UniswapV3" => "U",
        "Curve" => "C",
        _ => "?",
    }
}

/// Authoritative canonical Top Combo projection. Unlike the direct-pair
/// diagnostic above, this consumes the same sequential Phase-B evidence and
/// cost-adjusted net used by stability/risk gates.
fn canonical_route_economics(
    evidences: &[RoundEvidence],
    routes: &BTreeMap<String, StructuralRoute>,
    tokens: &[CanonicalToken],
    cost: &AdjCostParams,
    top_n: usize,
) -> (Vec<tui::TopSpreadRow>, f64, u32, u32) {
    let token_meta: HashMap<Address, (&str, u8)> = tokens
        .iter()
        .map(|token| (token.address, (token.symbol.as_str(), token.decimals)))
        .collect();
    let mut ranked = Vec::new();

    for evidence in evidences {
        let Some(economics) = evidence.economics.as_ref() else {
            continue;
        };
        let Some(route) = routes.get(&evidence.structural_cycle_key) else {
            continue;
        };
        let Some(cycle_rate) = u256_ratio(economics.final_amount_atomic, evidence.amount_in) else {
            continue;
        };
        let gross_pct = (cycle_rate - 1.0) * 100.0;
        let net_fraction = economics.net_pnl_atomic as f64
            / evidence
                .amount_in
                .to_string()
                .parse::<f64>()
                .unwrap_or(f64::INFINITY);
        let net_usd = net_fraction * cost.notional_usd;
        if !net_usd.is_finite() {
            continue;
        }

        let mut token_path = Vec::new();
        if let Some(first) = route.legs.first() {
            token_path.push(first.token_in.clone());
            token_path.extend(route.legs.iter().map(|leg| leg.token_out.clone()));
        }
        let pair = token_path.join(">");
        let venues: Vec<String> = route.legs.iter().map(|leg| leg.venue.clone()).collect();
        let legs_label = venues
            .iter()
            .map(|venue| venue_abbreviation(venue))
            .collect::<Vec<_>>()
            .join("→");
        let exact_legs = route
            .legs
            .iter()
            .zip(&evidence.leg_quotes)
            .map(|(leg, quote)| {
                let (_, decimals_in) = token_meta.get(&quote.token_in).copied().unwrap_or(("?", 0));
                let (_, decimals_out) = token_meta
                    .get(&quote.token_out)
                    .copied()
                    .unwrap_or(("?", 0));
                let atomic_rate = u256_ratio(quote.amount_out, quote.amount_in).unwrap_or_default();
                let rate = atomic_rate * 10f64.powi(decimals_in as i32 - decimals_out as i32);
                format!(
                    "{}:{}>{}@{:.12}",
                    leg.venue, leg.token_in, leg.token_out, rate
                )
            })
            .collect::<Vec<_>>()
            .join(" | ");

        ranked.push((
            tui::TopSpreadRow {
                hop_count: route.legs.len(),
                pair,
                tui_spread_pct: gross_pct,
                buy_dex: venues.first().cloned().unwrap_or_default(),
                sell_dex: venues.last().cloned().unwrap_or_default(),
                legs_label: Some(legs_label),
                cycle_rate: Some(cycle_rate),
                net_usd: Some(net_usd),
                distance_to_profit: (-net_usd).max(0.0),
                executable: true,
                has_curve_leg: venues.iter().any(|venue| venue == "Curve"),
                outlier: None,
            },
            evidence.structural_cycle_key.clone(),
            exact_legs,
            economics.gross_pnl_atomic,
            economics.net_pnl_atomic,
        ));
    }

    ranked.sort_by(|a, b| {
        b.0.net_usd
            .unwrap_or(f64::NEG_INFINITY)
            .partial_cmp(&a.0.net_usd.unwrap_or(f64::NEG_INFINITY))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    let net_positive = ranked
        .iter()
        .filter(|(row, ..)| row.net_usd.is_some_and(|net| net > 0.0))
        .count() as u32;
    let negative_cycles = ranked.len() as u32 - net_positive;
    let net_usd_total = ranked
        .iter()
        .filter_map(|(row, ..)| row.net_usd.filter(|net| *net > 0.0))
        .sum();

    for (rank, (row, key, legs, gross_atomic, net_atomic)) in ranked.iter().take(top_n).enumerate()
    {
        info!(
            target: "canonical_discovery",
            rank = rank + 1,
            structural_cycle_key = %key,
            route = %row.pair,
            legs = %legs,
            cycle_rate = row.cycle_rate.unwrap_or_default(),
            gross_pct = row.tui_spread_pct,
            net_usd = row.net_usd.unwrap_or_default(),
            gross_pnl_atomic = gross_atomic,
            net_pnl_atomic = net_atomic,
            executable = row.executable,
            "CANONICAL_TOP_COMBO"
        );
    }
    info!(
        target: "canonical_discovery",
        routes_evaluated = ranked.len(),
        net_positive,
        negative_cycles,
        net_usd_total,
        "canonical route economics projection complete"
    );

    (
        ranked
            .into_iter()
            .take(top_n)
            .map(|(row, ..)| row)
            .collect(),
        net_usd_total,
        net_positive,
        negative_cycles,
    )
}

fn combine_top_combo_rows(
    mut two_leg: Vec<tui::TopSpreadRow>,
    triangular: Vec<tui::TopSpreadRow>,
) -> Vec<tui::TopSpreadRow> {
    two_leg.extend(triangular);
    two_leg.sort_by(|a, b| {
        b.net_usd
            .unwrap_or(f64::NEG_INFINITY)
            .partial_cmp(&a.net_usd.unwrap_or(f64::NEG_INFINITY))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.hop_count.cmp(&b.hop_count))
            .then_with(|| a.pair.cmp(&b.pair))
    });
    two_leg
}

/// Mapeia `TopSpreadInfo` (radar, sync) → `TopSpreadRow` (TUI, subset sem TVL).
fn top_spread_row_from_info(i: TopSpreadInfo) -> tui::TopSpreadRow {
    tui::TopSpreadRow {
        hop_count: 2,
        pair: i.pair,
        tui_spread_pct: i.tui_spread_pct,
        buy_dex: i.buy_dex,
        sell_dex: i.sell_dex,
        legs_label: None,
        cycle_rate: i.cycle_rate,
        net_usd: i.net_usd,
        distance_to_profit: i.distance_to_profit,
        executable: i.executable,
        has_curve_leg: i.has_curve_leg,
        outlier: i.outlier,
    }
}

/// Guarda o JoinHandle da TUI e faz join com timeout no Drop. Isso garante
/// que, em qualquer caminho de saída do main (Ok, Err, early return), o
/// terminal seja restaurado antes do processo morrer. Sem isso, erros de
/// startup deixavam a TUI thread órfã em raw mode.
struct TuiGuard {
    handle: Option<std::thread::JoinHandle<()>>,
}

/// Envia broadcast de shutdown quando dropped, assegurando que a thread da
/// TUI saia do loop e execute o cleanup do terminal mesmo em caminhos de
/// erro (retornos com `?`) que não dispararam shutdown explicitamente.
struct ShutdownOnDrop(broadcast::Sender<()>);

impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

impl TuiGuard {
    fn join_with_timeout(&mut self, timeout: Duration) {
        if let Some(handle) = self.handle.take() {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = handle.join();
                let _ = tx.send(());
            });
            if rx.recv_timeout(timeout).is_err() {
                warn!("⚠️ TUI não finalizou em {:?} — prosseguindo.", timeout);
            }
        }
    }
}

impl Drop for TuiGuard {
    fn drop(&mut self) {
        self.join_with_timeout(Duration::from_secs(5));
    }
}

/// Restaura o terminal quando shutdown ocorre durante startup. A TUI thread
/// já faz seu próprio cleanup ao sair do run_inner; este helper garante que
/// esperemos ela por um curto prazo e registremos o estado de erro.
fn graceful_startup_cleanup(
    tui_guard: &mut TuiGuard,
    tui_state: Arc<std::sync::RwLock<tui::TuiState>>,
) -> Result<()> {
    if let Ok(mut s) = tui_state.write() {
        s.mark_startup_error("Shutdown solicitado durante inicialização".into());
    }
    tui_guard.join_with_timeout(Duration::from_secs(3));
    Ok(())
}

/// Canonical branch: no wallet, signer middleware, ArbitrageClient,
/// broadcaster, approval, or transaction sender is ever constructed here.
/// This is the *sole* execution authority when `DISCOVERY_ENGINE=canonical`:
/// `main()` returns straight into this loop before the legacy
/// `PRIVATE_KEY`/`ArbitrageEngine`/`select_opportunities`/execution path is
/// ever reached, so there is no route back into legacy from here — a
/// rejected or failed canonical round is logged and dropped, never
/// retried against the legacy engine.
async fn run_canonical_mode(
    provider: Arc<Provider<Http>>,
    cfg: Arc<Config>,
    adj_cost: Arc<AdjCostParams>,
    every_n_blocks: u64,
    round_timeout: Duration,
    tui_state: Arc<std::sync::RwLock<tui::TuiState>>,
    mut shutdown_rx: broadcast::Receiver<()>,
    tui_guard: &mut TuiGuard,
) -> Result<()> {
    let profile = match cfg.c2b_shadow.canonical_discovery_profile.as_str() {
        "liquid" => CanonicalDiscoveryProfile::Liquid,
        _ => CanonicalDiscoveryProfile::Base,
    };
    let discovery_config = CanonicalDiscoveryConfig::from_config(
        &cfg,
        profile,
        ExecutionProfile {
            chain_id: 137,
            profile_label: MAIN_PENDING_DRY_RUN_PROFILE.into(),
        },
    )
    .context("CANONICAL_DISCOVERY_CONFIG_INVALID")?;
    info!(
        profile = discovery_config.profile.label(),
        token_count = discovery_config.token_count(),
        "CANONICAL_DISCOVERY_PROFILE_RESOLVED"
    );
    let venue_count = discovery_config.venues.len();
    let canonical_tokens = discovery_config.tokens.clone();
    let top_n = cfg.log.top_spreads_n;
    let service = CanonicalDiscoveryService::new(provider.clone(), 137, discovery_config);

    // Dry-run-only thresholds. This phase never reaches a send/broadcast
    // call under any strategy decision, so a mis-tuned economic threshold
    // here only ever gates a pending `eth_call` simulation, never funds.
    let risk_manager = RiskManager::new(cfg.risk.clone());
    let canonical_risk_cfg = CanonicalRiskConfig {
        absolute_min_profit_floor_raw: U256::zero(),
        retention_bps: 0,
        max_gas_raw: U256::from(50_000_000u64),
        max_slippage_bps: 500,
        max_anchor_age_blocks: 256,
    };
    // `executor_address` is a plain configured contract/EOA address used
    // only as the pending `eth_call` `from` — never a wallet, never derived
    // from a private key.
    let executor_address: Address = cfg
        .flashloan
        .executor_address
        .clone()
        .unwrap_or_default()
        .parse()
        .unwrap_or_default();
    let simulation_client = CanonicalSimulationClient::new(provider.clone(), executor_address);
    let mut opportunity_source =
        CanonicalC2BOpportunitySource::new(risk_manager, simulation_client, canonical_risk_cfg);

    let mut last_scheduled: Option<(u64, H256)> = None;
    let mut ticks = tokio::time::interval(Duration::from_secs(2));
    let mut rounds_completed = 0u64;
    info!(
        "CANONICAL_STARTUP_WITHOUT_SIGNER=true CANONICAL_STARTUP_WITHOUT_BROADCASTER=true \
         CANONICAL_MAX_CONCURRENT_ROUNDS=1 CANONICAL_PENDING_ANCHORS_MAX=1"
    );
    let result = 'worker: loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                info!("CANONICAL_SHUTDOWN_CANCELS_WORKER=true");
                break 'worker Ok(());
            }
            _ = ticks.tick() => {
                let block_number_result = tokio::select! {
                    biased;
                    _ = shutdown_rx.recv() => {
                        info!("CANONICAL_SHUTDOWN_CANCELS_BLOCK_POLL=true");
                        break 'worker Ok(());
                    }
                    result = provider.get_block_number() => result,
                };
                let number = match block_number_result {
                    Ok(number) => number.as_u64(),
                    Err(error) => { warn!(error = %error, "canonical block poll failed"); continue; }
                };
                let block_result = tokio::select! {
                    biased;
                    _ = shutdown_rx.recv() => {
                        info!("CANONICAL_SHUTDOWN_CANCELS_BLOCK_READ=true");
                        break 'worker Ok(());
                    }
                    result = provider.get_block(number) => result,
                };
                let block = match block_result {
                    Ok(Some(block)) => block,
                    Ok(None) => continue,
                    Err(error) => { warn!(error = %error, "canonical get_block failed"); continue; }
                };
                let Some(hash) = block.hash else { continue; };

                // A reorg at the last-scheduled block number invalidates
                // any anchor we might otherwise re-derive from it.
                let reorg_detected = last_scheduled
                    .is_some_and(|(last_number, last_hash)| number == last_number && hash != last_hash);
                if reorg_detected {
                    warn!(block = number, "CANONICAL_REORG_DETECTED");
                }

                let anchor = AnchorBlock { number, hash, selected_from_head: number, confirmation_lag: 0 };
                if !should_schedule_anchor(
                    last_scheduled.map(|(scheduled_number, _)| scheduled_number),
                    &anchor,
                    every_n_blocks.max(1),
                    reorg_detected,
                ) {
                    debug!(block = number, "CANONICAL_SKIP_REASON=PREVIOUS_ANCHOR_TOO_RECENT_OR_REORG");
                    continue;
                }
                last_scheduled = Some((number, hash));

                // Once a `select!` branch is chosen, its handler runs to
                // completion; the outer shutdown branch cannot interrupt an
                // in-flight discovery. Keep shutdown in the same select as
                // the round future so all pending RPC/quote work is dropped
                // immediately when the broadcast arrives.
                let discovery_result = tokio::select! {
                    biased;
                    _ = shutdown_rx.recv() => {
                        info!("CANONICAL_SHUTDOWN_CANCELS_IN_FLIGHT_ROUND=true");
                        break 'worker Ok(());
                    }
                    result = tokio::time::timeout(
                        round_timeout,
                        service.discover_at(anchor.clone()),
                    ) => result,
                };
                match discovery_result {
                    Ok(Ok(result)) => {
                        rounds_completed += 1;
                        let round_evidence_count = result.round_evidence.len();
                        let economically_positive = result.economically_positive.len();
                        let mut last_prices = canonical_price_rows(&result.initial_quotes, &canonical_tokens);
                        // Preserve direct-pair net on price rows as a diagnostic,
                        // but source Top Combo and counters from authoritative
                        // sequential canonical route evidence.
                        let (
                            two_leg_spreads,
                            two_leg_net_usd_total,
                            two_leg_net_positive,
                            two_leg_negative_cycles,
                        ) = canonical_tui_economics(&mut last_prices, &adj_cost, top_n);
                        let (
                            triangular_spreads,
                            canonical_net_usd_total,
                            canonical_net_positive,
                            canonical_negative_cycles,
                        ) =
                            canonical_route_economics(
                                &result.round_evidence,
                                &result.structural_routes,
                                &canonical_tokens,
                                &adj_cost,
                                top_n,
                            );
                        let two_leg_displayed = two_leg_spreads.len();
                        let triangular_displayed = triangular_spreads.len();
                        let top_spreads =
                            combine_top_combo_rows(two_leg_spreads, triangular_spreads);
                        let net_usd_total =
                            two_leg_net_usd_total + canonical_net_usd_total;
                        let net_positive =
                            two_leg_net_positive.saturating_add(canonical_net_positive);
                        let negative_cycles =
                            two_leg_negative_cycles.saturating_add(canonical_negative_cycles);
                        let economics_consistent =
                            economically_positive == canonical_net_positive as usize;
                        if !economics_consistent {
                            error!(
                                discovery_net_positive = economically_positive,
                                canonical_projection_net_positive = canonical_net_positive,
                                "CANONICAL_ECONOMICS_DIVERGENCE_FAIL_CLOSED"
                            );
                        }
                        info!(
                            two_leg_displayed,
                            triangular_displayed,
                            combined_displayed = top_spreads.len(),
                            "CANONICAL_TUI_ROUTE_TYPES_READY"
                        );
                        if let Ok(mut state) = tui_state.write() {
                            state.running = true;
                            state.cycle_count = rounds_completed;
                            state.dex_count = venue_count;
                            state.pairs_count = last_prices.len();
                            state.gross_positive = result
                                .round_evidence
                                .iter()
                                .filter(|evidence| {
                                    evidence.gross_pnl_atomic.is_some_and(|gross| gross > 0)
                                })
                                .count() as u32;
                            state.net_positive = net_positive;
                            state.negative_cycles = negative_cycles;
                            state.net_usd_total = net_usd_total;
                            state.last_prices = last_prices;
                            state.top_spreads = top_spreads;
                            state.last_update = Some(std::time::Instant::now());
                        }
                        let evidence_for_shadow = if economics_consistent {
                            result.round_evidence
                        } else {
                            Vec::new()
                        };
                        let shadow_result = opportunity_source
                            .run_evidence_round(anchor, evidence_for_shadow, &cfg, number, true)
                            .await;
                        info!(
                            round = rounds_completed,
                            anchor = number,
                            round_evidence = round_evidence_count,
                            economically_positive,
                            economics_consistent,
                            stable_opportunities = shadow_result.stable_opportunities.len(),
                            risk_approved = shadow_result.risk_approvals.iter().filter(|(_, r)| r.is_ok()).count(),
                            strategies_selected = shadow_result.strategy_decisions.len(),
                            dry_run_results = shadow_result.execution_results.len(),
                            "CANONICAL_ROUND_COMPLETE"
                        );
                    }
                    Ok(Err(error)) => warn!(error = %error, "canonical round rejected; no legacy fallback"),
                    Err(_) => warn!("canonical round timed out; no legacy fallback"),
                }
            }
        }
    };
    info!("CANONICAL_WORKER_STOPPED=true");
    tui_guard.join_with_timeout(Duration::from_secs(3));
    metrics::set_bot_status(0);
    info!("CANONICAL_SHUTDOWN_COMPLETE=true");
    result
}

// ============================================================
// MAIN
// ============================================================
#[tokio::main]
async fn main() -> Result<()> {
    // ============================================================
    // 1️⃣ Logging e Inicialização
    // ============================================================
    let use_json_logs = std::env::var("BOT_JSON_LOGS")
        .unwrap_or_else(|_| "false".into())
        .to_lowercase()
        == "true";

    let env_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());
    let headless = headless_mode();

    // TUI ocupa terminal inteiro (alternate screen). Logs direto no stdout
    // colidem com o buffer da TUI e aparecem como texto solto fora das boxes.
    // Por isso logs vão só pro arquivo quando a TUI roda. Em headless espelha
    // também no stdout para o operador acompanhar o startup.
    std::fs::create_dir_all("logs").context("❌ Falha ao criar diretório logs/")?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("logs/bot.log")
        .context("❌ Falha ao abrir logs/bot.log")?;

    if use_json_logs {
        tracing_subscriber::fmt()
            .json()
            .with_current_span(false)
            .with_target(false)
            .with_writer(std::sync::Mutex::new(log_file))
            .init();
    } else {
        let filter = tracing_subscriber::EnvFilter::builder()
            .with_default_directive(LevelFilter::INFO.into())
            .parse(env_filter)
            .context("❌ Falha ao parsear RUST_LOG")?;

        let writer = std::sync::Mutex::new(log_file).with_max_level(Level::INFO);

        let fmt_layer = fmt::layer()
            .compact()
            .with_ansi(false)
            .with_target(false)
            .with_writer(writer);

        let stdout_layer = if headless {
            let stdout_writer =
                std::sync::Mutex::new(std::io::stdout()).with_max_level(Level::INFO);
            Some(
                fmt::layer()
                    .compact()
                    .with_ansi(false)
                    .with_target(false)
                    .with_writer(stdout_writer),
            )
        } else {
            None
        };

        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .with(stdout_layer)
            .init();
    }

    if headless {
        info!("🧾 Headless: logs em logs/bot.log + stdout.");
    } else {
        info!("🧾 Logging habilitado em logs/bot.log (TUI usa terminal).");
    }

    info!("🚀 Iniciando Flashloan DEX Arbitrage Bot v4.8.4-HYBRID-SAFE...");
    flashloan_bot::core::pipeline_obs::print_diagnostic_banner();

    // ============================================================
    // 2️⃣ Carregamento de Configuração e Variáveis (.env)
    // ============================================================
    let env_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".env");
    if dotenvy::from_path(&env_path).is_err() {
        if dotenvy::dotenv().is_err() {
            warn!("⚠️ Não foi possível carregar o arquivo .env.");
        } else {
            info!("✅ Variáveis de ambiente carregadas do .env padrão.");
        }
    } else {
        info!("✅ Variáveis de ambiente carregadas do arquivo .env no diretório raiz do Cargo.");
    }

    let config_path: PathBuf = {
        let from_env = std::env::var("CONFIG_FILE").ok();
        let p = from_env.unwrap_or_else(|| "config/config.toml".to_string());
        PathBuf::from(p.replace('\\', "/"))
    };
    info!(
        "🧩 Usando arquivo de configuração: {}",
        config_path.display()
    );

    let config = Config::from_file(config_path.clone()).with_context(|| {
        format!(
            "❌ Falha ao ler o arquivo de configuração: {}",
            config_path.display()
        )
    })?;
    let cfg_unlocked = {
        let lock = config.lock().await;
        Arc::new(lock.clone())
    };

    // Custo config-driven p/ projetar net dos ciclos `adj` (log + TUI). Fonte única:
    // notional do [arbitrage].default_trade_amount, premium Aave V3 verificado
    // on-chain ([flashloan].fee_pct, 5 bps), gas base ([execution].estimate_base_gas_usd).
    // PROJEÇÃO — não é decisão de execução (desconto real é downstream em economics/flashloan).
    let adj_cost = Arc::new(AdjCostParams {
        notional_usd: cfg_unlocked
            .arbitrage
            .default_trade_amount
            .parse::<f64>()
            .unwrap_or(100.0),
        flashloan_fee_pct: cfg_unlocked.flashloan.fee_pct.unwrap_or(0.0005),
        gas_usd_est: cfg_unlocked.execution.estimate_base_gas_usd,
    });
    info!(
        "🧮 adj cost params | notional=${:.2} flashloan_fee_pct={:.5} gas_est=${:.4} → cost/cycle=${:.4}",
        adj_cost.notional_usd,
        adj_cost.flashloan_fee_pct,
        adj_cost.gas_usd_est,
        adj_cost.cost_usd(),
    );

    // ============================================================
    // 3️⃣ Telegram Notifier
    // ============================================================
    let telegram = match TelegramNotifier::init_from_config(&cfg_unlocked).await {
        Ok(tg) => Arc::new(tg),
        Err(e) => {
            warn!("⚠️ TelegramNotifier falhou: {}", e);
            Arc::new(TelegramNotifier::disabled())
        }
    };

    log_config_snapshot(&cfg_unlocked);

    // ============================================================
    // 3️⃣.5 Canal de shutdown + watchdog de emergência
    // ============================================================
    // Criado o mais cedo possível para que TUI, metrics, health-checker e
    // listener Ctrl+C independente possam se inscrever.
    let (shutdown_tx, _) = broadcast::channel::<()>(4);
    let _shutdown_on_drop = ShutdownOnDrop(shutdown_tx.clone());
    let emergency_grace = Duration::from_secs(
        std::env::var("BOT_EMERGENCY_SHUTDOWN_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value >= 5)
            .unwrap_or(30),
    );
    emergency_shutdown::spawn_emergency_watchdog(emergency_grace);

    // ── TUI sobe o mais cedo possível para dar feedback de startup ──
    // Antes a TUI só aparecia depois de HTTP/WS/DexManager/Bot, então o
    // operador via uma tela preta por segundos (ou indefinidamente se RPC
    // travasse). Agora mostramos splash screen com a fase de inicialização.
    let tui_state = Arc::new(std::sync::RwLock::new(tui::TuiState::default()));
    let tui_enabled = !headless;
    let mut tui_handle = TuiGuard {
        handle: if tui_enabled {
            Some(tui::spawn_tui(tui_state.clone(), shutdown_tx.clone()))
        } else {
            info!("📄 Headless: TUI desabilitado (logs em logs/bot.log)");
            None
        },
    };

    fn tui_phase(state: &Arc<std::sync::RwLock<tui::TuiState>>, phase: &str) {
        if let Ok(mut s) = state.write() {
            s.set_startup_phase(phase);
        }
    }

    // Listener Ctrl+C em runtime separado: se o runtime principal travar em RPC,
    // este thread ainda consegue receber o sinal. Agora ele primeiro tenta o
    // shutdown gracioso via broadcast; só ativa a saída de emergência depois
    // de uma janela maior, evitando que um Ctrl+C casual mate o processo em 5s.
    {
        let shutdown_tx = shutdown_tx.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            if let Ok(rt) = rt {
                rt.block_on(async {
                    let _ = tokio::signal::ctrl_c().await;
                    warn!("🛑 Ctrl+C antecipado — solicitando shutdown gracioso.");
                    let _ = shutdown_tx.send(());
                    // Arm immediately; the watchdog itself owns the grace
                    // window, so there is a single, observable timeout.
                    emergency_shutdown::request_emergency_shutdown();
                });
            }
            // watchdog cuida do exit forçado
        });
    }

    // ============================================================
    // 3️⃣.6 Servidor Prometheus (métricas)
    // ============================================================
    // Antes o servidor de métricas nunca era iniciado — Infrastructure::initialize()
    // não era chamado do main.rs, e toda a camada de observabilidade era decorativa.
    // Agora chamamos diretamente: inicia warp em [prometheus].port (fallback 9101).
    if cfg_unlocked.prometheus.enabled || cfg_unlocked.metrics.enabled {
        if let Err(e) = try_serve_metrics_with_fallback(&cfg_unlocked, shutdown_tx.clone()).await {
            warn!("⚠️ Falha ao iniciar servidor Prometheus: {}", e);
        }
    } else {
        info!("📊 Prometheus desativado via config.");
    }

    // ============================================================
    // 4️⃣ RPC Providers (HTTP e WS)
    // ============================================================
    // Fonte única de endpoints: o `.env` sobrepõe o TOML quando presente, senão o
    // bloco [network] do config manda. Antes o `.env` era obrigatório e o TOML era
    // silenciosamente ignorado — duas fontes de verdade para a mesma coisa.
    let rpc_endpoints: Vec<String> = match std::env::var("BOT_RPC_ENDPOINTS") {
        Ok(raw) if !raw.trim().is_empty() => {
            let list: Vec<String> = raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            info!(
                "🌐 Endpoints RPC vindos de BOT_RPC_ENDPOINTS ({} entradas).",
                list.len()
            );
            list
        }
        _ => {
            let list = cfg_unlocked
                .network
                .rpc_endpoints
                .clone()
                .unwrap_or_default();
            info!(
                "🌐 BOT_RPC_ENDPOINTS ausente — usando [network].rpc_endpoints do config ({} entradas).",
                list.len()
            );
            list
        }
    };

    if !rpc_endpoints.iter().any(|u| is_usable_endpoint(u)) {
        anyhow::bail!(
            "❌ Nenhum endpoint RPC utilizável. Defina BOT_RPC_ENDPOINTS no .env ou \
             [network].rpc_endpoints no config (placeholders ${{VAR}} não resolvidos não contam)."
        );
    }

    // Incompatible canonical configuration fails startup outright — never a
    // silent downgrade to a smaller scope or a silent fallback to legacy.
    // A no-op when the resolved engine is `Legacy`.
    cfg_unlocked
        .c2b_shadow
        .validate_canonical_startup()
        .context("❌ CANONICAL_STARTUP_CONFIG_INVALID")?;

    // Decide engine before touching PRIVATE_KEY. Canonical has no route to
    // the legacy SignerMiddleware bootstrap below.
    if DiscoveryEngine::resolve(&cfg_unlocked.c2b_shadow.discovery_engine)
        == DiscoveryEngine::Canonical
    {
        let endpoint = rpc_endpoints
            .iter()
            .find(|endpoint| is_usable_endpoint(endpoint))
            .ok_or_else(|| anyhow::anyhow!("canonical mode has no usable read-only RPC"))?;
        let provider = Arc::new(Provider::<Http>::try_from(endpoint.as_str())?);
        if let Ok(mut state) = tui_state.write() {
            state.set_startup_phase("descoberta canônica em execução...");
            state.mark_startup_done();
        }
        return run_canonical_mode(
            provider,
            cfg_unlocked.clone(),
            adj_cost.clone(),
            cfg_unlocked.c2b_shadow.shadow_every_n_blocks,
            Duration::from_secs(cfg_unlocked.c2b_shadow.round_timeout_secs.max(1)),
            tui_state.clone(),
            shutdown_tx.subscribe(),
            &mut tui_handle,
        )
        .await;
    }

    let private_key = std::env::var("PRIVATE_KEY").context("❌ PRIVATE_KEY ausente no .env")?;

    tui_phase(&tui_state, "conectando RPC HTTP...");
    let client_http = {
        let mut sd_rx = shutdown_tx.subscribe();
        tokio::select! {
            _ = sd_rx.recv() => {
                info!("🔌 Shutdown durante conexão HTTP — abortando startup.");
                return graceful_startup_cleanup(&mut tui_handle, tui_state);
            }
            res = RpcProvider::connect_http_with_fallback(
                &cfg_unlocked.network,
                &private_key,
                &rpc_endpoints,
            ) => res.context("❌ Falha ao conectar via HTTP (fallback esgotado)")?,
        }
    };

    let chain_id = client_http.get_chainid().await?.as_u64();
    info!("🌐 RPC HTTP conectado (chain_id = {}).", chain_id);

    tui_phase(&tui_state, "conectando WebSocket...");
    let client_ws: Arc<Provider<Ws>> = {
        let mut sd_rx = shutdown_tx.subscribe();
        tokio::select! {
            _ = sd_rx.recv() => {
                info!("🔌 Shutdown durante conexão WS — abortando startup.");
                return graceful_startup_cleanup(&mut tui_handle, tui_state);
            }
            res = RpcProvider::connect_ws(&cfg_unlocked.network) => {
                res.context("❌ Falha ao conectar via WebSocket")?
            }
        }
    };
    info!("✅ WebSocket conectado.");

    // ============================================================
    // 5️⃣ DexManager
    // ============================================================
    tui_phase(&tui_state, "inicializando DexManager...");
    let dex_manager = Arc::new({
        let mut sd_rx = shutdown_tx.subscribe();
        tokio::select! {
            _ = sd_rx.recv() => {
                info!("🔌 Shutdown durante DexManager — abortando startup.");
                return graceful_startup_cleanup(&mut tui_handle, tui_state);
            }
            res = DexManager::new(client_http.clone(), cfg_unlocked.clone()) => {
                res.context("❌ Falha ao inicializar DexManager")?
            }
        }
    });
    info!("🧩 DexManager inicializado com sucesso.");
    // Health-checker como task sinalizada: antes era tokio::spawn detached
    // sem shutdown_rx (leaked até o processo morrer). Agora recebe broadcast
    // e sai limpo no shutdown.
    tui_phase(&tui_state, "iniciando health checker...");
    {
        let mut sd_rx = shutdown_tx.subscribe();
        tokio::select! {
            _ = sd_rx.recv() => {
                info!("🔌 Shutdown durante health checker — abortando startup.");
                return graceful_startup_cleanup(&mut tui_handle, tui_state);
            }
            _ = dex_manager.start_health_checker(shutdown_tx.subscribe()) => {}
        }
    }

    // ============================================================
    // 6️⃣ ExecutionEngine removido (codigo morto)
    // ============================================================
    // ExecutionEngine/MevConfig/BundleSender eram construidos aqui e
    // descartados (_execution_engine). O bot executa via ArbitrageClient
    // diretamente (execute_direct/execute_flashloan/execute_wrapper).
    // Ver ESTADO_ATUAL.md secao 6.

    // ============================================================
    // 7️⃣ Circuit Breaker
    // ============================================================
    let circuit_breaker = Arc::new(DexCircuitBreaker::new(5, 30));

    // ============================================================
    // 8️⃣ Inicialização do Bot
    // ============================================================
    tui_phase(&tui_state, "inicializando Bot...");
    let bot = {
        let mut sd_rx = shutdown_tx.subscribe();
        tokio::select! {
            _ = sd_rx.recv() => {
                info!("🔌 Shutdown durante inicialização do Bot — abortando startup.");
                return graceful_startup_cleanup(&mut tui_handle, tui_state);
            }
            res = async {
                match Bot::init_with_engine(client_http.clone(), config.clone(), telegram.clone(), None).await {
                    Ok(bot) => bot,
                    Err(e) => {
                        warn!("⚠️ Bot::init_with_engine() falhou: {:?}", e);
                        Bot::new_with_engine(client_http.clone(), config.clone(), telegram.clone(), None).await
                    }
                }
            } => res
        }
    };

    let bot = Arc::new(Mutex::new(bot));

    // ============================================================
    // 8️⃣.5 Replay scan (paper-only): varre blocos históricos e sai
    // ============================================================
    if flashloan_bot::core::replay_scan::should_run(&cfg_unlocked) {
        info!("📼 REPLAY_SCAN ativo — paper-only, sem radar/envio");
        let summary = {
            let guard = bot.lock().await;
            flashloan_bot::core::replay_scan::run(
                client_http.clone(),
                cfg_unlocked.clone(),
                guard.get_arbitrage_engine(),
                &guard.arbitrage_client,
            )
            .await
            .context("❌ replay_scan falhou")?
        };
        info!(
            target: "replay_scan",
            "📼 REPLAY_SCAN done | n={} edge={} sim_ok={} lucrativos={}",
            summary.n_blocos_amostrados,
            summary.n_blocos_com_edge,
            summary.n_sim_ok,
            summary.n_blocos_lucrativos_pos_custos
        );
        return Ok(());
    }

    // ============================================================
    // 9️⃣ Canais e Radar
    // ============================================================
    let (price_tx, price_rx) = mpsc::channel::<HashMap<String, HashMap<String, f64>>>(256);
    let price_rx = Arc::new(Mutex::new(price_rx));

    let radar_task = {
        let mut client_ws = client_ws.clone();
        let dex_manager = dex_manager.clone();
        let config = config.clone();
        let adj_cost = adj_cost.clone();
        let price_tx = price_tx.clone();
        let circuit_breaker = circuit_breaker.clone();
        let shutdown_tx = shutdown_tx.clone();
        let telegram = telegram.clone();

        tokio::spawn(async move {
            // start_high_hit_rate_radar só retorna Err quando a conexão WS
            // morre de vez (ex.: reconnect interno do ethers-rs esgotado —
            // ver logs/deployments do incidente 2026-07-27). Sem este loop,
            // uma única queda de WS matava o radar pro resto do processo:
            // o price_tx parava de receber, e a TUI (que roda em thread e
            // runtime próprios, ver src/tui.rs) continuava desenhando o
            // último estado congelado pra sempre, parecendo viva mas nunca
            // mais atualizando.
            const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
            const MAX_BACKOFF: Duration = Duration::from_secs(60);
            let mut backoff = INITIAL_BACKOFF;

            loop {
                info!("📡 High Hit Rate Radar iniciado.");
                let sd_rx = shutdown_tx.subscribe();
                let result = start_high_hit_rate_radar(
                    client_ws.clone(),
                    dex_manager.clone(),
                    config.clone(),
                    adj_cost.clone(),
                    circuit_breaker.clone(),
                    price_tx.clone(),
                    sd_rx,
                )
                .await;

                match result {
                    // Encerramento limpo: o próprio radar já tratou o sinal
                    // de shutdown internamente e retornou Ok(()).
                    Ok(()) => break,
                    Err(e) => {
                        error!("❌ Radar erro: {:?}", e);
                        let _ = telegram
                            .send_error_alert("Radar", &format!("{:?}", e))
                            .await;

                        let mut sd_rx_wait = shutdown_tx.subscribe();
                        tokio::select! {
                            _ = sd_rx_wait.recv() => {
                                info!("📡 Radar: shutdown durante backoff — não reconecta.");
                                break;
                            }
                            _ = tokio::time::sleep(backoff) => {}
                        }
                        backoff = (backoff * 2).min(MAX_BACKOFF);

                        // A conexão antiga provavelmente está morta (reconnect
                        // interno já esgotado) — reconecta do zero, com o
                        // fallback já existente entre ws_endpoints, antes de
                        // tentar o radar de novo.
                        let net_cfg = { config.lock().await.network.clone() };
                        match RpcProvider::connect_ws(&net_cfg).await {
                            Ok(fresh) => {
                                info!("📡 Radar: WS reconectado, retomando.");
                                client_ws = fresh;
                                backoff = INITIAL_BACKOFF;
                            }
                            Err(e) => {
                                error!("❌ Radar: falha ao reconectar WS: {:?}", e);
                            }
                        }
                    }
                }
            }
            info!("📡 Radar: task encerrada.");
        })
    };

    // ============================================================
    // 🔟 Executor Principal
    // ============================================================
    let bot_task = {
        let bot = bot.clone();
        let price_rx = price_rx.clone();
        let mut sd_rx = shutdown_tx.subscribe();
        let telegram = telegram.clone();
        let tui_state = tui_state.clone();
        let adj_cost = adj_cost.clone();
        let top_n = cfg_unlocked.log.top_spreads_n;

        // Guarda-forte contra travamento do executor: process_prices()
        // encadeia execute_opportunity → send_atomic_flashloan + bundle_sender
        // + telegram, nenhum com timeout próprio (ver execution_engine.rs:337,
        // bundle_sender.rs:97). Se qualquer um desses .await hung (RPC morto,
        // builder de bundle pendurado), o bot_task bloqueava pra sempre
        // segurando bot.lock — sem novos preços, TUI congelava, shutdown
        // pendurava no try_join_all. Timeout aqui cancela o future (libera o
        // lock), loga + alerta, e o ciclo seguinte retoma.
        const EXEC_TIMEOUT: Duration = Duration::from_secs(60);

        tokio::spawn(async move {
            info!("🤖 Bot executor iniciado.");
            let mut cycle_count = 0u64;
            loop {
                tokio::select! {
                    _ = sd_rx.recv() => {
                        info!("🔌 Desligando bot...");
                        let _ = tokio::time::timeout(
                            Duration::from_secs(5),
                            telegram.send_alert("Shutdown", "Bot sendo desligado"),
                        ).await;
                        break;
                    },
                    result = async {
                        let mut rx = price_rx.lock().await;
                        rx.recv().await
                    } => {
                        if let Some(prices) = result {
                            cycle_count += 1;
                            if cycle_count % 10 == 0 {
                                debug!("📊 Ciclo #{} — {} DEXs", cycle_count, prices.len());
                            }

                            update_tui_state(&tui_state, &adj_cost, &prices, top_n, cycle_count);

                            // FASE 7: detecção (descoberta + seleção top-N) sob lock;
                            // envio/confirmação de tx FORA do lock. O bot não segura
                            // MutexGuard através de awaits longos de broadcast.
                            let exec = async {
                                // (1) Descoberta + seleção sob lock (sem envio).
                                let selected = {
                                    let mut bot_guard = bot.lock().await;
                                    bot_guard.select_opportunities(prices).await
                                };

                                match selected {
                                    Ok(opps) if !opps.is_empty() => {
                                        // (2) Clona client + telegram sob lock brevemente,
                                        // libera, e executa fora do lock.
                                        let (client, tg): (ArbitrageClient, Arc<TelegramNotifier>) = {
                                            let bot_guard = bot.lock().await;
                                            (bot_guard.arbitrage_client.clone(), bot_guard.telegram.clone())
                                        };

                                        // FASE 8: top-N. Tenta próxima só se pré-broadcast
                                        // abort (sim/complexidade/rota). Broadcast/terminal para.
                                        for opp in opps {
                                            let res = execute_opportunity_standalone(&client, &tg, opp).await;
                                            match res {
                                                Ok(r) => {
                                                    if !should_try_next_opp(&r) {
                                                        break;
                                                    }
                                                }
                                                Err(e) => {
                                                    error!("❌ Erro ao executar oportunidade: {:?}", e);
                                                    break;
                                                }
                                            }
                                        }
                                        Ok(())
                                    }
                                    Ok(_) => Ok(()),
                                    Err(e) => Err(e),
                                }
                            };
                            match tokio::time::timeout(EXEC_TIMEOUT, exec).await {
                                Ok(Ok(())) => {}
                                Ok(Err(e)) => {
                                    error!("❌ Erro ao processar preços: {:?}", e);
                                    let _ = telegram.send_error_alert("Processamento", &format!("{:?}", e)).await;
                                }
                                Err(_) => {
                                    error!(
                                        "⏰ Executor travou >{}s no ciclo #{} — abortando future (lock liberado), próximo ciclo retoma.",
                                        EXEC_TIMEOUT.as_secs(), cycle_count
                                    );
                                    let _ = telegram
                                        .send_error_alert(
                                            "Executor Timeout",
                                            &format!("Ciclo #{} travou >{}s — abortado, retomando.", cycle_count, EXEC_TIMEOUT.as_secs()),
                                        )
                                        .await;
                                }
                            }
                        } else {
                            warn!("⚠️ Canal de preços fechado.");
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
            }
            info!("🛑 Executor encerrado após {} ciclos.", cycle_count);
        })
    };

    // ============================================================
    // 1️⃣1️⃣ Shutdown ordenado
    // ============================================================
    // Escuta SIGINT/SIGTERM (kill, Ctrl+C fora do raw mode da TUI) UMA vez,
    // dispara o broadcast de shutdown, e RETORNA. Antes isto era um loop
    // infinito: a task nunca completava, então future::try_join_all(tasks)
    // no final do main nunca terminava sozinho e o tokio::time::timeout
    // (shutdown_timeout, ...) — que conta desde o STARTUP, não desde o
    // sinal — always disparava em 180s, abortando radar/bot/TUI no ar.
    // O usuário via o bot morrer em ~180s ≈ 20 ciclos lentos e o terminal
    // ficar preso no alternate screen da TUI (sem cleanup). Agora a task
    // completa no shutdown e o timeout só passa a contar DEPOIS do sinal.
    let shutdown_task = {
        let shutdown_tx = shutdown_tx.clone();
        let telegram = telegram.clone();
        tokio::spawn(async move {
            // ── Espera UM sinal de encerramento ──
            #[cfg(unix)]
            {
                let mut sigint =
                    signal(SignalKind::interrupt()).expect("falha ao registrar handler SIGINT");
                let mut sigterm =
                    signal(SignalKind::terminate()).expect("falha ao registrar handler SIGTERM");

                tokio::select! {
                    _ = sigint.recv() => warn!("🛑 SIGINT recebido — encerrando..."),
                    _ = sigterm.recv() => warn!("🛑 SIGTERM recebido — encerrando..."),
                }
            }

            #[cfg(not(unix))]
            {
                tokio::signal::ctrl_c()
                    .await
                    .expect("falha ao registrar Ctrl+C handler");
                warn!("🛑 Ctrl-C recebido — encerrando...");
            }

            // Alerta Telegram com timeout: send_alert faz HTTP sem timeout
            // próprio. Se rede/Telegram pendurado, timeout garante que o
            // shutdown broadcast sai mesmo assim.
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                telegram.send_alert("Shutdown", "Sinal recebido - encerrando bot"),
            )
            .await;
            let _ = shutdown_tx.send(());
            // NÃO faz process::exit nem loop: o watchdog de emergência
            // (emergency_shutdown.rs) cuida da saída forçada se o runtime
            // não drenar em tempo. Retornar deixa a task completar e o
            // try_join_all do main terminar sem depender do timeout global.
        })
    };

    if tui_enabled {
        info!("🎯 TUI iniciado. Pressione 'q' no terminal da TUI para sair.");
    } else {
        info!("🎯 Headless — Ctrl-C para sair.");
    }

    // ============================================================
    // 1️⃣3️⃣ Execução Concorrente e Debug
    // ============================================================

    // Configura a lista de tasks principais
    let tasks = vec![radar_task, bot_task, shutdown_task];

    // Abort handles: se o shutdown ordenado estourar o timeout, forçamos o
    // abort das tasks que não retornaram.
    let aborts: Vec<tokio::task::AbortHandle> = tasks.iter().map(|t| t.abort_handle()).collect();
    // shutdown_timeout agora é o teto da FASE de shutdown (depois do sinal),
    // não mais um countdown desde o startup. Antes, como as tasks eram loops
    // infinitos, o try_join_all nunca terminava sozinho e este timeout —
    // contando desde "Sistema pronto" — matava o bot em 180s mesmo sem ninguém
    // pedir shutdown. Agora o bloqueio abaixo só acontece APÓS o sinal, então
    // este valor só é gasto drenando tasks na saída.
    let shutdown_timeout =
        Duration::from_secs(cfg_unlocked.general.shutdown_timeout.max(10) as u64);

    // Transição splash → dashboard.
    if let Ok(mut s) = tui_state.write() {
        s.mark_startup_done();
    }

    info!("🎯 Sistema pronto (modo hot-reload).");

    if telegram.is_enabled() {
        let _ = telegram
            .send_alert("Sistema Pronto", "Bot iniciado e monitorando oportunidades")
            .await;
    }

    // ── Bloqueia até ALGUÉM pedir shutdown ──
    // Fontes de shutdown: 'q'/Esc/Ctrl+C na TUI (raw mode), SIGINT/SIGTERM
    // (shutdown_task), Ctrl+C headless (thread early ctrl_c), e o watchdog
    // de emergência (process::exit) como último recurso. Sem este bloqueio,
    // o main avançava direto pro try_join_all com timeout e o processo morria
    // em 180s desde o startup. Agora o bot roda indefinidamente até um sinal.
    let mut main_shutdown_rx = shutdown_tx.subscribe();
    let _ = main_shutdown_rx.recv().await;
    info!(
        "🛑 Shutdown solicitado — drenando tasks (teto {}s)...",
        shutdown_timeout.as_secs()
    );

    // ── Drena as tasks com teto de tempo ──
    // Cada task responde ao broadcast: radar/bot quebram o loop no
    // shutdown_rx; shutdown_task retorna após enviar o broadcast. Se uma
    // task ignorar o sinal (ex.: radar preso em subscribe_blocks sem
    // timeout), o abort garante saída mesmo assim.
    match tokio::time::timeout(shutdown_timeout, future::try_join_all(tasks)).await {
        Ok(Ok(_)) => info!("✅ Todas as tasks finalizadas."),
        Ok(Err(e)) => error!("❌ Erro nas tasks: {:?}", e),
        Err(_) => {
            error!(
                "⏰ Drenagem excedeu {}s — abortando tasks restantes (kill -9 não mais necessário).",
                shutdown_timeout.as_secs()
            );
            for a in &aborts {
                a.abort();
            }
        }
    }

    // O join da thread da TUI (com timeout) é feito automaticamente pelo
    // Drop de TuiGuard, garantindo restauração do terminal em qualquer caminho
    // de saída. A TUI thread já rodou cleanup (disable_raw_mode +
    // LeaveAlternateScreen) ao sair do run_inner.

    metrics::set_bot_status(0);
    info!("👋 Encerrando Flashloan Bot com segurança.");
    let _ = telegram
        .send_alert("Bot Encerrado", "Finalizado com segurança")
        .await;

    Ok(())
}

#[cfg(test)]
mod canonical_tui_tests {
    use super::*;

    fn quote(
        venue: Venue,
        token_in: Address,
        token_out: Address,
        amount_out: u64,
    ) -> PinnedQuoteRecord {
        PinnedQuoteRecord {
            quote_id: H256::zero(),
            anchor_block: 1,
            anchor_hash: H256::zero(),
            venue,
            pool: Address::from_low_u64_be(99),
            token_in,
            token_out,
            amount_in: U256::from(100_000_000u64),
            amount_out: U256::from(amount_out),
            pool_state_id: H256::zero(),
            execution_metadata_id: H256::zero(),
            adapter_version: "test".into(),
            provenance_hash: H256::zero(),
        }
    }

    #[test]
    fn canonical_tui_selects_best_tier_and_filters_dust_pool() {
        let token_a = Address::from_low_u64_be(1);
        let token_b = Address::from_low_u64_be(2);
        let tokens = vec![
            CanonicalToken {
                address: token_a,
                decimals: 6,
                symbol: "A".into(),
            },
            CanonicalToken {
                address: token_b,
                decimals: 6,
                symbol: "B".into(),
            },
        ];
        let quotes = vec![
            quote(Venue::QuickSwap, token_a, token_b, 100_000_000),
            quote(Venue::QuickSwap, token_b, token_a, 99_000_000),
            quote(Venue::SushiSwap, token_a, token_b, 200_000),
            quote(Venue::SushiSwap, token_b, token_a, 300_000),
            quote(Venue::UniswapV3, token_a, token_b, 50_000_000),
            quote(Venue::UniswapV3, token_a, token_b, 101_000_000),
            quote(Venue::UniswapV3, token_b, token_a, 98_000_000),
        ];

        let mut rows = canonical_price_rows_with_bounds(&quotes, &tokens, 0.9, 1.05);
        let forward = rows.iter().find(|row| row.pair == "A/B").unwrap();
        assert_eq!(forward.quickswap, Some(1.0));
        assert_eq!(forward.sushiswap, None);
        assert_eq!(forward.uniswap_v3, Some(1.01));

        let (top, _, _, _) = canonical_tui_economics(&mut rows, &AdjCostParams::default(), 8);
        assert!(!top.is_empty());
        assert!(top.iter().all(|combo| combo.hop_count == 2));
        assert!(top.iter().all(|combo| combo.net_usd.is_some()));
        assert!(rows.iter().any(|row| row.net_usd.is_some()));
    }

    #[test]
    fn combined_top_combo_keeps_two_leg_and_triangular_routes() {
        let base = tui::TopSpreadRow {
            hop_count: 2,
            pair: "USDT-WMATIC".into(),
            tui_spread_pct: 0.1,
            buy_dex: "QuickSwap".into(),
            sell_dex: "UniswapV3".into(),
            legs_label: Some("Q→U".into()),
            cycle_rate: Some(1.001),
            net_usd: Some(0.04),
            distance_to_profit: 0.0,
            executable: true,
            has_curve_leg: false,
            outlier: None,
        };
        let mut triangular = base.clone();
        triangular.hop_count = 3;
        triangular.pair = "USDT>USDC>WMATIC>USDT".into();
        triangular.legs_label = Some("U→Q→S".into());
        triangular.net_usd = Some(0.02);

        let combined = combine_top_combo_rows(vec![base], vec![triangular]);
        assert_eq!(combined.len(), 2);
        assert_eq!(combined[0].hop_count, 2);
        assert_eq!(combined[1].hop_count, 3);
    }
}
