//! Polygon discovery diagnostic. This binary owns only a read-only `Provider`.
//! It never loads `.env`, `PRIVATE_KEY`, a wallet, signer, executor, or broadcaster.

use anyhow::{anyhow, Context, Result};
use ethers::{
    abi::Abi,
    contract::Contract,
    providers::{Middleware, Provider},
    types::{Address, U256},
};
use flashloan_bot::{
    config::Config,
    core::{
        bf_graph::{find_arbitrage_cycles, PriceGraph},
        diagnostic_graph::{
            enumerate_simple_cycles_exact, find_negative_cycles_raw, save_snapshot_atomic,
            validate_diagnostic_graph, DiagnosticEdge, DiagnosticGraph, DiagnosticToken,
        },
        read_only::{ReadOnlyCounters, ReadOnlySafety},
    },
    infra::{
        price_feed::{PriceFeedSource, PRICE_FEED},
        rotating_http_client::RotatingHttpClient,
    },
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const TOKENS: [&str; 5] = ["USDC", "USDT", "WMATIC", "WETH", "WBTC"];
const V2_ABI: &str = r#"[{"inputs":[{"internalType":"uint256","name":"amountIn","type":"uint256"},{"internalType":"address[]","name":"path","type":"address[]"}],"name":"getAmountsOut","outputs":[{"internalType":"uint256[]","name":"amounts","type":"uint256[]"}],"stateMutability":"view","type":"function"}]"#;
const V3_ABI: &str = r#"[{"inputs":[{"internalType":"address","name":"tokenIn","type":"address"},{"internalType":"address","name":"tokenOut","type":"address"},{"internalType":"uint24","name":"fee","type":"uint24"},{"internalType":"uint256","name":"amountIn","type":"uint256"},{"internalType":"uint160","name":"sqrtPriceLimitX96","type":"uint160"}],"name":"quoteExactInputSingle","outputs":[{"internalType":"uint256","name":"amountOut","type":"uint256"}],"stateMutability":"nonpayable","type":"function"}]"#;
const UNISWAP_V3_QUOTER: &str = "0xb27308f9F90D607463bb33eA1BeBb41C27CE5AB6";

fn parse_args() -> Result<(u64, u64, Option<PathBuf>, Option<PathBuf>)> {
    let mut scans = 1;
    let mut duration = 120;
    let mut save_graph = None;
    let mut replay_graph = None;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        let value = args
            .get(i + 1)
            .ok_or_else(|| anyhow!("missing value for {}", args[i]))?;
        match args[i].as_str() {
            "--max-scans" => scans = value.parse().context("invalid --max-scans")?,
            "--duration-seconds" => {
                duration = value.parse().context("invalid --duration-seconds")?
            }
            "--save-graph" => save_graph = Some(PathBuf::from(value)),
            "--replay-graph" => replay_graph = Some(PathBuf::from(value)),
            other => return Err(anyhow!("unknown argument {other}")),
        }
        i += 2;
    }
    if scans == 0 || duration == 0 {
        return Err(anyhow!("scan limits must be positive"));
    }
    Ok((scans, duration, save_graph, replay_graph))
}

fn comparison_path(graph_path: &Path) -> PathBuf {
    graph_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("phase2a_cycle_comparison.json")
}

