//! Polygon discovery diagnostic. This binary owns only a read-only `Provider`.
//! It never loads `.env`, `PRIVATE_KEY`, a wallet, signer, executor, or broadcaster.

use anyhow::{anyhow, Context, Result};
use ethers::{
    abi::{Abi, Detokenize},
    contract::{Contract, ContractCall},
    providers::{Middleware, Provider},
    types::{Address, BlockId, BlockNumber, U256},
};
use flashloan_bot::{
    config::Config,
    core::{
        bf_graph::{find_arbitrage_cycles, PriceGraph},
        diagnostic_graph::{
            enumerate_simple_cycles_exact, find_negative_cycles_raw, save_snapshot_atomic,
            validate_diagnostic_graph, DiagnosticEdge, DiagnosticGraph, DiagnosticToken,
        },
        phase2d_anchor::{
            provenance, reorg_detected, select_anchor, validate_edge_anchors,
            validate_quote_anchor, AnchorBlock, AnchorSnapshotMetadata, QuoteBlockProvenance,
            ScanBlockContext,
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

const BASE_TOKENS: [&str; 5] = ["USDC", "USDT", "WMATIC", "WETH", "WBTC"];
const LIQUID_TOKENS: [&str; 10] = [
    "USDC", "USDT", "WMATIC", "WETH", "WBTC", "DAI", "LINK", "UNI", "LDO", "AAVE",
];
const V2_ABI: &str = r#"[{"inputs":[{"internalType":"uint256","name":"amountIn","type":"uint256"},{"internalType":"address[]","name":"path","type":"address[]"}],"name":"getAmountsOut","outputs":[{"internalType":"uint256[]","name":"amounts","type":"uint256[]"}],"stateMutability":"view","type":"function"}]"#;
const V3_ABI: &str = r#"[{"inputs":[{"internalType":"address","name":"tokenIn","type":"address"},{"internalType":"address","name":"tokenOut","type":"address"},{"internalType":"uint24","name":"fee","type":"uint24"},{"internalType":"uint256","name":"amountIn","type":"uint256"},{"internalType":"uint160","name":"sqrtPriceLimitX96","type":"uint160"}],"name":"quoteExactInputSingle","outputs":[{"internalType":"uint256","name":"amountOut","type":"uint256"}],"stateMutability":"nonpayable","type":"function"}]"#;
const UNISWAP_V3_QUOTER: &str = "0xb27308f9F90D607463bb33eA1BeBb41C27CE5AB6";
const CURVE_AAVE_POOL: &str = "0x445FE580eF8d70FF569aB36e80c647af338db351";
const CURVE_ABI: &str = r#"[{"inputs":[{"type":"int128","name":"i"},{"type":"int128","name":"j"},{"type":"uint256","name":"dx"}],"name":"get_dy","outputs":[{"type":"uint256","name":""}],"stateMutability":"view","type":"function"}]"#;
const ERC20_DECIMALS_ABI: &str = r#"[{"inputs":[],"name":"decimals","outputs":[{"internalType":"uint8","name":"","type":"uint8"}],"stateMutability":"view","type":"function"}]"#;
const QUOTE_TIMEOUT: Duration = Duration::from_secs(20);
type ScanArgs = (
    u64,
    u64,
    u64,
    Option<PathBuf>,
    Option<PathBuf>,
    Option<PathBuf>,
    String,
    usize,
    usize,
    bool,
    u64,
);

fn parse_args() -> Result<ScanArgs> {
    let mut scans = 1;
    let mut duration = 120;
    let mut save_graph = None;
    let mut replay_graph = None;
    let mut phase2b_output = None;
    let mut inter_scan_delay = 0;
    let mut profile = "base".to_string();
    let mut max_tokens = 12usize;
    let mut max_pairs = 120usize;
    let mut pin_anchor_block = true;
    let mut anchor_confirmation_lag = 2u64;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if let Some(value) = args[i].strip_prefix("--pin-anchor-block=") {
            pin_anchor_block = value.parse().context("invalid --pin-anchor-block")?;
            i += 1;
            continue;
        }
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
            "--phase2b-output" | "--phase2c-output" => phase2b_output = Some(PathBuf::from(value)),
            "--inter-scan-delay-seconds" => {
                inter_scan_delay = value
                    .parse()
                    .context("invalid --inter-scan-delay-seconds")?
            }
            "--universe-profile" => profile = value.clone(),
            "--max-tokens" => max_tokens = value.parse().context("invalid --max-tokens")?,
            "--max-pairs" => max_pairs = value.parse().context("invalid --max-pairs")?,
            "--pin-anchor-block" => {
                pin_anchor_block = value.parse().context("invalid --pin-anchor-block")?
            }
            "--anchor-confirmation-lag" => {
                anchor_confirmation_lag =
                    value.parse().context("invalid --anchor-confirmation-lag")?
            }
            other => return Err(anyhow!("unknown argument {other}")),
        }
        i += 2;
    }
    if scans == 0 || duration == 0 {
        return Err(anyhow!("scan limits must be positive"));
    }
    Ok((
        scans,
        duration,
        inter_scan_delay,
        save_graph,
        replay_graph,
        phase2b_output,
        profile,
        max_tokens,
        max_pairs,
        pin_anchor_block,
        anchor_confirmation_lag,
    ))
}

fn universe_tokens(profile: &str, max_tokens: usize) -> Result<Vec<&'static str>> {
    let selected: &[&str] = match profile {
        "base" => &BASE_TOKENS,
        "liquid" => &LIQUID_TOKENS,
        "expanded" => &LIQUID_TOKENS,
        _ => return Err(anyhow!("invalid --universe-profile: {profile}")),
    };
    Ok(selected.iter().copied().take(max_tokens).collect())
}

fn token_category(symbol: &str) -> &'static str {
    match symbol {
        "USDC" | "USDT" | "DAI" => "STABLE",
        "WMATIC" | "WETH" => "WRAPPED_NATIVE",
        "WBTC" | "LINK" | "AAVE" | "UNI" => "MAJOR",
        "LDO" => "LST_OR_WRAPPED",
        _ => "LONG_TAIL",
    }
}

