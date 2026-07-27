// ============================================================
// src/dex/adapters/curve.rs — Curve Finance Adapter (Polygon)
// ============================================================
// Pool amDAI/amUSDC/amUSDT: fee 0.04%, TVL ~$2.3M
// ============================================================

use crate::{
    config::{token_cache::TokenCache, Config},
    dex::{
        calculate_price_from_decimals,
        normalize_price,
        quote_amount_for_usd,
        rate_limiter::ALCHEMY_RATE_LIMITER,
        DexContract,
        TokenPairPrice,
    },
    AppMiddleware,
};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use ethers::{
    abi::{Abi, Token},
    contract::{Contract, Multicall},
    types::{Address, U64, U256},
};
use std::{str::FromStr, sync::Arc};
use tracing::{debug, info, warn};

const DEX_NAME: &str = "Curve";

// Endereços RAW dos stables na Polygon. O `get_price` (path
// fallback) recebe endereços raw do token_cache, mas `pool_tokens` guarda só os
// amTokens (amDAI/amUSDC/amUSDT). Sem este mapa, `resolve_am_to_symbol` nunca
// casava → fallback Curve sempre retornava None, mesmo p/ stable-stable.
const RAW_STABLE_TO_SYMBOL: &[(&str, &str)] = &[
    ("0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063", "DAI"),  // DAI  raw
    // amUSDC no pool é o USDC.e bridged, não o USDC nativo Circle (0x3c49...).
    ("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174", "USDC.e"),
    ("0xc2132D05D31c914a87C6611C10748AEb04B58e8F", "USDT"), // USDT raw
];

// ABI mínima para Curve pool (get_dy + coins)
const CURVE_POOL_ABI: &str = r#"[
    {
        "name": "get_dy",
        "outputs": [{"type": "uint256", "name": ""}],
        "inputs": [{"type": "int128", "name": "i"}, {"type": "int128", "name": "j"}, {"type": "uint256", "name": "dx"}],
        "stateMutability": "view",
        "type": "function"
    },
    {
        "name": "coins",
        "outputs": [{"type": "address", "name": ""}],
        "inputs": [{"type": "uint256", "name": "i"}],
        "stateMutability": "view",
        "type": "function"
    },
    {
        "name": "balances",
        "outputs": [{"type": "uint256", "name": ""}],
        "inputs": [{"type": "uint256", "name": "i"}],
        "stateMutability": "view",
        "type": "function"
    }
]"#;

// Pool principal de stables na Polygon
const CURVE_AAVE_POOL: &str = "0x445FE580eF8d70FF569aB36e80c647af338db351";

// Tokens wrapped do Aave usados no Curve
const AM_USDC: &str = "0x1a13F4Ca1d028320A707D99520AbFefca3998b7F";
const AM_USDT: &str = "0x60D55F02A771d515e077c9C2403a1ef324885CeC";
const AM_DAI: &str = "0x27F8D03b3a2196956ED754baDc28D73be8830A6e";

#[derive(Clone)]
pub struct CurveDex {
    client: Arc<AppMiddleware>,
    pool_address: Address,
    config: Arc<Config>,
    token_cache: Arc<TokenCache>,
    pool_tokens: Vec<(Address, u8, String)>, // (am_address, decimals, original_symbol)
}

#[derive(Clone)]
struct CurveQuoteCall {
    token_a: String,
    token_b: String,
    idx_a: usize,
    idx_b: usize,
    decimals_a: u8,
    decimals_b: u8,
    amount_in: U256,
}

impl CurveDex {
    pub async fn new(client: Arc<AppMiddleware>, pool_addr: Address, config: Arc<Config>) -> Self {
        let token_cache = TokenCache::global(config.clone()).await;

        // I2: a ordem é descoberta no contrato, não presumida. Fallback estático
        // só preserva operação se o Multicall/RPC falhar no boot.
        let pool_tokens = discover_pool_tokens(client.clone(), pool_addr)
            .await
            .unwrap_or_else(|| {
                warn!("[{}] coins(i) indisponível; usando ordem Curve conhecida", DEX_NAME);
                default_stable_pool_tokens()
            });

        info!("✅ {}Dex inicializado | pool={} | 3 stables (0.04% fee)", DEX_NAME, pool_addr);
        Self {
            client,
            pool_address: pool_addr,
            config,
            token_cache,
            pool_tokens,
        }
    }

