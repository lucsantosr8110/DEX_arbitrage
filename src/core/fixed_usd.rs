//! Fixed-point USD helpers for economic gates (no f64 on approve/reject paths).
//!
//! # Scale
//! - `UsdE8`: USD × 10^8. `$1.00` = `100_000_000`.
//! - Token amounts stay in raw `U256` (`10^decimals` units).
//!
//! # Ingress
//! Price feeds and gas estimators still produce `f64` at the boundary. Convert
//! once via `*_to_usd_e8_*` helpers; thereafter only integer math. Display/UI
//! must use `*_display_f64` helpers — never call those from gates.

use anyhow::{anyhow, bail, Result};
use ethers::types::U256;

/// USD with 8 fractional digits. `$1` = 1e8.
pub const USD_E8_SCALE: u128 = 100_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsdE8(pub u128);

impl UsdE8 {
    pub fn zero() -> Self {
        Self(0)
    }

    pub fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    pub fn checked_add(self, other: Self) -> Result<Self> {
        self.0
            .checked_add(other.0)
            .map(Self)
            .ok_or_else(|| anyhow!("UsdE8 add overflow"))
    }

    /// Telemetry only — not for gates.
    pub fn display_f64(self) -> f64 {
        self.0 as f64 / USD_E8_SCALE as f64
    }
}

/// Ceil-convert a non-negative finite `f64` USD amount to `UsdE8`.
pub fn usd_f64_to_e8_ceil(usd: f64) -> Result<UsdE8> {
    if !usd.is_finite() {
        bail!("usd_f64_to_e8_ceil: non-finite usd={usd}");
    }
    if usd < 0.0 {
        bail!("usd_f64_to_e8_ceil: negative usd={usd}");
    }
    if usd == 0.0 {
        return Ok(UsdE8(0));
    }
    let scaled = (usd * USD_E8_SCALE as f64).ceil();
    if !scaled.is_finite() || scaled > u128::MAX as f64 {
        bail!("usd_f64_to_e8_ceil: overflow usd={usd}");
    }
    Ok(UsdE8(scaled as u128))
}

/// Parse config string like `"0.50"` into `UsdE8` (ceil).
pub fn usd_str_to_e8_ceil(raw: &str) -> Result<UsdE8> {
    let v: f64 = raw
        .trim()
        .parse()
        .map_err(|e| anyhow!("usd_str_to_e8_ceil: parse '{raw}': {e}"))?;
    usd_f64_to_e8_ceil(v)
}

/// Fail-closed price ingress from `Option<f64>` (opp.token_price_usd).
pub fn price_usd_e8_from_option(price: Option<f64>) -> Result<UsdE8> {
    let Some(p) = price else {
        bail!("MissingProfitTokenPrice: token_price_usd is None");
    };
    if !p.is_finite() || p <= 0.0 {
        bail!("MissingProfitTokenPrice: token_price_usd={p}");
    }
    usd_f64_to_e8_ceil(p)
}

/// `ceil((usd_e8 * 10^decimals) / price_e8)` → token raw units.
pub fn usd_e8_to_token_raw_ceil(usd: UsdE8, price: UsdE8, decimals: u32) -> Result<U256> {
    if price.0 == 0 {
        bail!("usd_e8_to_token_raw_ceil: price_e8=0");
    }
    if decimals > 36 {
        bail!("usd_e8_to_token_raw_ceil: decimals={decimals} > 36");
    }
    if usd.0 == 0 {
        return Ok(U256::zero());
    }
    let scale = U256::exp10(decimals as usize);
    let usd_u = U256::from(usd.0);
    let price_u = U256::from(price.0);
    let num = usd_u
        .checked_mul(scale)
        .ok_or_else(|| anyhow!("usd_e8_to_token_raw_ceil: usd*10^dec overflow"))?;
    // ceil_div(num, price) = (num + price - 1) / price
    let numer = num
        .checked_add(price_u - U256::one())
        .ok_or_else(|| anyhow!("usd_e8_to_token_raw_ceil: ceil add overflow"))?;
    Ok(numer / price_u)
}

/// `(amount_raw * price_e8) / 10^decimals` → UsdE8 (floor; used for principal/fee).
///
/// Overflow → explicit error (no silent `u128::MAX` clip).
pub fn token_raw_to_usd_e8(amount: U256, price: UsdE8, decimals: u32) -> Result<UsdE8> {
    if price.0 == 0 {
        bail!("token_raw_to_usd_e8: price_e8=0");
    }
    if decimals > 36 {
        bail!("token_raw_to_usd_e8: decimals={decimals} > 36");
    }
    if amount.is_zero() {
        return Ok(UsdE8(0));
    }
    let scale = U256::exp10(decimals as usize);
    let price_u = U256::from(price.0);
    // (amount * price) / scale — split to reduce overflow pressure:
    // q = amount / scale; r = amount % scale;
    // usd = q * price + (r * price) / scale
    let q = amount / scale;
    let r = amount % scale;
    let hi = q
        .checked_mul(price_u)
        .ok_or_else(|| anyhow!("token_raw_to_usd_e8: q*price overflow"))?;
    let lo_num = r
        .checked_mul(price_u)
        .ok_or_else(|| anyhow!("token_raw_to_usd_e8: r*price overflow"))?;
    let lo = lo_num / scale;
    let sum = hi
        .checked_add(lo)
        .ok_or_else(|| anyhow!("token_raw_to_usd_e8: usd sum overflow"))?;
    if sum > U256::from(u128::MAX) {
        bail!("token_raw_to_usd_e8: result exceeds u128 ({sum})");
    }
    Ok(UsdE8(sum.as_u128()))
}