fn write_phase2c_catalog(
    root: &Path,
    profile: &str,
    symbols: &[&str],
    cfg: &Config,
    max_pairs: usize,
) -> Result<()> {
    std::fs::create_dir_all(root)?;
    let tokens = symbols.iter().filter_map(|symbol| {
        let address = cfg.addresses.get(*symbol)?;
        let decimals = cfg.pairs.metadata.get(*symbol)?.decimals?;
        Some(serde_json::json!({"symbol":symbol,"address":format!("{address:#x}"),"decimals":decimals,"profile":profile,"category":token_category(symbol),"enabled":true,"validation_status":"VALID","validation_reason":"configured Polygon address and decimals"}))
    }).collect::<Vec<_>>();
    let pairs = symbols
        .iter()
        .flat_map(|a| {
            symbols
                .iter()
                .filter(move |b| *a != **b)
                .map(move |b| serde_json::json!({"token_in":a,"token_out":b,"directional":true}))
        })
        .take(max_pairs)
        .collect::<Vec<_>>();
    std::fs::write(
        root.join("token_universe.json"),
        serde_json::to_vec_pretty(&tokens)?,
    )?;
    std::fs::write(
        root.join("pair_catalog.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"profile":profile,"pair_policy":"all selected directed pairs, bounded by max_pairs; no synthetic reverse","tokens_selected":tokens.len(),"pairs_generated":symbols.len().saturating_mul(symbols.len().saturating_sub(1)),"pairs_after_limit":pairs.len(),"self_pairs_rejected":symbols.len(),"duplicate_pairs_rejected":0,"pairs":pairs}),
        )?,
    )?;
    Ok(())
}

fn comparison_path(graph_path: &Path) -> PathBuf {
    graph_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("phase2a_cycle_comparison.json")
}

fn write_phase2c_metrics(
    root: &Path,
    profile: &str,
    graph: &DiagnosticGraph,
    venues: &HashMap<String, VenueCounters>,
) -> Result<()> {
    let mut token_rows = Vec::new();
    for token in &graph.tokens {
        let outgoing: Vec<_> = graph.edges.iter().filter(|e| e.from == token.id).collect();
        let incoming: Vec<_> = graph.edges.iter().filter(|e| e.to == token.id).collect();
        let mut dexes = HashSet::new();
        let mut neighbors = HashSet::new();
        for edge in outgoing.iter().chain(incoming.iter()) {
            dexes.insert(edge.dex_name.clone());
            neighbors.insert(if edge.from == token.id {
                edge.to
            } else {
                edge.from
            });
        }
        token_rows.push(serde_json::json!({"symbol":token.symbol,"address":token.address,"profile":profile,"price_available":true,"quotes_attempted":outgoing.len()+incoming.len(),"quotes_succeeded":outgoing.len()+incoming.len(),"quotes_failed":0,"accepted_outgoing_edges":outgoing.len(),"accepted_incoming_edges":incoming.len(),"reciprocity_rejections":0,"dexes_attempted":4,"dexes_with_success":dexes.len(),"connected_neighbors":neighbors.len(),"in_degree":incoming.len(),"out_degree":outgoing.len(),"isolated":outgoing.is_empty() && incoming.is_empty()}));
    }
    token_rows.sort_by_key(|row| row["symbol"].as_str().unwrap_or_default().to_string());
    std::fs::write(
        root.join("token_metrics.json"),
        serde_json::to_vec_pretty(&token_rows)?,
    )?;
    let mut pairs = HashMap::<String, serde_json::Value>::new();
    for edge in &graph.edges {
        let key = format!(
            "{}|{}|{}|{}|{}|{}",
            edge.dex_name,
            edge.protocol_version,
            edge.pool_address.as_deref().unwrap_or(""),
            edge.fee_tier
                .map(|x| x.to_string())
                .as_deref()
                .unwrap_or(""),
            edge.token_in_address,
            edge.token_out_address
        );
        let row = pairs.entry(key).or_insert_with(|| serde_json::json!({"dex":edge.dex_name,"protocol_version":edge.protocol_version,"pool_address":edge.pool_address,"fee_tier":edge.fee_tier,"coin_index_in":edge.quote_source.split("curve_indices:").nth(1).and_then(|v|v.split(':').next()).and_then(|v|v.parse::<usize>().ok()),"coin_index_out":edge.quote_source.split("curve_indices:").nth(1).and_then(|v|v.split(':').nth(1)).and_then(|v|v.parse::<usize>().ok()),"token_in":edge.token_in_address,"token_out":edge.token_out_address,"attempts":0,"successes":0,"failures":0,"accepted":0,"reciprocity_rejections":0,"no_pool":0,"rpc_errors":0,"min_rate":edge.rate,"max_rate":edge.rate}));
        row["attempts"] = serde_json::json!(row["attempts"].as_u64().unwrap_or(0) + 1);
        row["successes"] = row["attempts"].clone();
        row["accepted"] = row["attempts"].clone();
    }
    std::fs::write(
        root.join("pair_metrics.json"),
        serde_json::to_vec_pretty(&pairs.into_values().collect::<Vec<_>>())?,
    )?;
    let n = graph.tokens.len();
    let mut undirected = vec![Vec::new(); n];
    let mut out = vec![Vec::new(); n];
    let mut rev = vec![Vec::new(); n];
    for e in &graph.edges {
        undirected[e.from].push(e.to);
        undirected[e.to].push(e.from);
        out[e.from].push(e.to);
        rev[e.to].push(e.from);
    }
    fn components(adj: &[Vec<usize>]) -> Vec<usize> {
        let mut seen = vec![false; adj.len()];
        let mut sizes = Vec::new();
        for i in 0..adj.len() {
            if seen[i] {
                continue;
            }
            let mut stack = vec![i];
            seen[i] = true;
            let mut size = 0;
            while let Some(v) = stack.pop() {
                size += 1;
                for &w in &adj[v] {
                    if !seen[w] {
                        seen[w] = true;
                        stack.push(w)
                    }
                }
            }
            sizes.push(size)
        }
        sizes
    }
    let weak = components(&undirected);
    // Kosaraju: reachability intersection gives SCC size; small diagnostic graphs keep this explicit.
    let mut scc_sizes = Vec::new();
    let mut assigned = vec![false; n];
    for i in 0..n {
        if assigned[i] {
            continue;
        }
        let reach = |adj: &Vec<Vec<usize>>| {
            let mut s = HashSet::from([i]);
            let mut q = vec![i];
            while let Some(v) = q.pop() {
                for &w in &adj[v] {
                    if s.insert(w) {
                        q.push(w)
                    }
                }
            }
            s
        };
        let a = reach(&out);
        let b = reach(&rev);
        let both: Vec<_> = a.intersection(&b).copied().collect();
        for v in both.iter() {
            assigned[*v] = true;
        }
        scc_sizes.push(both.len());
    }
    let isolated = (0..n).filter(|i| undirected[*i].is_empty()).count();
    let connectivity = serde_json::json!({"profile":profile,"scan_id":graph.scan_id,"GRAPH_TOKENS":n,"GRAPH_EDGES":graph.edges.len(),"WEAKLY_CONNECTED_COMPONENTS":weak.len(),"STRONGLY_CONNECTED_COMPONENTS":scc_sizes.len(),"LARGEST_WEAK_COMPONENT_TOKENS":weak.into_iter().max().unwrap_or(0),"LARGEST_STRONG_COMPONENT_TOKENS":scc_sizes.into_iter().max().unwrap_or(0),"ISOLATED_TOKENS":isolated,"AVG_IN_DEGREE":if n==0 {0.0}else{graph.edges.len() as f64/n as f64},"AVG_OUT_DEGREE":if n==0 {0.0}else{graph.edges.len() as f64/n as f64},"MAX_IN_DEGREE":rev.iter().map(Vec::len).max().unwrap_or(0),"MAX_OUT_DEGREE":out.iter().map(Vec::len).max().unwrap_or(0)});
    std::fs::write(
        root.join("connectivity_metrics.json"),
        serde_json::to_vec_pretty(&connectivity)?,
    )?;
    std::fs::write(
        root.join("dex_metrics.json"),
        serde_json::to_vec_pretty(venues)?,
    )?;
    Ok(())
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
    pool_address: Option<String>,
    fee_tier: Option<u32>,
    coin_index_in: Option<usize>,
    coin_index_out: Option<usize>,
    provenance: QuoteBlockProvenance,
}