    fn load_abi(abi_str: &str) -> Result<Abi> {
        serde_json::from_str(abi_str).map_err(|e| anyhow!("ABI parse error: {}", e))
    }

    fn pool_index(&self, symbol: &str) -> Option<usize> {
        stable_pool_index(&self.pool_tokens, symbol)
    }

    fn quote_notional_usd(&self) -> f64 {
        self.config.executable_trade_notional_usd()
    }

    async fn resolve_am_to_symbol(&self, addr: &Address) -> Option<String> {
        // 1) Match direto contra os amTokens do pool.
        if let Some((_, _, sym)) = self
            .pool_tokens
            .iter()
            .find(|(am_addr, _, _)| am_addr == addr)
        {
            return Some(sym.clone());
        }
        // 2) AUDIT fix: fallback p/ endereço RAW do stable (path get_price recebe
        //    endereços raw do token_cache, não amTokens).
        stable_symbol_for_address(addr)
    }

    fn symbol_from_pair(&self, pair: &str) -> Option<String> {
        // "USDC-USDT" → Some("USDC"), etc.
        let sym = pair.split('-').next()?;
        let normalized = match sym.to_uppercase().as_str() {
            "USDC" | "USDT" | "DAI" => sym.to_uppercase(),
            "MATIC" | "WMATIC" => return None, //不在 Curve pool
            _ => return None,
        };
        Some(normalized)
    }
}

#[async_trait]
impl DexContract for CurveDex {
    fn name(&self) -> String {
        DEX_NAME.to_string()
    }

    async fn get_price(&self, token_a: &Address, token_b: &Address) -> Result<Option<f64>> {
        let sym_a = self.resolve_am_to_symbol(token_a).await;
        let sym_b = self.resolve_am_to_symbol(token_b).await;

        let (Some(idx_a), Some(idx_b)) = (
            sym_a.as_deref().and_then(|s| self.pool_index(s)),
            sym_b.as_deref().and_then(|s| self.pool_index(s)),
        ) else {
            return Ok(None);
        };
        if idx_a == idx_b {
            return Ok(None);
        }

        let abi = Self::load_abi(CURVE_POOL_ABI)?;
        let pool = Contract::new(self.pool_address, abi, self.client.clone());

        let decimals_a = self.pool_tokens[idx_a].1;
        let decimals_b = self.pool_tokens[idx_b].1;

        let amount_in = match quote_amount_for_usd(
            sym_a.as_deref().unwrap_or("USDC"),
            decimals_a,
            self.quote_notional_usd(),
        )
        .await
        {
            Ok(amount) => amount,
            Err(e) => {
                debug!("[{}] notional indisponível: {}", DEX_NAME, e);
                return Ok(None);
            }
        };

        ALCHEMY_RATE_LIMITER.acquire().await?;

        let dy: U256 = pool
            .method("get_dy", (idx_a as i128, idx_b as i128, amount_in))?
            .call()
            .await
            .map_err(|e| anyhow!("Curve get_dy failed: {}", e))?;

        if dy.is_zero() {
            return Ok(None);
        }

        let price = calculate_price_from_decimals(amount_in, dy, decimals_a, decimals_b)?;
        Ok(normalize_price(price))
    }

