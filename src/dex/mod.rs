// ================================================================
// src/dex/mod.rs â€” v5.1.1-FINAL-SAFE-WBTC (COMPLETO E CORRIGIDO)
// ================================================================
//
// ðŸš€ CORREÃ‡Ã•ES CRÃTICAS E SUporte WBTC:
//    - NOVO: get_pair_or_pool_address() no trait (Factory Check).
//    - EndereÃ§os de token agora dependem do campo `addresses` da Config (Fix E0609).
//
// ================================================================

use crate::{
    config::{token_cache::TokenCache, Config},
    AppMiddleware,
    utils::utils::u256_to_f64_precise, 
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use ethers::{
    types::{Address, U256}
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::RwLock;
use once_cell::sync::Lazy;
use tracing::{debug, warn};

// ================================================================
// V3 FEE TIERS — fonte única (executor vs observação)
// ================================================================
// FlashloanExecutor on-chain: `require(fee == 500 || fee == 3000 || fee == 10000)`.
// Fee 100 (1 bps) existe no Quoter/Factory, mas **não** é executável sem redeploy.
// Rota EXECUTÁVEL = só EXECUTABLE_V3_FEE_TIERS.
// Fee 100 pode ser cotado para métrica (descartes), nunca cacheado para exec/extraData.

/// Tiers que o `FlashloanExecutor` aceita — única fonte para seleção + A4.
pub const EXECUTABLE_V3_FEE_TIERS: [u32; 3] = [500, 3000, 10_000];

/// Fee 100: só observação/métrica. Nunca alimenta cache executável nem extraData.
pub const OBSERVED_V3_FEE_TIER_100: u32 = 100;

/// Todos os tiers cotáveis no Quoter (métrica + seleção). Seleção filtra via
/// [`select_executable_v3_best_out`].
pub const QUOTE_V3_FEE_TIERS: [u32; 4] = [100, 500, 3000, 10_000];

static FEE100_BEST_DISCARDED: AtomicU64 = AtomicU64::new(0);

/// True se o tier é aceito pelo executor on-chain.
#[inline]
pub fn is_executable_v3_fee_tier(fee: u32) -> bool {
    matches!(fee, 500 | 3000 | 10_000)
}

/// Contador: vezes em que fee 100 teria sido o best-out absoluto mas foi
/// descartado da rota executável (alternativa exec ou nenhuma).
pub fn fee100_best_discarded_count() -> u64 {
    FEE100_BEST_DISCARDED.load(Ordering::Relaxed)
}

pub fn reset_fee100_best_discarded_count() {
    FEE100_BEST_DISCARDED.store(0, Ordering::Relaxed)
}

fn note_fee100_best_discarded() {
    FEE100_BEST_DISCARDED.fetch_add(1, Ordering::Relaxed);
}

/// Escolhe o melhor `(fee, amount_out)` **somente** entre tiers executáveis.
///
/// - Se fee 100 teria o maior `amountOut` (global), incrementa métrica + log warn.
/// - Se nenhum tier executável tem quote > 0 → `None` (opp descartada; sem fallback 100).
pub fn select_executable_v3_best_out(quotes: &[(u32, U256)]) -> Option<(u32, U256)> {
    let mut best_any: Option<(u32, U256)> = None;
    let mut best_exec: Option<(u32, U256)> = None;

    for &(fee, out) in quotes {
        if out.is_zero() {
            continue;
        }
        if best_any.map(|(_, o)| out > o).unwrap_or(true) {
            best_any = Some((fee, out));
        }
        if is_executable_v3_fee_tier(fee) && best_exec.map(|(_, o)| out > o).unwrap_or(true) {
            best_exec = Some((fee, out));
        }
    }

    if let Some((fee, out100)) = best_any {
        if fee == OBSERVED_V3_FEE_TIER_100 {
            note_fee100_best_discarded();
            match best_exec {
                Some((exec_fee, exec_out)) => {
                    warn!(
                        target: "v3_fee_select",
                        fee100_out = %out100,
                        exec_fee,
                        exec_out = %exec_out,
                        "V3 fee=100 seria best-out mas descartado (não executável); usando tier executável"
                    );
                }
                None => {
                    warn!(
                        target: "v3_fee_select",
                        fee100_out = %out100,
                        "V3 fee=100 seria best-out e nenhum tier executável cotou — par descartado"
                    );
                }
            }
        }
    }

    best_exec
}

// ================================================================
// CACHE GLOBAL DE FEE TIERS (para V3 pools)
// ================================================================
// Armazena o fee tier **executável** de cada par V3 (uint24: 500/3000/10000).
// Nunca deve gravar 100. Chave: "DEX_NAME:{fee_cache_key}" → fee_tier.
static FEE_TIER_CACHE: Lazy<RwLock<std::collections::HashMap<String, u32>>> =
    Lazy::new(|| RwLock::new(std::collections::HashMap::new()));

/// Chave canônica do par para `FEE_TIER_CACHE`.
///
/// Convenção: símbolos em ASCII uppercase, ordenados lexicograficamente,
/// unidos por `-`. Assim `key(A,B) == key(B,A)` (fee do pool V3 é o mesmo
/// nas duas direções). Ex.: `fee_cache_key("WETH", "USDC") == "USDC-WETH"`.
///
/// Todos os writers (`get_price`, multicall) e readers (`cached_fee_tier*`)
/// DEVEM usar esta função — nunca hex de endereço nem `"A-B"` direcional cru.
pub fn fee_cache_key(token_a: &str, token_b: &str) -> String {
    let a = token_a.to_ascii_uppercase();
    let b = token_b.to_ascii_uppercase();
    if a <= b {
        format!("{}-{}", a, b)
    } else {
        format!("{}-{}", b, a)
    }
}

/// Registra o fee tier V3 (`uint24` nativo) no cache global.
/// Recusa fee 100 / não-executável (não alimenta A4/extraData com tier inválido).
pub fn cache_fee_tier(dex_name: &str, token_a: &str, token_b: &str, fee_tier: u32) {
    if !is_executable_v3_fee_tier(fee_tier) {
        warn!(
            target: "v3_fee_select",
            fee_tier,
            "cache_fee_tier recusou tier não-executável (não gravar 100 no cache)"
        );
        return;
    }
    let pair = fee_cache_key(token_a, token_b);
    let key = format!("{}:{}", dex_name, pair);
    if let Ok(mut cache) = FEE_TIER_CACHE.write() {
        cache.insert(key, fee_tier);
        debug!(
            "[FEE TIER CACHE SET] {}:{} => fee_tier={}",
            dex_name, pair, fee_tier
        );
    }
}

/// Retorna o fee tier V3 do cache (`uint24`), ou `None` se miss.
pub fn cached_fee_tier(dex_name: &str, token_a: &str, token_b: &str) -> Option<u32> {
    let pair = fee_cache_key(token_a, token_b);
    let key = format!("{}:{}", dex_name, pair);
    FEE_TIER_CACHE.read().ok()?.get(&key).cloned()
}

/// Lookup a partir de par direcional `"A-B"` (radar/engine). Normaliza via `fee_cache_key`.
pub fn cached_fee_tier_pair(dex_name: &str, pair: &str) -> Option<u32> {
    let (a, b) = pair.split_once('-')?;
    cached_fee_tier(dex_name, a, b)
}

// ================================================================
// TRAIT PRINCIPAL (interface comum para todos os DEX adapters)
// ================================================================

#[async_trait]
pub trait DexContract: Send + Sync {
    fn name(&self) -> String;

    // âœ… Assinatura correta
    async fn get_price(&self, token_a: &Address, token_b: &Address) -> Result<Option<f64>>;
    
    async fn get_prices_multicall(
        &self,
        pairs: &[(String, String)],
    ) -> Result<Vec<TokenPairPrice>>;

    async fn swap(&self, token_in: Address, token_out: Address, amount_in: U256) -> Result<U256>;
    
    // 🚀 NOVO MÉTODO CRÍTICO: Consulta a Factory/Quoter para obter o endereço do Par/Pool.
    async fn get_pair_or_pool_address(
        &self,
        token_a: Address,
        token_b: Address,
    ) -> Result<Option<Address>>;

    /// Pool usado no gate de liquidez. `fee_hint` = fee V3 do quote (executável).
    /// Default: ignora fee (V2/Curve). V3 sobrescreve para bater no pool cotado.
    async fn get_pool_address_for_liquidity(
        &self,
        token_a: Address,
        token_b: Address,
        fee_hint: u32,
    ) -> Result<Option<Address>> {
        let _ = fee_hint;
        self.get_pair_or_pool_address(token_a, token_b).await
    }

    /// Endereços dos tokens cujo `balanceOf(pool)` representa a TVL do par.
    /// Default: `None` → o gate usa os próprios tokens do par (V2/V3: o pool
    /// custodia os tokens raw). Curve (am3CRV) sobrescreve: a pool custodia
    /// **amTokens**, não os stables raw, então `balanceOf(raw_token, pool) ≈ 0`
    /// → TVL 0 → preço Curve descartado mesmo a pool sendo líquida. Retorna os
    /// amTokens correspondentes para o gate medir a custódia real. Decimais e
    /// preço do amToken == do raw (amUSDC 6dec $1 == USDC 6dec $1), então só o
    /// endereço do `balanceOf` muda. (Audit A12)
    async fn liquidity_token_addresses(
        &self,
        _token_a: Address,
        _token_b: Address,
    ) -> Option<(Address, Address)> {
        None
    }
    async fn get_token_address(&self, symbol_or_address: &str) -> Result<Address> {
        use std::str::FromStr;
        let cache = TokenCache::global(self.config().clone()).await;
        let token_str = symbol_or_address.trim();
        if token_str.starts_with("0x") && token_str.len() == 42 {
            return Address::from_str(token_str)
                .map_err(|_| anyhow!("EndereÃ§o invÃ¡lido: {}", token_str));
        }
        if let Some(addr) = cache.resolve(token_str).await {
            debug!(
                "ðŸ” [TokenCache] ResoluÃ§Ã£o bem-sucedida: {} â†’ {}",
                token_str, addr
            );
            Ok(addr)
        } else {
            warn!(
                "âš ï¸ Token {} nÃ£o encontrado no cache global (DEX: {})",
                token_str,
                self.name()
            );
            Err(anyhow!(
                "Token {} nÃ£o suportado em {}",
                token_str,
                self.name()
            ))
        }
    }
    async fn get_amount_with_decimals(
        &self,
        token_address: Address,
        base_amount: f64,
    ) -> Result<U256> {
        let decimals =
            get_token_decimals::get_token_decimals(self.client().clone(), token_address).await?;
        let amount = (base_amount * 10f64.powi(decimals as i32)) as u128; 
        Ok(U256::from(amount))
    }
    fn client(&self) -> &Arc<AppMiddleware>;
    fn config(&self) -> &Arc<Config>;
    
    // ðŸ”„ ImplementaÃ§Ã£o padrÃ£o usando Config (AGORA VAI ENCONTRAR O CAMPO `addresses`)
    /// Notional em USD usado para dimensionar as cotacoes de preco.
    ///
    /// Ancorado em `arbitrage.default_trade_amount` para que o spread medido seja o
    /// spread no tamanho que o bot de fato executaria. Ver `quote_amount_for_usd`.
    fn quote_notional_usd(&self) -> f64 {
        self.config()
            .arbitrage
            .default_trade_amount
            .parse::<f64>()
            .unwrap_or(100.0)
    }

    fn get_wmatic_address(&self) -> Option<Address> {
        self.config().addresses.get("WMATIC").cloned()
    }
    fn get_weth_address(&self) -> Option<Address> {
        self.config().addresses.get("WETH").cloned()
    }
    fn get_usdc_address(&self) -> Option<Address> {
        self.config().addresses.get("USDC").cloned()
    }
    fn get_usdt_address(&self) -> Option<Address> {
        self.config().addresses.get("USDT").cloned()
    }
    fn get_dai_address(&self) -> Option<Address> {
        self.config().addresses.get("DAI").cloned()
    }
    // ðŸš€ NOVO: Getter para WBTC
    fn get_wbtc_address(&self) -> Option<Address> {
        self.config().addresses.get("WBTC").cloned()
    }
}

// ================================================================
// ðŸ”¹ FunÃ§Ãµes utilitÃ¡rias (cÃ¡lculo de preÃ§o)
// ================================================================

pub async fn calculate_price_with_decimals(
    client: Arc<AppMiddleware>,
    amount_in: U256,
    amount_out: U256,
    token_in: Address,
    token_out: Address,
) -> Result<f64> {
    let decimals_in = get_token_decimals::get_token_decimals(client.clone(), token_in).await?;
    let decimals_out = get_token_decimals::get_token_decimals(client, token_out).await?;
    calculate_price_from_decimals(amount_in, amount_out, decimals_in, decimals_out)
}

/// Quantidade do token equivalente a um notional em USD, para usar como `amount_in`
/// numa cotação.
///
/// Antes os adapters cotavam `10^decimals` — literalmente **1 unidade do token de
/// entrada**. Isso significa cotar $1 quando a entrada é DAI e ~$2.900 quando é
/// WETH: dois notionais incomparáveis. Quebrava duas coisas de uma vez:
///
/// 1. O spread medido era o spread *naquele* tamanho, não no tamanho que o bot
///    realmente executa (`arbitrage.default_trade_amount`). Pool que parecia
///    lucrativa a 1 DAI escorregava a $100.
/// 2. O produto recíproco `p(A,B) × p(B,A)` misturava price impact em dois
///    tamanhos diferentes, então não dava para separar "cotação corrompida" de
///    "pool rasa para o notional de entrada".
///
/// O preço de referência vem do `PRICE_FEED` (Coingecko, cache de 2 min, com
/// fallback heurístico embutido). Ele só dimensiona a cotação — o preço que vale
/// para lucro é sempre o que o DEX devolve.
pub async fn quote_amount_for_usd(symbol: &str, decimals: u8, usd_notional: f64) -> Result<U256> {
    use crate::infra::price_feed::PRICE_FEED;

    let price_usd = PRICE_FEED.get_price(symbol).await.unwrap_or(0.0);

    if !price_usd.is_finite() || price_usd <= 0.0 || !usd_notional.is_finite() || usd_notional <= 0.0
    {
        // Sem referência utilizável, cai no comportamento antigo (1 unidade). É um
        // tamanho ruim, mas conhecido — melhor que uma quantidade arbitrária.
        warn!(
            "[quote_size] sem preço de referência para {} (usd={}), usando 1 unidade",
            symbol, price_usd
        );
        return Ok(U256::exp10(decimals as usize));
    }

    let units = usd_notional / price_usd;
    let raw = units * 10f64.powi(decimals as i32);

    if !raw.is_finite() || raw < 1.0 || raw >= u128::MAX as f64 {
        warn!(
            "[quote_size] notional ${:.2} em {} gerou quantidade fora de faixa ({:.3e}), usando 1 unidade",
            usd_notional, symbol, raw
        );
        return Ok(U256::exp10(decimals as usize));
    }

    Ok(U256::from(raw as u128))
}

pub fn calculate_price_from_decimals(
    amount_in: U256,
    amount_out: U256,
    decimals_in: u8,
    decimals_out: u8,
) -> Result<f64> {
    
    let amount_in_f64 = u256_to_f64_precise(amount_in, decimals_in);
    let amount_out_f64 = u256_to_f64_precise(amount_out, decimals_out);

    if amount_in_f64 <= 0.0 {
        warn!("âš ï¸ [PRICE_CALC_LOCAL] amount_in zero ou negativo: {}", amount_in_f64);
        return Ok(0.0);
    }

    let price = amount_out_f64 / amount_in_f64;
    
    if price > 1_000_000.0 || price <= 1e-12 {
        // debug, nao warn: pools de poeira (reserva minima) devolvem preco extremo
        // e sao rejeitados aqui. E esperado ao monitorar tokens menos liquidos —
        // logar a cada ciclo por pool inundaria o log. O descarte e o correto.
        debug!("[PRICE_CALC_LOCAL] preco extremo/zero descartado: {:.6}", price);
        return Ok(0.0);
    }

    Ok(price)
}

pub fn validate_price(price: f64) -> bool {
    price > 1e-12 && price < 1e12
}
pub fn normalize_price(price: f64) -> Option<f64> {
    if validate_price(price) {
        Some(price)
    } else {
        None
    }
}


// ================================================================
// SUBMÃ“DULOS INTERNOS
// ================================================================
pub mod adapters;
pub mod circuit_breaker;
pub mod error;
pub mod get_token_decimals;
pub mod liquidity;
pub mod metadata_warm;
pub mod manager;
pub mod radar;
pub mod rate_limiter;
pub mod resilient_dex_system;

// ================================================================
// REEXPORTS
// ================================================================
pub use adapters::{
    uniswap_v2::{UniswapV2Dex, V2Dex}, uniswap_v3::UniswapV3Dex,
};
pub use error::{DexError, DexResult};
pub use get_token_decimals::{cache_token_decimals, cached_token_decimals, get_token_decimals};
pub use manager::DexManager;
pub use radar::start_high_hit_rate_radar;

// ================================================================
// ESTRUTURAS E TIPOS
// ================================================================

#[derive(Clone, Debug)]
pub struct DexConfig {
    pub max_slippage_bps: u64,
    pub min_profit_threshold: f64,
    pub flash_loan_fee_bps: u64,
}
impl Default for DexConfig {
    fn default() -> Self {
        Self {
            max_slippage_bps: 50,
            min_profit_threshold: 0.001,
            flash_loan_fee_bps: 9,
        }
    }
}
#[derive(Clone, Debug)]
pub struct TokenPairPrice {
    pub token_a: String,
    pub token_b: String,
    pub price: f64,
    pub dex_name: String,
    pub timestamp: u64,
    /// Fee tier do pool (unidade nativa V3 uint24: 500/3000/10000). V2 default 3000.
    pub fee_tier_bps: u32,
}
impl TokenPairPrice {
    pub fn new(token_a: String, token_b: String, price: f64, dex_name: String) -> Self {
        Self {
            token_a,
            token_b,
            price,
            dex_name,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            fee_tier_bps: 3000, // Default: 0.3% (V2 e V3 mais comum)
        }
    }

    pub fn with_fee_tier(mut self, fee_tier_bps: u32) -> Self {
        self.fee_tier_bps = fee_tier_bps;
        self
    }

    pub fn is_valid(&self) -> bool {
        validate_price(self.price)
    }
}

#[derive(Clone, Debug)]
pub struct ArbitrageOpportunity {
    pub path: Vec<String>,
    pub profit_percentage: f64,
    pub estimated_profit: f64,
    pub dex_path: Vec<String>,
    pub timestamp: u64,
}
impl ArbitrageOpportunity {
    pub fn is_profitable(&self, min_profit_threshold: f64) -> bool {
        self.profit_percentage >= min_profit_threshold
    }
}


// ================================================================
// CONSTANTES GLOBAIS
// ================================================================
pub mod addresses {
    use ethers::types::Address;
    // ðŸš¨ EndereÃ§os de Factory e Router (COMPLETOS)
    pub const UNISWAP_V2_ROUTER: &str = "0x4752ba5dbc23f44d87826276bf6fd6b1c372ad24";
    pub const UNISWAP_V2_FACTORY: &str = "0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f"; 
    pub const QUICKSWAP_ROUTER: &str = "0xa5E0829CaCEd8fFDD4De3c43696c57F7D7A678ff";
    pub const QUICKSWAP_FACTORY: &str = "0x5757371414417b8C6CA4EcAe1E7c9a75d0a21e56"; // Polygon QuickSwap Factory
    pub const SUSHISWAP_ROUTER: &str = "0x1b02dA8Cb0d097eB8D57A175b88c7D8b47997506";
    pub const SUSHISWAP_FACTORY: &str = "0xc35DADB65012e64E77Df488C1ae0701AFFc320f6"; // Polygon SushiSwap Factory
    pub const UNISWAP_V3_QUOTER: &str = "0xb27308f9F90D607463bb33eA1BeBb41C27CE5AB6";
    pub const UNISWAP_V3_FACTORY: &str = "0x1F98431c8ADf9730573766cd80bd442F4B6c4fC8"; // Uniswap V3 Factory (Polygon)
    pub const QUOTER_V2_ADDRESS: &str = "0x61fFE691821291C350f9C4b99d0e0e5B0d64E8b7"; // Fallback Quoter V2 para V3
    
    // WBTC Polygon PoS (bridged) — alinhado com [pairs.tokens]
    pub const WBTC_ADDRESS: &str = "0x1BFD67037B42Cf73acF2047067bd4F2C47D9BfD6"; 
    
    pub fn router_address(router: &str) -> Address {
        router.parse().expect("EndereÃ§o de router invÃ¡lido")
    }
}
// ðŸš€ NOVO: Pares com WBTC adicionados
pub const SUPPORTED_PAIRS: &[(&str, &str)] = &[
    ("WMATIC", "USDC"),
    ("WMATIC", "USDT"),
    ("WMATIC", "DAI"),
    ("WMATIC", "WETH"),
    ("USDC", "USDT"),
    ("USDC", "DAI"),
    ("USDC", "WETH"),
    ("USDT", "DAI"),
    ("USDT", "WETH"),
    ("DAI", "WETH"),
    // -- WBTC PAIRS --
    ("WBTC", "USDC"),
    ("WBTC", "USDT"),
    ("WBTC", "WETH"),
    ("WBTC", "WMATIC"),
    ("WBTC", "DAI"),
];
pub fn all_trading_pairs() -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for &(a, b) in SUPPORTED_PAIRS {
        pairs.push((a.to_string(), b.to_string()));
        pairs.push((b.to_string(), a.to_string()));
    }
    pairs
}

#[cfg(test)]
mod fee_tier_cache_tests {
    use super::{
        cache_fee_tier, cached_fee_tier, cached_fee_tier_pair, fee_cache_key,
        fee100_best_discarded_count, is_executable_v3_fee_tier, reset_fee100_best_discarded_count,
        select_executable_v3_best_out, EXECUTABLE_V3_FEE_TIERS, OBSERVED_V3_FEE_TIER_100,
    };
    use ethers::types::U256;

    #[test]
    fn fee_cache_key_symmetric_and_uppercase() {
        assert_eq!(fee_cache_key("WETH", "USDC"), "USDC-WETH");
        assert_eq!(fee_cache_key("USDC", "WETH"), fee_cache_key("WETH", "USDC"));
        assert_eq!(fee_cache_key("weth", "usdc"), "USDC-WETH");
    }

    #[test]
    fn fee_tier_cache_write_read_no_miss() {
        // Write com ordem A,B; read com B,A e via par direcional — sem miss → sem default 0.3%.
        cache_fee_tier("UniswapV3", "WETH", "USDC_CACHE_TEST", 500);
        assert_eq!(
            cached_fee_tier("UniswapV3", "USDC_CACHE_TEST", "WETH"),
            Some(500)
        );
        assert_eq!(
            cached_fee_tier_pair("UniswapV3", "WETH-USDC_CACHE_TEST"),
            Some(500)
        );
        assert_eq!(
            cached_fee_tier_pair("UniswapV3", "USDC_CACHE_TEST-WETH"),
            Some(500)
        );
    }

    #[test]
    fn v3_fee_tier_to_fraction_scale() {
        // Documenta a escala correta (espelha ArbitrageEngine::dex_fee).
        let cases = [(500u32, 0.0005), (3000, 0.003), (10_000, 0.01)];
        for (tier, expected) in cases {
            let frac = tier as f64 / 1_000_000.0;
            assert!(
                (frac - expected).abs() < 1e-15,
                "tier {} => {}, expected {}",
                tier,
                frac,
                expected
            );
        }
    }

    #[test]
    fn executable_tiers_are_500_3000_10000_not_100() {
        assert_eq!(EXECUTABLE_V3_FEE_TIERS, [500, 3000, 10_000]);
        assert!(is_executable_v3_fee_tier(500));
        assert!(is_executable_v3_fee_tier(3000));
        assert!(is_executable_v3_fee_tier(10_000));
        assert!(!is_executable_v3_fee_tier(OBSERVED_V3_FEE_TIER_100));
    }

    #[test]
    fn select_executable_prefers_best_exec_over_fee100() {
        reset_fee100_best_discarded_count();
        let before = fee100_best_discarded_count();
        let quotes = [
            (100u32, U256::from(1_000u64)),
            (500u32, U256::from(900u64)),
            (3000u32, U256::from(800u64)),
            (10_000u32, U256::from(700u64)),
        ];
        let (fee, out) = select_executable_v3_best_out(&quotes).expect("exec tier");
        assert_eq!(fee, 500);
        assert_eq!(out, U256::from(900u64));
        assert!(fee100_best_discarded_count() > before);
    }

    #[test]
    fn select_executable_discards_when_only_fee100_quotes() {
        reset_fee100_best_discarded_count();
        let quotes = [(100u32, U256::from(1_000u64))];
        assert!(select_executable_v3_best_out(&quotes).is_none());
        assert!(fee100_best_discarded_count() >= 1);
    }

    #[test]
    fn cache_fee_tier_rejects_100() {
        cache_fee_tier("UniswapV3", "AAA_REJ100", "BBB_REJ100", 100);
        assert_eq!(
            cached_fee_tier("UniswapV3", "AAA_REJ100", "BBB_REJ100"),
            None
        );
    }
}