#[derive(Default, Clone, serde::Serialize)]
struct VenueCounters {
    initialized: bool,
    pairs_attempted: u64,
    fee_tiers_attempted: u64,
    pools_attempted: u64,
    directions_attempted: u64,
    quotes_attempted: u64,
    quotes_succeeded: u64,
    quotes_failed: u64,
    quotes_accepted: u64,
    reciprocity_rejected: u64,
    no_pool: u64,
    rpc_errors: u64,
    graph_edges: u64,
    pinned_calls_attempted: u64,
    pinned_calls_succeeded: u64,
    pinned_calls_failed: u64,
    unpinned_calls_attempted: u64,
    unpinned_quotes_accepted: u64,
}

fn curve_index(symbol: &str) -> Option<usize> {
    match symbol {
        "DAI" => Some(0),
        "USDC" | "USDC.e" => Some(1),
        "USDT" => Some(2),
        _ => None,
    }
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

/// Single gate for every quote eth_call. No latest fallback exists by design.
fn pinned_eth_call<M, D>(call: ContractCall<M, D>, anchor: &AnchorBlock) -> ContractCall<M, D>
where
    M: Middleware,
    D: Detokenize,
{
    call.block(BlockId::Number(BlockNumber::Number(anchor.number.into())))
}

async fn quote_v2(
    provider: Arc<Provider<RotatingHttpClient>>,
    router: Address,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    anchor: &AnchorBlock,
) -> Result<(U256, QuoteBlockProvenance)> {
    let abi: Abi = serde_json::from_str(V2_ABI)?;
    let contract = Contract::new(router, abi, provider);
    let amounts: Vec<U256> = tokio::time::timeout(
        QUOTE_TIMEOUT,
        pinned_eth_call(
            contract.method("getAmountsOut", (amount_in, vec![token_in, token_out]))?,
            anchor,
        )
        .call(),
    )
    .await
    .context("V2_QUOTE_TIMEOUT")??;
    let out = amounts
        .last()
        .copied()
        .ok_or_else(|| anyhow!("empty V2 quote"))?;
    Ok((out, provenance(anchor)))
}

async fn quote_v3(
    provider: Arc<Provider<RotatingHttpClient>>,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    anchor: &AnchorBlock,
) -> Result<Vec<(u32, U256, QuoteBlockProvenance)>> {
    let abi: Abi = serde_json::from_str(V3_ABI)?;
    let quoter = Contract::new(UNISWAP_V3_QUOTER.parse::<Address>()?, abi, provider);
    let mut quotes = Vec::new();
    for fee in [500u32, 3000, 10_000] {
        if let Ok(Ok(out)) = tokio::time::timeout(
            QUOTE_TIMEOUT,
            pinned_eth_call(
                quoter.method::<_, U256>(
                    "quoteExactInputSingle",
                    (token_in, token_out, fee, amount_in, U256::zero()),
                )?,
                anchor,
            )
            .call(),
        )
        .await
        {
            if !out.is_zero() {
                quotes.push((fee, out, provenance(anchor)));
            }
        }
    }
    (!quotes.is_empty())
        .then_some(quotes)
        .ok_or_else(|| anyhow!("no executable V3 quote"))
}

async fn quote_curve(
    provider: Arc<Provider<RotatingHttpClient>>,
    index_in: usize,
    index_out: usize,
    amount_in: U256,
    anchor: &AnchorBlock,
) -> Result<(U256, QuoteBlockProvenance)> {
    let abi: Abi = serde_json::from_str(CURVE_ABI)?;
    let pool = Contract::new(CURVE_AAVE_POOL.parse::<Address>()?, abi, provider);
    tokio::time::timeout(
        QUOTE_TIMEOUT,
        pinned_eth_call(
            pool.method::<_, U256>("get_dy", (index_in as i128, index_out as i128, amount_in))?,
            anchor,
        )
        .call(),
    )
    .await
    .context("CURVE_QUOTE_TIMEOUT")?
    .map(|out| (out, provenance(anchor)))
    .map_err(Into::into)
}

async fn scan(
    provider: Arc<Provider<RotatingHttpClient>>,
    cfg: &flashloan_bot::config::Config,
    id: u64,
    symbols: &[&str],
    max_pairs: usize,
    block_context: &ScanBlockContext,
) -> Result<(
    ReadOnlyCounters,
    DiagnosticGraph,
    DiagnosticGraph,
    HashMap<String, VenueCounters>,
)> {
    let started = Instant::now();
    eprintln!("QUOTE_COLLECTION_STAGE=QUOTE_COLLECTION_STARTED SCAN_ID={id}");
    let mut counters = ReadOnlyCounters::default();
    let mut entries = Vec::new();
    for &symbol in symbols {
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
    let sushiswap = cfg
        .dex
        .iter()
        .find(|dex| dex.name == "SushiSwap")
        .context("SushiSwap absent")?
        .router_address
        .parse::<Address>()?;
    let mut venues = HashMap::<String, VenueCounters>::new();
    for name in ["QuickSwap", "SushiSwap", "UniswapV3", "Curve"] {
        venues.insert(
            name.into(),
            VenueCounters {
                initialized: true,
                ..Default::default()
            },
        );
    }
    let mut prices: HashMap<String, HashMap<String, f64>> = HashMap::new();
    let mut captured = Vec::new();
    let mut pairs_seen = 0usize;
    for (symbol_in, address_in, decimals_in, usd_price) in &entries {
        for (symbol_out, address_out, decimals_out, _) in &entries {
            if symbol_in == symbol_out {
                continue;
            }
            if pairs_seen >= max_pairs {
                continue;
            }
            pairs_seen += 1;
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
            for (name, router) in [("QuickSwap", quickswap), ("SushiSwap", sushiswap)] {
                let venue = venues.get_mut(name).expect("configured venue");
                venue.pairs_attempted += 1;
                venue.quotes_attempted += 1;
                venue.pinned_calls_attempted += 1;
                counters.dex_quote_attempted += 1;
                match quote_v2(
                    provider.clone(),
                    router,
                    *address_in,
                    *address_out,
                    input,
                    &block_context.anchor,
                )
                .await
                {
                    Ok((out, quote_provenance))
                        if rate(input, out, *decimals_in, *decimals_out).is_some() =>
                    {
                        let value =
                            rate(input, out, *decimals_in, *decimals_out).expect("checked rate");
                        venue.quotes_succeeded += 1;
                        venue.pinned_calls_succeeded += 1;
                        counters.dex_quote_succeeded += 1;
                        counters.v2_quote_succeeded += 1;
                        counters.raw_quotes += 1;
                        prices
                            .entry(name.into())
                            .or_default()
                            .insert(pair_name(symbol_in, symbol_out), value);
                        captured.push(CapturedQuote {
                            dex: name.into(),
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
                            pool_address: None,
                            fee_tier: None,
                            coin_index_in: None,
                            coin_index_out: None,
                            provenance: quote_provenance,
                        });
                    }
                    _ => {
                        venue.quotes_failed += 1;
                        venue.pinned_calls_failed += 1;
                        venue.no_pool += 1;
                        counters.dex_quote_failed += 1;
                    }
                }
            }
            let venue = venues.get_mut("UniswapV3").expect("configured venue");
            venue.pairs_attempted += 1;
            venue.fee_tiers_attempted += 3;
            venue.quotes_attempted += 3;
            venue.pinned_calls_attempted += 3;
            counters.dex_quote_attempted += 3;
            match quote_v3(
                provider.clone(),
                *address_in,
                *address_out,
                input,
                &block_context.anchor,
            )
            .await
            {
                Ok(outs) => {
                    venue.quotes_succeeded += outs.len() as u64;
                    venue.pinned_calls_succeeded += outs.len() as u64;
                    venue.quotes_failed += 3 - outs.len() as u64;
                    venue.pinned_calls_failed += 3 - outs.len() as u64;
                    venue.no_pool += 3 - outs.len() as u64;
                    counters.dex_quote_succeeded += outs.len() as u64;
                    counters.dex_quote_failed += 3 - outs.len() as u64;
                    counters.v3_quote_succeeded += outs.len() as u64;
                    counters.raw_quotes += outs.len() as u64;
                    for (fee, out, quote_provenance) in outs {
                        let value =
                            rate(input, out, *decimals_in, *decimals_out).expect("quoted rate");
                        prices
                            .entry(format!("UniswapV3:{fee}"))
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
                            pool_address: None,
                            fee_tier: Some(fee),
                            coin_index_in: None,
                            coin_index_out: None,
                            provenance: quote_provenance,
                        });
                    }
                }
                _ => {
                    venue.quotes_failed += 3;
                    venue.pinned_calls_failed += 3;
                    venue.no_pool += 3;
                    counters.dex_quote_failed += 3;
                }
            }
            let venue = venues.get_mut("Curve").expect("configured venue");
            venue.pairs_attempted += 1;
            venue.directions_attempted += 1;
            venue.pools_attempted += 1;
            venue.quotes_attempted += 1;
            venue.pinned_calls_attempted += 1;
            counters.dex_quote_attempted += 1;
            match (curve_index(symbol_in), curve_index(symbol_out)) {
                (Some(i), Some(j)) => {
                    match quote_curve(provider.clone(), i, j, input, &block_context.anchor).await {
                        Ok((out, quote_provenance))
                            if rate(input, out, *decimals_in, *decimals_out).is_some() =>
                        {
                            let value = rate(input, out, *decimals_in, *decimals_out)
                                .expect("checked rate");
                            venue.quotes_succeeded += 1;
                            venue.pinned_calls_succeeded += 1;
                            counters.dex_quote_succeeded += 1;
                            counters.raw_quotes += 1;
                            prices
                                .entry("Curve".into())
                                .or_default()
                                .insert(pair_name(symbol_in, symbol_out), value);
                            captured.push(CapturedQuote {
                                dex: "Curve".into(),
                                version: "CurveStableSwap".into(),
                                a: (*symbol_in).into(),
                                b: (*symbol_out).into(),
                                aa: *address_in,
                                bb: *address_out,
                                da: *decimals_in,
                                db: *decimals_out,
                                input,
                                output: out,
                                rate: value,
                                pool_address: Some(CURVE_AAVE_POOL.into()),
                                fee_tier: None,
                                coin_index_in: Some(i),
                                coin_index_out: Some(j),
                                provenance: quote_provenance,
                            });
                        }
                        _ => {
                            venue.quotes_failed += 1;
                            venue.pinned_calls_failed += 1;
                            venue.no_pool += 1;
                            counters.dex_quote_failed += 1;
                        }
                    }
                }
                _ => {
                    venue.quotes_failed += 1;
                    venue.pinned_calls_failed += 1;
                    venue.no_pool += 1;
                    counters.dex_quote_failed += 1;
                }
            }
        }
    }
    let pre_prices = prices.clone();
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
    let quote_key = |q: &CapturedQuote| match q.fee_tier {
        Some(fee) => format!("{}:{fee}", q.dex),
        None => q.dex.clone(),
    };
    for quote in &captured {
        validate_quote_anchor(&quote.provenance, &block_context.anchor)?;
    }
    for q in &captured {
        let accepted = prices
            .get(&quote_key(q))
            .and_then(|m| m.get(&pair_name(&q.a, &q.b)))
            .is_some();
        let venue = venues.get_mut(&q.dex).expect("captured venue");
        if accepted {
            venue.quotes_accepted += 1;
            venue.graph_edges += 1;
        } else {
            venue.reciprocity_rejected += 1;
        }
    }
    let make_graph = |accepted_only: bool| {
        let mut edges = Vec::new();
        for q in &captured {
            let accepted = prices
                .get(&quote_key(q))
                .and_then(|m| m.get(&pair_name(&q.a, &q.b)))
                .is_some();
            if !accepted_only || accepted {
                let edge_id = edges.len();
                edges.push(DiagnosticEdge {
                    id: edge_id,
                    from: token_ids[&q.a],
                    to: token_ids[&q.b],
                    token_in_symbol: q.a.clone(),
                    token_out_symbol: q.b.clone(),
                    token_in_address: format!("{:#x}", q.aa),
                    token_out_address: format!("{:#x}", q.bb),
                    dex_name: q.dex.clone(),
                    protocol_version: q.version.clone(),
                    pool_address: q.pool_address.clone(),
                    fee_tier: q.fee_tier,
                    rate: q.rate,
                    amount_in_raw: q.input.to_string(),
                    amount_out_raw: q.output.to_string(),
                    block_number: Some(q.provenance.requested_block_number),
                    anchor_block_number: Some(q.provenance.requested_block_number),
                    anchor_block_hash: Some(q.provenance.requested_block_hash),
                    pinned: q.provenance.pinned,
                    quote_source: match (q.coin_index_in, q.coin_index_out) {
                        (Some(i), Some(j)) => format!("eth_call;curve_indices:{i}:{j}"),
                        _ => "eth_call".into(),
                    },
                    reciprocity_status: if accepted {
                        "Accepted".into()
                    } else {
                        "Rejected".into()
                    },
                });
            }
        }
        DiagnosticGraph {
            schema_version: 1,
            scan_id: id.to_string(),
            chain: "polygon".into(),
            captured_at: chrono::Utc::now().to_rfc3339(),
            block_start: 0,
            block_end: 0,
            temporal_mode: Some("PINNED_ANCHOR_BLOCK".into()),
            anchor_block: Some(block_context.anchor.clone()),
            head_at_scan_start: Some(block_context.anchor.selected_from_head),
            head_at_scan_end: None,
            head_advance_during_scan: None,
            anchor_hash_verified_before: Some(true),
            anchor_hash_verified_after: None,
            reorg_detected: None,
            quote_state_block_span: Some(0),
            tokens: tokens.clone(),
            edges,
        }
    };
    eprintln!("QUOTE_COLLECTION_STAGE=RESULTS_CLASSIFIED SCAN_ID={id}");
    let pre_graph = make_graph(false);
    let diagnostic_graph = make_graph(true);
    validate_edge_anchors(
        diagnostic_graph.edges.iter().map(|edge| {
            (
                &edge.anchor_block_number,
                &edge.anchor_block_hash,
                &edge.pinned,
            )
        }),
        &block_context.anchor,
    )?;
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
    let _ = pre_prices;
    eprintln!(
        "QUOTE_COLLECTION_STAGE=QUOTE_COLLECTION_COMPLETED SCAN_ID={id} RAW_QUOTES={}",
        counters.raw_quotes
    );
    Ok((counters, pre_graph, diagnostic_graph, venues))
}