    async fn get_prices_multicall(
        &self,
        pairs: &[(String, String)],
        quote_block: Option<U64>,
    ) -> Result<Vec<TokenPairPrice>> {
        let abi = Self::load_abi(CURVE_POOL_ABI)?;
        let pool = Contract::new(self.pool_address, abi, self.client.clone());

        let mut calls = Vec::new();

        for (token_a, token_b) in pairs {
            // Só processa pares de stables
            let (Some(idx_a), Some(idx_b)) = (
                self.pool_index(token_a),
                self.pool_index(token_b),
            ) else {
                // AUDIT 2026-07-25: par fora do pool stable (ex.: DAI-WETH). Curve
                // não serve este par — `–` é honesto. Log em debug p/ não spammar
                // (o resumo de exclusão da DEX é barulhento no radar).
                debug!(
                    "[{}] sem pool p/ {}-{} (não é stable-stable) — pulando",
                    DEX_NAME, token_a, token_b
                );
                continue;
            };
            if idx_a == idx_b {
                continue;
            }

            let decimals_a = self.pool_tokens[idx_a].1;
            let decimals_b = self.pool_tokens[idx_b].1;

            let amount_in = match quote_amount_for_usd(token_a, decimals_a, self.quote_notional_usd()).await {
                Ok(amount) => amount,
                Err(e) => {
                    debug!("[{}] pulando {}-{}: notional indisponível: {}", DEX_NAME, token_a, token_b, e);
                    continue;
                }
            };

            calls.push(CurveQuoteCall {
                token_a: token_a.clone(), token_b: token_b.clone(), idx_a, idx_b,
                decimals_a, decimals_b, amount_in,
            });
        }

        if calls.is_empty() { return Ok(Vec::new()); }
        let mut multicall = Multicall::new(self.client.clone(), None).await?;
        if let Some(block) = quote_block {
            multicall = multicall.block(block);
        }
        for info in &calls {
            let call = pool.method::<_, U256>("get_dy", (info.idx_a as i128, info.idx_b as i128, info.amount_in))?;
            // I1: uma falha de par não invalida todo lote Curve.
            multicall.add_call(call, true);
        }
        ALCHEMY_RATE_LIMITER.acquire().await?;
        let raw: Vec<Result<Token, _>> = multicall.call_raw().await?;
        let mut results = Vec::new();
        for (info, result) in calls.iter().zip(raw) {
            let Ok(Token::Uint(dy)) = result else {
                debug!("[{}] get_dy({}→{}) falhou no multicall", DEX_NAME, info.token_a, info.token_b);
                continue;
            };
            if dy.is_zero() { continue; }
            if let Ok(price) = calculate_price_from_decimals(info.amount_in, dy, info.decimals_a, info.decimals_b) {
                if let Some(normalized) = normalize_price(price) {
                    results.push(TokenPairPrice::new(info.token_a.clone(), info.token_b.clone(), normalized, DEX_NAME.into()).with_fee_tier(4));
                }
            }
        }

        Ok(results)
    }

    async fn swap(&self, _token_in: Address, _token_out: Address, _amount_in: U256) -> Result<U256> {
        Err(anyhow!("Curve swap not implemented (read-only adapter)"))
    }

    async fn get_pair_or_pool_address(
        &self,
        token_a: Address,
        token_b: Address,
    ) -> Result<Option<Address>> {
        let sym_a = self.resolve_am_to_symbol(&token_a).await;
        let sym_b = self.resolve_am_to_symbol(&token_b).await;

        if sym_a.is_some() && sym_b.is_some() {
            Ok(Some(self.pool_address))
        } else {
            Ok(None)
        }
    }

    /// Gate de liquidez: am3CRV custodia **amTokens** (amDAI/amUSDC/amUSDT), não os
    /// stables raw. Antes (commit 3d8f161) o gate reportava pool=None (fail-open)
    /// porque o gate genérico media `balanceOf(raw_token, pool) ≈ 0` → TVL 0 →
    /// preço descartado. Agora o hook `liquidity_token_addresses` retorna os
    /// amTokens, então o gate mede a custódia real e podemos reportar a pool
    /// de verdade. A pool é am3CRV (CURVE_AAVE_POOL), known-liquid ~$2.3M.
    /// (Audit A12)
    async fn get_pool_address_for_liquidity(
        &self,
        _token_a: Address,
        _token_b: Address,
        _fee_hint: u32,
    ) -> Result<Option<Address>> {
        Ok(curve_liquidity_pool_address())
    }

