// ================================================================
// src/dex/adapters/mod.rs — v3.7 (Curve + Polygon)
// ================================================================

pub mod curve;
pub mod uniswap_v2;
pub mod uniswap_v3;

// Reexporta para o DexManager e Radar
pub use curve::CurveDex;
pub use uniswap_v2::{UniswapV2Dex, V2Dex};
pub use uniswap_v3::UniswapV3Dex;