fn audit_graph(graph: &DiagnosticGraph, output: &Path) -> Result<()> {
    let validation = validate_diagnostic_graph(graph)?;
    let exact = enumerate_simple_cycles_exact(graph, 2, 4);
    let bf = find_negative_cycles_raw(graph);
    let exact_negative: HashSet<_> = exact
        .iter()
        .filter(|cycle| cycle.total_weight < -flashloan_bot::core::diagnostic_graph::EPSILON)
        .map(|cycle| cycle.canonical_key.clone())
        .collect();
    let bf_negative: HashSet<_> = bf
        .cycles
        .iter()
        .map(|cycle| cycle.canonical_key.clone())
        .collect();
    let two_hop = exact
        .iter()
        .filter(|cycle| cycle.edge_ids.len() == 2)
        .count();
    let three_hop = exact
        .iter()
        .filter(|cycle| cycle.edge_ids.len() == 3)
        .count();
    let four_hop = exact
        .iter()
        .filter(|cycle| cycle.edge_ids.len() == 4)
        .count();
    let only_exact = exact_negative.difference(&bf_negative).count();
    let only_bf = bf_negative.difference(&exact_negative).count();
    let both = exact_negative.intersection(&bf_negative).count();
    let payload = serde_json::json!({
        "schema_version": 1,
        "scan_id": graph.scan_id,
        "graph_validation": validation,
        "exact_cycles": {
            "all": exact.len(), "negative": exact_negative.len(),
            "two_hop": two_hop, "three_hop": three_hop, "four_hop": four_hop
        },
        "bellman_ford": {
            "negative_relaxations": bf.negative_relaxations,
            "reconstruction_attempts": bf.reconstruction_attempts,
            "reconstruction_successes": bf.reconstruction_successes,
            "reconstruction_failures": bf.reconstruction_failures,
            "negative_cycles": bf_negative.len()
        },
        "comparison": {"both": both, "only_exact": only_exact, "only_bf": only_bf}
    });
    std::fs::write(output, serde_json::to_vec_pretty(&payload)?)?;
    eprintln!("[GRAPH_VALIDATION] tokens={} edges={} invalid_edges={} parallel_edge_groups={} bidirectional_pair_groups={} verdict=PASS", validation.tokens_total, validation.edges_total, validation.invalid_edges, validation.parallel_edge_groups, validation.bidirectional_pair_groups);
    eprintln!("[BF_DIAGNOSTIC] EXACT_CYCLES_2_HOP={two_hop} EXACT_CYCLES_3_HOP={three_hop} EXACT_CYCLES_4_HOP={four_hop} EXACT_NEGATIVE_CYCLES={} BF_RAW_NEGATIVE_CYCLES={} BF_NEGATIVE_RELAXATIONS={} BF_RECONSTRUCTION_ATTEMPTS={} BF_RECONSTRUCTION_SUCCESSES={} BF_RECONSTRUCTION_FAILURES={} CYCLES_IN_EXACT_AND_BF={both} CYCLES_ONLY_IN_EXACT={only_exact} CYCLES_ONLY_IN_BF={only_bf}", exact_negative.len(), bf_negative.len(), bf.negative_relaxations, bf.reconstruction_attempts, bf.reconstruction_successes, bf.reconstruction_failures);
    Ok(())
}

#[derive(Clone)]
struct CapturedQuote {
    dex: String,
    version: String,
    a: String,
    b: String,
    aa: Address,
    bb: Address,
    da: u8,
    db: u8,
    input: U256,
    output: U256,
    rate: f64,
}

fn pair_name(a: &str, b: &str) -> String {
    format!("{a}-{b}")
}

fn amount_raw(notional: f64, price: f64, decimals: u8) -> Result<U256> {
    if !price.is_finite() || price <= 0.0 {
        return Err(anyhow!("invalid price"));
    }
    let raw = (notional / price) * 10f64.powi(decimals as i32);
    if !raw.is_finite() || raw < 1.0 || raw >= u128::MAX as f64 {
        return Err(anyhow!("invalid amount"));
    }
    Ok(U256::from(raw as u128))
}

fn rate(amount_in: U256, amount_out: U256, decimals_in: u8, decimals_out: u8) -> Option<f64> {
    let i = amount_in.to_string().parse::<f64>().ok()? / 10f64.powi(decimals_in as i32);
    let o = amount_out.to_string().parse::<f64>().ok()? / 10f64.powi(decimals_out as i32);
    let result = o / i;
    result
        .is_finite()
        .then_some(result)
        .filter(|value| *value > 0.0)
}

fn prune_reciprocity(map: &mut HashMap<String, f64>, counters: &mut ReadOnlyCounters) {
    let mut remove = HashSet::new();
    for (pair, value) in map.iter() {
        let Some((a, b)) = pair.split_once('-') else {
            continue;
        };
        let reverse = pair_name(b, a);
        if let Some(reverse_value) = map.get(&reverse) {
            let product = value * reverse_value;
            if !(0.95..=1.01).contains(&product) {
                remove.insert(pair.clone());
                remove.insert(reverse);
            }
        }
    }
    counters.rejected_reciprocity += remove.len() as u64;
    for pair in remove {
        map.remove(&pair);
    }
}