    /// A12: a pool am3CRV custodia **amTokens**, não os stables raw. O gate
    /// genérico mede `balanceOf(token_do_par, pool)` — com token raw isso dá ~0
    /// → TVL 0 → preço Curve descartado mesmo a pool sendo líquida. Aqui
    /// retornamos os amTokens correspondentes aos tokens do par, para o gate
    /// medir a custódia real. Decimais e preço do amToken == do raw
    /// (amUSDC 6dec $1 == USDC 6dec $1), então só o endereço do balanceOf muda.
    /// Se algum token não for stable do pool, devolve None → gate usa raw
    /// (fallback seguro, embora Curve só liste stables).
    async fn liquidity_token_addresses(
        &self,
        token_a: Address,
        token_b: Address,
    ) -> Option<(Address, Address)> {
        // Resolve símbolo (amToken OU raw stable) → acha o amToken do pool.
        // Inlined (2x) em vez de closure p/ poder `.await` (`resolve_am_to_symbol`
        // é async). Decimals do amToken == do raw, então só o endereço muda.
        let am_a = {
            let sym = self.resolve_am_to_symbol(&token_a).await?;
            self.pool_tokens
                .iter()
                .find(|(_, _, s)| s.eq_ignore_ascii_case(&sym))
                .map(|(am, _, _)| *am)?
        };
        let am_b = {
            let sym = self.resolve_am_to_symbol(&token_b).await?;
            self.pool_tokens
                .iter()
                .find(|(_, _, s)| s.eq_ignore_ascii_case(&sym))
                .map(|(am, _, _)| *am)?
        };
        Some((am_a, am_b))
    }

    fn config(&self) -> &Arc<Config> {
        &self.config
    }

    fn client(&self) -> &Arc<AppMiddleware> {
        &self.client
    }
}

// ============================================================
// FUNÇÕES PURAS (testáveis sem RPC/AppMiddleware)
// ============================================================

/// Ordem real do pool am3CRV: amDAI(0), amUSDC(1), amUSDT(2).
fn default_stable_pool_tokens() -> Vec<(Address, u8, String)> {
    vec![
        (Address::from_str(AM_DAI).unwrap(), 18, "DAI".to_string()),
        (Address::from_str(AM_USDC).unwrap(), 6, "USDC.e".to_string()),
        (Address::from_str(AM_USDT).unwrap(), 6, "USDT".to_string()),
    ]
}

/// Descobre `coins(0..2)` via Multicall e mantém somente os amTokens esperados.
/// A ordem retornada pelo contrato vira a única fonte de índices para `get_dy`.
async fn discover_pool_tokens(
    client: Arc<AppMiddleware>,
    pool_address: Address,
) -> Option<Vec<(Address, u8, String)>> {
    let abi = CurveDex::load_abi(CURVE_POOL_ABI).ok()?;
    let pool = Contract::new(pool_address, abi, client.clone());
    let mut multicall = Multicall::new(client, None).await.ok()?;
    for index in 0u64..3 {
        multicall.add_call(pool.method::<_, Address>("coins", index).ok()?, false);
    }
    let raw: Vec<Result<Token, _>> = multicall.call_raw().await.ok()?;
    let mut tokens = Vec::with_capacity(3);
    for result in raw {
        let Ok(Token::Address(address)) = result else { return None; };
        let (decimals, symbol) = stable_metadata_for_am_token(address)?;
        tokens.push((address, decimals, symbol.to_string()));
    }
    (tokens.len() == 3).then_some(tokens)
}

fn stable_metadata_for_am_token(address: Address) -> Option<(u8, &'static str)> {
    let address = format!("{address:#x}").to_ascii_lowercase();
    match address.as_str() {
        a if a == AM_DAI.to_ascii_lowercase() => Some((18, "DAI")),
        a if a == AM_USDC.to_ascii_lowercase() => Some((6, "USDC.e")),
        a if a == AM_USDT.to_ascii_lowercase() => Some((6, "USDT")),
        _ => None,
    }
}

/// Índice de um símbolo no pool (None → não suportado → `–` honesto).
fn stable_pool_index(pool_tokens: &[(Address, u8, String)], symbol: &str) -> Option<usize> {
    pool_tokens
        .iter()
        .position(|(_, _, orig)| orig.eq_ignore_ascii_case(symbol))
}