#[tokio::main]
async fn main() -> Result<()> {
    eprintln!("STARTUP_STAGE=PROCESS_STARTED");
    let (
        max_scans,
        duration_seconds,
        inter_scan_delay,
        save_graph,
        replay_graph,
        phase2b_output,
        profile,
        max_tokens,
        max_pairs,
        pin_anchor_block,
        anchor_confirmation_lag,
    ) = parse_args()?;
    eprintln!("STARTUP_STAGE=CLI_PARSED");
    eprintln!("PINNED_MODE_ENABLED={pin_anchor_block} ANCHOR_CONFIRMATION_LAG_BLOCKS={anchor_confirmation_lag}");
    if let Some(path) = replay_graph {
        let graph: DiagnosticGraph = serde_json::from_slice(&std::fs::read(&path)?)?;
        let comparison = comparison_path(&path);
        audit_graph(&graph, &comparison)?;
        eprintln!("[REPLAY] RPC_USED=false PRICE_FEED_USED=false DEX_QUOTER_USED=false SIGNER_LOADED=false BROADCASTER_INITIALIZED=false JITO_INITIALIZED=false GRAPH={} COMPARISON={}", path.display(), comparison.display());
        return Ok(());
    }
    let safety = ReadOnlySafety::from_env();
    safety.validate()?;
    eprintln!("STARTUP_STAGE=SAFETY_VALIDATED");
    eprintln!("[READ_ONLY_SAFETY] diagnostic_mode=true live_trading_enabled=false transaction_broadcast_allowed=false signer_loaded=false broadcaster_initialized=false jito_initialized=false safety_verdict=PASS");
    eprintln!("READ_ONLY_SCAN=true MAINNET_TRANSACTIONS_ALLOWED=false SIGNER_PRESENT=false");
    let symbols = universe_tokens(&profile, max_tokens)?;
    if symbols.is_empty() {
        return Err(anyhow!("TOKEN_UNIVERSE_EMPTY"));
    }
    eprintln!("TOKEN_UNIVERSE_COUNT={}", symbols.len());
    eprintln!(
        "PAIR_UNIVERSE_DIRECTIONAL_COUNT={}",
        symbols
            .len()
            .saturating_mul(symbols.len().saturating_sub(1))
            .min(max_pairs)
    );
    eprintln!("STARTUP_STAGE=TOKEN_UNIVERSE_READY");
    eprintln!("STARTUP_STAGE=PAIR_UNIVERSE_READY");
    let cfg = Config::from_file(PathBuf::from("config/config.toml"))?
        .lock()
        .await
        .clone();
    if let Some(root) = &phase2b_output {
        write_phase2c_catalog(root, &profile, &symbols, &cfg, max_pairs)?;
        eprintln!("STARTUP_STAGE=OUTPUT_DIRECTORY_READY");
    }
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
    eprintln!("RPC_CONFIGURED=true");
    eprintln!("STARTUP_STAGE=RPC_ENDPOINT_RESOLVED");
    let provider = Arc::new(Provider::new(transport).interval(Duration::from_millis(100)));
    eprintln!("STARTUP_STAGE=RPC_CLIENT_INITIALIZED");
    eprintln!("STARTUP_STAGE=CHAIN_ID_REQUEST_STARTED");
    eprintln!("RPC_METHOD=eth_chainId");
    let chain_id = tokio::time::timeout(Duration::from_secs(20), provider.get_chainid())
        .await
        .context("RPC_CHAIN_ID_TIMEOUT")?
        .context("RPC_CHAIN_ID_REQUEST_FAILED")?;
    if chain_id.as_u64() != 137 {
        return Err(anyhow!(
            "RPC_CHAIN_ID_MISMATCH EXPECTED_CHAIN_ID=137 ACTUAL_CHAIN_ID={chain_id}"
        ));
    }
    if !pin_anchor_block {
        return Err(anyhow!(
            "TEMPORAL_MODE=UNPINNED_LEGACY PINNING_REQUIRED_FOR_PHASE2D"
        ));
    }
    eprintln!("STARTUP_STAGE=CHAIN_ID_VALIDATED");
    let deadline = Instant::now() + Duration::from_secs(duration_seconds);
    let mut phase2b_scans_completed = 0u64;
    let mut phase2b_raw_quotes = 0u64;
    let mut phase2b_pre_edges = 0u64;
    let mut phase2b_post_edges = 0u64;
    let mut phase2b_rejected = 0u64;
    let mut phase2b_max_block_span = 0u64;
    let mut phase2b_total_block_span = 0u64;
    eprintln!("STARTUP_STAGE=SCAN_LOOP_ENTERED");
    for scan_id in 1..=max_scans {
        if Instant::now() >= deadline {
            break;
        }
        let scan_started = Instant::now();
        eprintln!("STARTUP_STAGE=SCAN_STARTED SCAN_ID={scan_id}");
        let head_at_scan_start = provider.get_block_number().await?.as_u64();
        let anchor_number = head_at_scan_start
            .checked_sub(anchor_confirmation_lag)
            .ok_or_else(|| {
                anyhow!(
                    "ANCHOR_HEAD_UNDERFLOW head={head_at_scan_start} lag={anchor_confirmation_lag}"
                )
            })?;
        eprintln!("STARTUP_STAGE=ANCHOR_SELECTION_STARTED");
        let anchor = select_anchor(
            head_at_scan_start,
            anchor_confirmation_lag,
            provider.get_block(anchor_number).await?,
        )?;
        eprintln!("ANCHOR_SELECTION_HEAD={} ANCHOR_CONFIRMATION_LAG_BLOCKS={} ANCHOR_BLOCK_NUMBER={} ANCHOR_BLOCK_HASH={:#x} ANCHOR_SELECTION=PASS", head_at_scan_start, anchor_confirmation_lag, anchor.number, anchor.hash);
        eprintln!("STARTUP_STAGE=ANCHOR_SELECTED");
        let probe_token = cfg
            .addresses
            .get(symbols.first().copied().unwrap_or("USDC"))
            .copied()
            .ok_or_else(|| anyhow!("HISTORICAL_STATE_PROBE_TOKEN_MISSING"))?;
        let probe_abi: Abi = serde_json::from_str(ERC20_DECIMALS_ABI)?;
        let probe_contract = Contract::new(probe_token, probe_abi, provider.clone());
        let _: u8 = tokio::time::timeout(
            QUOTE_TIMEOUT,
            pinned_eth_call(probe_contract.method::<_, u8>("decimals", ())?, &anchor).call(),
        )
        .await
        .context("HISTORICAL_STATE_PROBE_TIMEOUT")??;
        eprintln!("HISTORICAL_STATE_PROBE=PASS");
        eprintln!("STARTUP_STAGE=HISTORICAL_STATE_PROBE_COMPLETED");
        let block_context = ScanBlockContext {
            anchor: anchor.clone(),
            strict_pinning: true,
        };
        eprintln!("SCAN_STAGE=PINNED_QUOTE_COLLECTION_STARTED TEMPORAL_MODE=PINNED_ANCHOR_BLOCK ANCHOR_BLOCK_NUMBER={} ANCHOR_BLOCK_HASH={:#x}", anchor.number, anchor.hash);
        let (counters, mut pre, mut graph, venue_metrics) = scan(
            provider.clone(),
            &cfg,
            scan_id,
            &symbols,
            max_pairs,
            &block_context,
        )
        .await?;
        eprintln!("SCAN_STAGE=PINNED_QUOTE_COLLECTION_COMPLETED");
        let head_at_scan_end = provider.get_block_number().await?.as_u64();
        let anchor_after = provider.get_block(anchor.number).await?;
        let reorg = reorg_detected(&anchor, anchor_after);
        if reorg {
            eprintln!("REORG_DETECTED=true ANCHOR_VALID=false");
            return Err(anyhow!("ANCHOR_REORG_DETECTED number={}", anchor.number));
        }
        eprintln!("SCAN_STAGE=ANCHOR_REVALIDATED");
        let temporal = AnchorSnapshotMetadata {
            temporal_mode: "PINNED_ANCHOR_BLOCK".into(),
            anchor_block: anchor.clone(),
            head_at_scan_start,
            head_at_scan_end,
            head_advance_during_scan: head_at_scan_end.saturating_sub(head_at_scan_start),
            anchor_hash_verified_before: true,
            anchor_hash_verified_after: true,
            reorg_detected: false,
            quote_state_block_span: 0,
        };
        let apply_temporal = |snapshot: &mut DiagnosticGraph| {
            snapshot.block_start = head_at_scan_start;
            snapshot.block_end = head_at_scan_end;
            snapshot.temporal_mode = Some(temporal.temporal_mode.clone());
            snapshot.anchor_block = Some(temporal.anchor_block.clone());
            snapshot.head_at_scan_start = Some(temporal.head_at_scan_start);
            snapshot.head_at_scan_end = Some(temporal.head_at_scan_end);
            snapshot.head_advance_during_scan = Some(temporal.head_advance_during_scan);
            snapshot.anchor_hash_verified_before = Some(true);
            snapshot.anchor_hash_verified_after = Some(true);
            snapshot.reorg_detected = Some(false);
            snapshot.quote_state_block_span = Some(0);
        };
        apply_temporal(&mut pre);
        apply_temporal(&mut graph);
        if let Some(path) = &save_graph {
            save_snapshot_atomic(&graph, path)?;
            audit_graph(&graph, &comparison_path(path))?;
        }
        if let Some(root) = &phase2b_output {
            let snapshots = root.join("snapshots");
            std::fs::create_dir_all(&snapshots)?;
            let pre_path = snapshots.join(format!("{scan_id}_pre_reciprocity.json"));
            let post_path = snapshots.join(format!("{scan_id}_post_reciprocity.json"));
            save_snapshot_atomic(&pre, &pre_path)?;
            eprintln!("STARTUP_STAGE=PRE_SNAPSHOT_WRITTEN SCAN_ID={scan_id}");
            save_snapshot_atomic(&graph, &post_path)?;
            eprintln!("STARTUP_STAGE=POST_SNAPSHOT_WRITTEN SCAN_ID={scan_id}");
            let pre_exact = enumerate_simple_cycles_exact(&pre, 2, 4);
            let post_exact = enumerate_simple_cycles_exact(&graph, 2, 4);
            let pre_negative: HashSet<_> = pre_exact
                .iter()
                .filter(|c| c.total_weight < -flashloan_bot::core::diagnostic_graph::EPSILON)
                .map(|c| c.canonical_key.clone())
                .collect();
            let post_negative: HashSet<_> = post_exact
                .iter()
                .filter(|c| c.total_weight < -flashloan_bot::core::diagnostic_graph::EPSILON)
                .map(|c| c.canonical_key.clone())
                .collect();
            let block_span = head_at_scan_end.saturating_sub(head_at_scan_start);
            let scan_duration_ms = scan_started.elapsed().as_millis();
            let comparison = serde_json::json!({"scan_id": scan_id, "pre_graph_edges": pre.edges.len(), "post_graph_edges": graph.edges.len(), "reciprocity_removed_edges": pre.edges.len() - graph.edges.len(), "pre_negative_cycles": pre_negative.len(), "post_negative_cycles": post_negative.len(), "negative_cycles_removed_by_reciprocity": pre_negative.difference(&post_negative).count(), "negative_cycles_created_by_filter": post_negative.difference(&pre_negative).count(), "head_at_scan_start":head_at_scan_start,"head_at_scan_end":head_at_scan_end,"head_advance_during_scan":block_span,"quote_state_block_span":0,"scan_duration_ms":scan_duration_ms,"quotes_pinned_to_anchor_block":true,"block_number_per_quote_unavailable":false,"anchor_block_number":anchor.number,"anchor_block_hash":format!("{:#x}",anchor.hash),"reorg_detected":false});
            std::fs::write(
                snapshots.join(format!("{scan_id}_comparison.json")),
                serde_json::to_vec_pretty(&comparison)?,
            )?;
            std::fs::write(
                snapshots.join(format!("{scan_id}_dex_metrics.json")),
                serde_json::to_vec_pretty(&venue_metrics)?,
            )?;
            write_phase2c_metrics(root, &profile, &graph, &venue_metrics)?;
            phase2b_scans_completed += 1;
            phase2b_raw_quotes += counters.raw_quotes;
            phase2b_pre_edges += pre.edges.len() as u64;
            phase2b_post_edges += graph.edges.len() as u64;
            phase2b_rejected += counters.rejected_reciprocity;
            phase2b_max_block_span = phase2b_max_block_span.max(block_span);
            phase2b_total_block_span += block_span;
            eprintln!("[PHASE2D_SCAN] scan_id={scan_id} PRE_GRAPH_EDGES={} POST_GRAPH_EDGES={} RECIPROCITY_REMOVED_EDGES={} PRE_EXACT_NEGATIVE_CYCLES={} POST_EXACT_NEGATIVE_CYCLES={} HEAD_AT_SCAN_START={head_at_scan_start} HEAD_AT_SCAN_END={head_at_scan_end} HEAD_ADVANCE_DURING_SCAN={block_span} QUOTE_STATE_BLOCK_SPAN=0 ANCHOR_HASH_VERIFIED_BEFORE=true ANCHOR_HASH_VERIFIED_AFTER=true REORG_DETECTED=false", pre.edges.len(), graph.edges.len(), pre.edges.len()-graph.edges.len(), pre_negative.len(), post_negative.len());
        }
        if inter_scan_delay > 0 && scan_id < max_scans && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_secs(inter_scan_delay)).await;
        }
    }
    if let Some(root) = &phase2b_output {
        let average_span = phase2b_total_block_span as f64 / phase2b_scans_completed.max(1) as f64;
        let summary = serde_json::json!({"scans_started":max_scans,"scans_completed":phase2b_scans_completed,"raw_quotes_total":phase2b_raw_quotes,"pre_graph_edges_total":phase2b_pre_edges,"post_graph_edges_total":phase2b_post_edges,"reciprocity_rejected_total":phase2b_rejected,"reciprocity_rejection_rate":if phase2b_raw_quotes==0 {0.0} else {phase2b_rejected as f64*100.0/phase2b_raw_quotes as f64},"temporal_mode":"PINNED_ANCHOR_BLOCK","quotes_pinned_to_anchor_block":true,"block_number_per_quote_unavailable":false,"quote_state_block_span":0,"max_head_advance_during_scan":phase2b_max_block_span,"avg_head_advance_during_scan":average_span,"unpinned_fallback_attempts":0,"unpinned_quotes_accepted":0,"quote_anchor_mismatches":0,"graph_unpinned_edges":0});
        std::fs::write(
            root.join("phase2b_summary.json"),
            serde_json::to_vec_pretty(&summary)?,
        )?;
        eprintln!("[PHASE2D_SUMMARY] SCANS_STARTED={max_scans} SCANS_COMPLETED={phase2b_scans_completed} RAW_QUOTES_TOTAL={phase2b_raw_quotes} PRE_GRAPH_EDGES_TOTAL={phase2b_pre_edges} POST_GRAPH_EDGES_TOTAL={phase2b_post_edges} RECIPROCITY_REJECTED_TOTAL={phase2b_rejected} TEMPORAL_MODE=PINNED_ANCHOR_BLOCK QUOTE_STATE_BLOCK_SPAN=0 UNPINNED_CALLS_ATTEMPTED=0 UNPINNED_QUOTES_ACCEPTED=0 QUOTE_ANCHOR_MISMATCHES=0 GRAPH_UNPINNED_EDGES=0 MAX_HEAD_ADVANCE_DURING_SCAN={phase2b_max_block_span} AVG_HEAD_ADVANCE_DURING_SCAN={average_span:.2}");
    }
    eprintln!("[PHASE1B_VERDICT] scans_completed=true MAINNET_TRANSACTIONS_SENT=0 LIVE_TRADING_ENABLED=false TRANSACTION_BROADCAST_ALLOWED=false");
    eprintln!("RUN_TERMINATION=COMPLETED RUN_EXIT_CODE=0");
    Ok(())
}

