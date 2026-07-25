// ================================================================
// src/dex/adapters/mod.rs — v3.7 (Curve + Polygon)
// ================================================================

pub mod curve;
pub mod quickswap;
pub mod sushiswap;
pub mod uniswap_v2;
pub mod uniswap_v3;

// Reexporta para o DexManager e Radar
pub use curve::CurveDex;
pub use quickswap::QuickSwapDex;
pub use sushiswap::SushiSwapDex;
pub use uniswap_v2::UniswapV2Dex;
pub use uniswap_v3::UniswapV3Dex;