/// Resolve um endereço (amToken OU raw stable) p/ símbolo. Usado pelo path
/// `get_price` (fallback) que recebe endereços raw do token_cache.
fn stable_symbol_for_address(addr: &Address) -> Option<String> {
    for (raw, sym) in RAW_STABLE_TO_SYMBOL {
        if format!("{:?}", addr).to_lowercase() == raw.to_lowercase() {
            return Some((*sym).to_string());
        }
    }
    None
}

/// Endereço de pool a reportar ao gate de liquidez. Após A12 (hook
/// `liquidity_token_addresses` retorna amTokens), o gate mede a custódia real
/// → reportamos a pool de verdade em vez de fail-open. Mantido como helper
/// p/ o teste de regressão `curve_liquidity_gate_reports_pool`.
fn curve_liquidity_pool_address() -> Option<Address> {
    Address::from_str(CURVE_AAVE_POOL).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_tokens() -> Vec<(Address, u8, String)> {
        default_stable_pool_tokens()
    }

    #[test]
    fn stable_pool_index_finds_three_stables() {
        let pt = pool_tokens();
        assert_eq!(stable_pool_index(&pt, "DAI"), Some(0));
        assert_eq!(stable_pool_index(&pt, "USDC.e"), Some(1));
        assert_eq!(stable_pool_index(&pt, "USDC"), None, "USDC nativo não é amUSDC");
        assert_eq!(stable_pool_index(&pt, "USDT"), Some(2));
        assert_eq!(stable_pool_index(&pt, "dai"), Some(0)); // case-insensitive
    }

    #[test]
    fn discovered_am_tokens_keep_symbol_and_decimals() {
        let usdc = Address::from_str(AM_USDC).unwrap();
        assert_eq!(stable_metadata_for_am_token(usdc), Some((6, "USDC.e")));
        assert_eq!(stable_metadata_for_am_token(Address::zero()), None);
    }

    /// Q2: Curve é stable-only. Pares não-stable (WETH, WMATIC, WBTC, LINK…)
    /// não têm pool → `None` → `–` honesto. Nada de inventar preço.
    #[test]
    fn stable_pool_index_rejects_non_stables() {
        let pt = pool_tokens();
        for sym in ["WETH", "WMATIC", "WBTC", "LINK", "UNI", "LDO", "AAVE"] {
            assert_eq!(stable_pool_index(&pt, sym), None, "{sym} não deveria ter pool");
        }
    }

    /// O path get_price (fallback) recebe endereços RAW (USDC.e 0x2791…,
    /// USDT 0xc213…, DAI 0x8f3C…) — não os amTokens. Sem o mapa raw, o fallback
    /// Curve sempre retornava None. Agora resolve.
    #[test]
    fn raw_stable_address_resolves_to_symbol() {
        let usdc = Address::from_str("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174").unwrap();
        let usdt = Address::from_str("0xc2132D05D31c914a87C6611C10748AEb04B58e8F").unwrap();
        let dai = Address::from_str("0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063").unwrap();
        let weth = Address::from_str("0x7ceB23fD6bC0adD59E62ac25578270cFf1b9f619").unwrap();

        assert_eq!(stable_symbol_for_address(&usdc).as_deref(), Some("USDC.e"));
        assert_eq!(stable_symbol_for_address(&usdt).as_deref(), Some("USDT"));
        assert_eq!(stable_symbol_for_address(&dai).as_deref(), Some("DAI"));
        assert_eq!(stable_symbol_for_address(&weth), None); // não-stable → None
    }

    /// Guarda de regressão A12: o gate de liquidez agora mede `balanceOf` dos
    /// amTokens (hook `liquidity_token_addresses`), não mais dos stables raw.
    /// Logo Curve reporta a pool real (CURVE_AAVE_POOL) ao gate, em vez de
    /// fail-open None. Pool é known-liquid (~$2.3M, amUSDC.balanceOf(pool)≈838k).
    #[test]
    fn curve_liquidity_gate_reports_pool() {
        assert_eq!(
            curve_liquidity_pool_address(),
            Some(Address::from_str(CURVE_AAVE_POOL).unwrap())
        );
    }
}