async fn quote_v2(
    provider: Arc<Provider<RotatingHttpClient>>,
    router: Address,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Result<U256> {
    let abi: Abi = serde_json::from_str(V2_ABI)?;
    let contract = Contract::new(router, abi, provider);
    let amounts: Vec<U256> = contract
        .method("getAmountsOut", (amount_in, vec![token_in, token_out]))?
        .call()
        .await?;
    amounts
        .last()
        .copied()
        .ok_or_else(|| anyhow!("empty V2 quote"))
}

async fn quote_v3(
    provider: Arc<Provider<RotatingHttpClient>>,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Result<U256> {
    let abi: Abi = serde_json::from_str(V3_ABI)?;
    let quoter = Contract::new(UNISWAP_V3_QUOTER.parse::<Address>()?, abi, provider);
    let mut best = None;
    for fee in [500u32, 3000, 10_000] {
        if let Ok(out) = quoter
            .method::<_, U256>(
                "quoteExactInputSingle",
                (token_in, token_out, fee, amount_in, U256::zero()),
            )?
            .call()
            .await
        {
            if !out.is_zero() && best.map(|old| out > old).unwrap_or(true) {
                best = Some(out);
            }
        }
    }
    best.ok_or_else(|| anyhow!("no executable V3 quote"))
}

async fn scan(
    provider: Arc<Provider<RotatingHttpClient>>,
    cfg: &flashloan_bot::config::Config,
    id: u64,
) -> Result<(ReadOnlyCounters, DiagnosticGraph)> {
    let started = Instant::now();
    let mut counters = ReadOnlyCounters::default();
    let mut entries = Vec::new();
    for symbol in TOKENS {
        let Some(address) = cfg.addresses.get(symbol).copied() else {
            continue;
        };
        let Some(decimals) = cfg
            .pairs
            .metadata
            .get(symbol)
            .and_then(|item| item.decimals)
        else {
            continue;
        };
        counters.configured_tokens += 1;
        counters.price_feed_attempted += 1;
        match PRICE_FEED.get_price_with_source(symbol).await {
            Ok((price, PriceFeedSource::Primary)) => {
                entries.push((symbol, address, decimals, price))
            }
            Ok((price, PriceFeedSource::Fallback)) => {
                counters.price_feed_succeeded_fallback += 1;
                entries.push((symbol, address, decimals, price));
            }
            Err(error) => {
                counters.price_feed_failed += 1;
                eprintln!(
                    "[READ_ONLY_FEED_ERROR] symbol={symbol} reason={}",
                    error.to_string().replace('\n', " ")
                );
            }
        }
    }
    counters.price_feed_succeeded_primary =
        entries.len() as u64 - counters.price_feed_succeeded_fallback;
    let quickswap = cfg
        .dex
        .iter()
        .find(|dex| dex.name == "QuickSwap")
        .context("QuickSwap absent")?
        .router_address
        .parse::<Address>()?;
    let mut prices: HashMap<String, HashMap<String, f64>> = HashMap::new();
    let mut captured = Vec::new();
    for (symbol_in, address_in, decimals_in, usd_price) in &entries {
        for (symbol_out, address_out, decimals_out, _) in &entries {
            if symbol_in == symbol_out {
                continue;
            }
            counters.configured_pairs += 1;
            counters.sizing_attempted += 1;
            let input = match amount_raw(100.0, *usd_price, *decimals_in) {
                Ok(value) => {
                    counters.sizing_succeeded += 1;
                    value
                }
                Err(_) => {
                    counters.sizing_failed += 1;
                    continue;
                }
            };
            counters.dex_quote_attempted += 2;
            match quote_v2(
                provider.clone(),
                quickswap,
                *address_in,
                *address_out,
                input,
            )
            .await
            {
                Ok(out) if rate(input, out, *decimals_in, *decimals_out).is_some() => {
                    let value = rate(input, out, *decimals_in, *decimals_out).unwrap();
                    counters.dex_quote_succeeded += 1;
                    counters.v2_quote_succeeded += 1;
                    counters.raw_quotes += 1;
                    prices
                        .entry("QuickSwap".into())
                        .or_default()
                        .insert(pair_name(symbol_in, symbol_out), value);
                    captured.push(CapturedQuote {
                        dex: "QuickSwap".into(),
                        version: "V2".into(),
                        a: (*symbol_in).into(),
                        b: (*symbol_out).into(),
                        aa: *address_in,
                        bb: *address_out,
                        da: *decimals_in,
                        db: *decimals_out,
                        input,
                        output: out,
                        rate: value,
                    });
                }
                _ => counters.dex_quote_failed += 1,
            }
            match quote_v3(provider.clone(), *address_in, *address_out, input).await {
                Ok(out) if rate(input, out, *decimals_in, *decimals_out).is_some() => {
                    let value = rate(input, out, *decimals_in, *decimals_out).unwrap();
                    counters.dex_quote_succeeded += 1;
                    counters.v3_quote_succeeded += 1;
                    counters.raw_quotes += 1;
                    prices
                        .entry("UniswapV3".into())
                        .or_default()
                        .insert(pair_name(symbol_in, symbol_out), value);
                    captured.push(CapturedQuote {
                        dex: "UniswapV3".into(),
                        version: "V3".into(),
                        a: (*symbol_in).into(),
                        b: (*symbol_out).into(),
                        aa: *address_in,
                        bb: *address_out,
                        da: *decimals_in,
                        db: *decimals_out,
                        input,
                        output: out,
                        rate: value,
                    });
                }
                _ => counters.dex_quote_failed += 1,
            }
        }
    }
    for map in prices.values_mut() {
        prune_reciprocity(map, &mut counters);
    }
    counters.accepted_quotes = prices.values().map(|map| map.len() as u64).sum();
    counters.price_map_dexes = prices.values().filter(|map| !map.is_empty()).count() as u64;
    counters.price_map_pairs = counters.accepted_quotes;
    let mut token_ids = HashMap::new();
    let mut tokens = Vec::new();
    for q in &captured {
        for (s, a, d) in [(&q.a, q.aa, q.da), (&q.b, q.bb, q.db)] {
            if !token_ids.contains_key(s) {
                let id = tokens.len();
                token_ids.insert(s.clone(), id);
                tokens.push(DiagnosticToken {
                    id,
                    symbol: s.clone(),
                    address: format!("{a:#x}"),
                    decimals: d,
                });
            }
        }
    }
    let mut edges = Vec::new();
    for q in captured {
        if prices
            .get(&q.dex)
            .and_then(|m| m.get(&pair_name(&q.a, &q.b)))
            .is_some()
        {
            let id = edges.len();
            edges.push(DiagnosticEdge {
                id,
                from: token_ids[&q.a],
                to: token_ids[&q.b],
                token_in_symbol: q.a,
                token_out_symbol: q.b,
                token_in_address: format!("{:#x}", q.aa),
                token_out_address: format!("{:#x}", q.bb),
                dex_name: q.dex,
                protocol_version: q.version,
                pool_address: None,
                fee_tier: None,
                rate: q.rate,
                amount_in_raw: q.input.to_string(),
                amount_out_raw: q.output.to_string(),
                block_number: None,
                quote_source: "eth_call".into(),
                reciprocity_status: "Accepted".into(),
            });
        }
    }
    let diagnostic_graph = DiagnosticGraph {
        schema_version: 1,
        scan_id: id.to_string(),
        chain: "polygon".into(),
        captured_at: chrono::Utc::now().to_rfc3339(),
        block_start: 0,
        block_end: 0,
        tokens,
        edges,
    };
    let graph = PriceGraph::from_price_map(&prices);
    counters.graph_vertices = graph.tokens.len() as u64;
    counters.graph_edges = graph.edges.len() as u64;
    let cycles = find_arbitrage_cycles(&graph, 0.1, 50.0);
    counters.bf_cycles_raw = cycles.len() as u64;
    counters.bf_cycles_unique = cycles.len() as u64;
    eprintln!("[PIPELINE_SUMMARY] scan_id={id} duration_ms={} configured_tokens={} configured_pairs={} price_feed_attempted={} price_feed_succeeded_primary={} price_feed_succeeded_fallback={} price_feed_failed={} price_feed_invalid={} sizing_attempted={} sizing_succeeded={} sizing_failed={} dex_quote_attempted={} dex_quote_succeeded={} dex_quote_failed={} v2_quote_succeeded={} v3_quote_succeeded={} raw_quotes={} accepted_quotes={} rejected_reciprocity={} price_map_dexes={} price_map_pairs={} graph_vertices={} graph_edges={} bf_cycles_raw={} bf_cycles_unique={} opportunities_emitted={}", started.elapsed().as_millis(), counters.configured_tokens, counters.configured_pairs, counters.price_feed_attempted, counters.price_feed_succeeded_primary, counters.price_feed_succeeded_fallback, counters.price_feed_failed, counters.price_feed_invalid, counters.sizing_attempted, counters.sizing_succeeded, counters.sizing_failed, counters.dex_quote_attempted, counters.dex_quote_succeeded, counters.dex_quote_failed, counters.v2_quote_succeeded, counters.v3_quote_succeeded, counters.raw_quotes, counters.accepted_quotes, counters.rejected_reciprocity, counters.price_map_dexes, counters.price_map_pairs, counters.graph_vertices, counters.graph_edges, counters.bf_cycles_raw, counters.bf_cycles_unique, counters.opportunities_emitted);
    eprintln!(
        "[PIPELINE_REJECTIONS] NonReciprocal={}",
        counters.rejected_reciprocity
    );
    if !counters.valid() {
        return Err(anyhow!("counter consistency failure"));
    }
    eprintln!("[DIAGNOSTIC_GRAPH] RAW_DIRECTIONAL_QUOTES={} ACCEPTED_DIRECTIONAL_QUOTES={} REJECTED_RECIPROCITY_QUOTES={} DIAGNOSTIC_GRAPH_EDGES={} LEGACY_PRICE_MAP_ENTRIES={} COLLAPSED_EDGE_COUNT={}", counters.raw_quotes, diagnostic_graph.edges.len(), counters.rejected_reciprocity, diagnostic_graph.edges.len(), counters.price_map_pairs, diagnostic_graph.edges.len().saturating_sub(counters.price_map_pairs as usize));
    Ok((counters, diagnostic_graph))
}

#[tokio::main]
async fn main() -> Result<()> {
    let (max_scans, duration_seconds, save_graph, replay_graph) = parse_args()?;
    if let Some(path) = replay_graph {
        let graph: DiagnosticGraph = serde_json::from_slice(&std::fs::read(&path)?)?;
        let comparison = comparison_path(&path);
        audit_graph(&graph, &comparison)?;
        eprintln!("[REPLAY] RPC_USED=false PRICE_FEED_USED=false DEX_QUOTER_USED=false SIGNER_LOADED=false BROADCASTER_INITIALIZED=false JITO_INITIALIZED=false GRAPH={} COMPARISON={}", path.display(), comparison.display());
        return Ok(());
    }
    let safety = ReadOnlySafety::from_env();
    safety.validate()?;
    eprintln!("[READ_ONLY_SAFETY] diagnostic_mode=true live_trading_enabled=false transaction_broadcast_allowed=false signer_loaded=false broadcaster_initialized=false jito_initialized=false safety_verdict=PASS");
    eprintln!("READ_ONLY_SCAN=true MAINNET_TRANSACTIONS_ALLOWED=false SIGNER_PRESENT=false");
    let cfg = Config::from_file(PathBuf::from("config/config.toml"))?
        .lock()
        .await
        .clone();
    let endpoints = std::env::var("BOT_RPC_ENDPOINTS")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_else(|| cfg.network.rpc_endpoints.clone().unwrap_or_default());
    let transport = RotatingHttpClient::from_strings(
        &endpoints,
        Duration::from_millis(cfg.network.timeout_ms.max(1_000)),
    )?;
    let provider = Arc::new(Provider::new(transport).interval(Duration::from_millis(100)));
    let chain_id = provider.get_chainid().await?;
    if chain_id.as_u64() != 137 {
        return Err(anyhow!("expected Polygon chain 137, got {chain_id}"));
    }
    let deadline = Instant::now() + Duration::from_secs(duration_seconds);
    for scan_id in 1..=max_scans {
        if Instant::now() >= deadline {
            break;
        }
        let (_, graph) = scan(provider.clone(), &cfg, scan_id).await?;
        if let Some(path) = &save_graph {
            save_snapshot_atomic(&graph, path)?;
            audit_graph(&graph, &comparison_path(path))?;
        }
    }
    eprintln!("[PHASE1B_VERDICT] scans_completed=true MAINNET_TRANSACTIONS_SENT=0 LIVE_TRADING_ENABLED=false TRANSACTION_BROADCAST_ALLOWED=false");
    Ok(())
}