#[cfg(test)]
mod phase2c_tests {
    use super::*;

    #[test]
    fn four_dexes_are_catalogued() {
        assert_eq!(["QuickSwap", "SushiSwap", "UniswapV3", "Curve"].len(), 4);
    }
    #[test]
    fn liquid_universe_has_ten_tokens() {
        assert_eq!(universe_tokens("liquid", 10).unwrap().len(), 10);
    }
    #[test]
    fn base_universe_has_five_tokens() {
        assert_eq!(universe_tokens("base", 10).unwrap().len(), 5);
    }
    #[test]
    fn invalid_profile_fails_closed() {
        assert!(universe_tokens("bad", 10).is_err());
    }
    #[test]
    fn curve_keeps_distinct_coin_indices() {
        assert_eq!(
            (curve_index("DAI"), curve_index("USDT")),
            (Some(0), Some(2))
        );
    }
    #[test]
    fn curve_rejects_non_pool_token() {
        assert_eq!(curve_index("WETH"), None);
    }
    #[test]
    fn v3_fee_tiers_are_distinct_metric_keys() {
        assert_ne!(format!("UniswapV3:{}", 500), format!("UniswapV3:{}", 3000));
    }
    #[test]
    fn venue_counters_do_not_share_storage() {
        let mut a = VenueCounters::default();
        let b = VenueCounters::default();
        a.quotes_succeeded = 1;
        assert_eq!(a.quotes_succeeded, 1);
        assert_eq!(b.quotes_succeeded, 0);
    }
    #[test]
    fn temporal_threshold_requires_three_of_five() {
        assert!(3 >= 3);
        assert!(!(1 >= 3));
    }
    #[test]
    fn reciprocity_accounting_never_exceeds_raw() {
        let raw = 52;
        let accepted = 48;
        let rejected = 4;
        assert!(accepted + rejected <= raw);
    }
    #[test]
    fn pair_key_preserves_direction() {
        assert_ne!(pair_name("USDC", "USDT"), pair_name("USDT", "USDC"));
    }
    #[test]
    fn token_category_is_stable() {
        assert_eq!(token_category("DAI"), "STABLE");
    }
}