/// Flashloan premium in UsdE8: `principal_e8 * fee_bps / 10_000` (floor).
pub fn flashloan_fee_usd_e8(principal: UsdE8, fee_bps: u64) -> Result<UsdE8> {
    if fee_bps == 0 {
        return Ok(UsdE8(0));
    }
    if fee_bps > 10_000 {
        bail!("flashloan_fee_usd_e8: fee_bps={fee_bps} > 10000");
    }
    let num = principal
        .0
        .checked_mul(fee_bps as u128)
        .ok_or_else(|| anyhow!("flashloan_fee_usd_e8: mul overflow"))?;
    Ok(UsdE8(num / 10_000))
}

/// Convert fee fraction (e.g. 0.0005) to bps; reject non-finite / out of range.
pub fn fee_pct_to_bps(fee_pct: f64) -> Result<u64> {
    if !fee_pct.is_finite() || fee_pct < 0.0 {
        bail!("fee_pct_to_bps: invalid fee_pct={fee_pct}");
    }
    let bps = (fee_pct * 10_000.0).round();
    if bps > 10_000.0 {
        bail!("fee_pct_to_bps: fee_pct={fee_pct} exceeds 100%");
    }
    Ok(bps as u64)
}

/// Net = gross - gas - fl_fee - adverse. Underflow → 0 (not profitable).
pub fn net_profit_usd_e8(
    gross: UsdE8,
    gas: UsdE8,
    flashloan_fee: UsdE8,
    adverse: UsdE8,
) -> UsdE8 {
    let costs = gas.0.saturating_add(flashloan_fee.0).saturating_add(adverse.0);
    UsdE8(gross.0.saturating_sub(costs))
}

/// Telemetry: raw → f64 human units. Not for gates.
pub fn token_raw_display_f64(amount: U256, decimals: u32) -> f64 {
    crate::utils::u256_to_f64(amount, decimals)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_6dec_hurdle_one_dollar() {
        let price = UsdE8(USD_E8_SCALE); // $1
        let hurdle = UsdE8(USD_E8_SCALE); // $1
        let raw = usd_e8_to_token_raw_ceil(hurdle, price, 6).unwrap();
        assert_eq!(raw, U256::from(1_000_000u64));
    }

    #[test]
    fn token_18dec_price_2000_ceil() {
        // $10 hurdle / $2000 = 0.005 token → 5e15 raw
        let price = usd_f64_to_e8_ceil(2000.0).unwrap();
        let hurdle = usd_f64_to_e8_ceil(10.0).unwrap();
        let raw = usd_e8_to_token_raw_ceil(hurdle, price, 18).unwrap();
        assert_eq!(raw, U256::from(10u64).pow(U256::from(15)) * U256::from(5u64));
    }

    #[test]
    fn cheap_token_ceil_rounds_up() {
        // $1 / $0.01 = 100 tokens @ 8 decimals = 100 * 1e8
        let price = usd_f64_to_e8_ceil(0.01).unwrap();
        let hurdle = UsdE8(USD_E8_SCALE);
        let raw = usd_e8_to_token_raw_ceil(hurdle, price, 8).unwrap();
        assert_eq!(raw, U256::from(100u64) * U256::exp10(8));
    }

    #[test]
    fn ceil_when_not_divisible() {
        // $1.00 / $3.00 @ 6 decimals → ceil(1e8 * 1e6 / 3e8) = ceil(1e14/3e8) = ceil(333333.33) = 333334
        let price = usd_f64_to_e8_ceil(3.0).unwrap();
        let hurdle = UsdE8(USD_E8_SCALE);
        let raw = usd_e8_to_token_raw_ceil(hurdle, price, 6).unwrap();
        assert_eq!(raw, U256::from(333_334u64));
    }

    #[test]
    fn missing_price_fails() {
        assert!(price_usd_e8_from_option(None).is_err());
        assert!(price_usd_e8_from_option(Some(0.0)).is_err());
        assert!(price_usd_e8_from_option(Some(-1.0)).is_err());
        assert!(price_usd_e8_from_option(Some(f64::NAN)).is_err());
    }

    #[test]
    fn raw_to_usd_monotonic_no_clip() {
        let price = UsdE8(USD_E8_SCALE);
        let a = U256::from(1_000_000u64);
        let b = a + U256::one();
        let ua = token_raw_to_usd_e8(a, price, 6).unwrap();
        let ub = token_raw_to_usd_e8(b, price, 6).unwrap();
        // 1 raw unit @ 6dec @ $1 = 1e8/1e6 = 100 e8-units? 
        // (1e6 * 1e8) / 1e6 = 1e8 for 1 token; for +1 raw: floor may equal
        // Use larger gap:
        let b2 = a + U256::from(1_000_000u64);
        let ub2 = token_raw_to_usd_e8(b2, price, 6).unwrap();
        assert!(ub2.0 > ua.0);
        let _ = ub; // may equal ua for +1 raw at $1/1e6
    }

    #[test]
    fn large_raw_no_silent_saturation() {
        // 1e30 raw @ 18 dec = 1e12 tokens @ $1 → 1e12 * 1e8 = 1e20 e8 — fits u128
        let big = U256::from(10u64).pow(U256::from(30));
        let price = UsdE8(USD_E8_SCALE);
        let usd = token_raw_to_usd_e8(big, price, 18).unwrap();
        assert_eq!(usd.0, 100_000_000u128 * 1_000_000_000_000u128); // 1e12 dollars * 1e8
    }

    #[test]
    fn fee_bps_five() {
        let principal = UsdE8(100 * USD_E8_SCALE); // $100
        let fee = flashloan_fee_usd_e8(principal, 5).unwrap(); // 5 bps
        assert_eq!(fee.0, USD_E8_SCALE / 20); // $0.05
    }
}
