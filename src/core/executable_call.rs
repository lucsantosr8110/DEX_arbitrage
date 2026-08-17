//! Typed, fail-closed calldata builders (E1-B). No RPC or transaction IO.

use crate::core::route_artifact::RouteLeg;
use ethers::{
    abi::{encode, Token},
    types::{Address, Bytes, U256},
};
use std::{collections::HashMap, str::FromStr, sync::Arc};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Venue {
    UniswapV3,
    QuickSwap,
    SushiSwap,
    Curve,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequirement {
    pub token: Address,
    pub spender: Address,
    pub amount: U256,
    pub reset_to_zero_first: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutableCallValidation {
    pub canonical_signature: String,
    pub expected_selector: [u8; 4],
    pub selector_verified: bool,
    pub token_order_verified: bool,
    pub amount_verified: bool,
    pub recipient_verified: bool,
    pub deadline_verified: bool,
    pub venue_specific_fields_verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutableCall {
    pub venue: Venue,
    pub target: Address,
    pub value: U256,
    pub calldata: Bytes,
    pub selector: [u8; 4],
    pub method: String,
    pub approvals: Vec<ApprovalRequirement>,
    pub validation: ExecutableCallValidation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionCallContext {
    pub recipient: Address,
    pub deadline: U256,
    pub amount_out_min: U256,
    pub default_sqrt_price_limit_x96: U256,
    pub router: Option<Address>,
    pub curve_method: Option<String>,
    pub curve_indices: Option<(i128, i128)>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ExecutableCallError {
    #[error("UNSUPPORTED_VENUE: {0:?}")]
    UnsupportedVenue(Venue),
    #[error("MISSING_ROUTER")]
    MissingRouter,
    #[error("MISSING_POOL_ADDRESS")]
    MissingPoolAddress,
    #[error("MISSING_TOKEN_ADDRESS: {0}")]
    MissingTokenAddress(String),
    #[error("INVALID_TOKEN_ORDER")]
    InvalidTokenOrder,
    #[error("INVALID_AMOUNT")]
    InvalidAmount,
    #[error("INVALID_RECIPIENT")]
    InvalidRecipient,
    #[error("INVALID_DEADLINE")]
    InvalidDeadline,
    #[error("INVALID_FEE_TIER")]
    InvalidFeeTier,
    #[error("INVALID_CURVE_INDICES")]
    InvalidCurveIndices,
    #[error("UNSUPPORTED_POOL: {0}")]
    UnsupportedPool(String),
    #[error("INVALID_SELECTOR")]
    InvalidSelector,
    #[error("ENCODING_FAILED: {0}")]
    EncodingFailed(String),
}

pub trait ExecutableLegBuilder: Send + Sync {
    fn venue(&self) -> Venue;
    fn build_call(
        &self,
        leg: &RouteLeg,
        amount_in: U256,
        context: &ExecutionCallContext,
    ) -> Result<ExecutableCall, ExecutableCallError>;
}

fn address(value: &str) -> Result<Address, ExecutableCallError> {
    Address::from_str(value).map_err(|_| ExecutableCallError::MissingTokenAddress(value.into()))
}
fn selector(signature: &str) -> [u8; 4] {
    ethers::utils::keccak256(signature.as_bytes())[..4]
        .try_into()
        .unwrap_or([0; 4])
}
fn prefix(mut data: Vec<u8>, expected: [u8; 4]) -> Result<Bytes, ExecutableCallError> {
    if data.len() < 4 || data[..4] != expected {
        return Err(ExecutableCallError::InvalidSelector);
    }
    Ok(Bytes::from(std::mem::take(&mut data)))
}
fn base_validation(
    signature: &str,
    selector: [u8; 4],
    token_ok: bool,
    amount_ok: bool,
    recipient_ok: bool,
    deadline_ok: bool,
    venue_ok: bool,
) -> ExecutableCallValidation {
    ExecutableCallValidation {
        canonical_signature: signature.into(),
        expected_selector: selector,
        selector_verified: true,
        token_order_verified: token_ok,
        amount_verified: amount_ok,
        recipient_verified: recipient_ok,
        deadline_verified: deadline_ok,
        venue_specific_fields_verified: venue_ok,
    }
}
fn common(
    leg: &RouteLeg,
    amount: U256,
    c: &ExecutionCallContext,
) -> Result<(Address, Address, Address), ExecutableCallError> {
    if amount.is_zero() {
        return Err(ExecutableCallError::InvalidAmount);
    }
    if c.recipient.is_zero() {
        return Err(ExecutableCallError::InvalidRecipient);
    }
    if c.deadline.is_zero() {
        return Err(ExecutableCallError::InvalidDeadline);
    }
    let a = address(&leg.token_in)?;
    let b = address(&leg.token_out)?;
    if a == b {
        return Err(ExecutableCallError::InvalidTokenOrder);
    }
    let target = c.router.ok_or(ExecutableCallError::MissingRouter)?;
    if target.is_zero() {
        return Err(ExecutableCallError::MissingRouter);
    }
    Ok((a, b, target))
}

#[derive(Debug, Clone, Copy)]
pub struct UniswapV3Builder;
impl ExecutableLegBuilder for UniswapV3Builder {
    fn venue(&self) -> Venue {
        Venue::UniswapV3
    }
    fn build_call(
        &self,
        leg: &RouteLeg,
        amount: U256,
        c: &ExecutionCallContext,
    ) -> Result<ExecutableCall, ExecutableCallError> {
        let (a, b, target) = common(leg, amount, c)?;
        let fee = leg.fee_tier.ok_or(ExecutableCallError::InvalidFeeTier)?;
        if fee > 999_999 {
            return Err(ExecutableCallError::InvalidFeeTier);
        }
        if c.default_sqrt_price_limit_x96 > U256::from(2).pow(U256::from(160)) {
            return Err(ExecutableCallError::InvalidFeeTier);
        }
        let sig =
            "exactInputSingle((address,address,uint24,address,uint256,uint256,uint256,uint160))";
        let sel = selector(sig);
        let tuple = Token::Tuple(vec![
            Token::Address(a),
            Token::Address(b),
            Token::Uint(U256::from(fee)),
            Token::Address(c.recipient),
            Token::Uint(c.deadline),
            Token::Uint(amount),
            Token::Uint(c.amount_out_min),
            Token::Uint(c.default_sqrt_price_limit_x96),
        ]);
        let mut data = sel.to_vec();
        data.extend(encode(&[tuple]));
        Ok(ExecutableCall {
            venue: Venue::UniswapV3,
            target,
            value: U256::zero(),
            calldata: prefix(data, sel)?,
            selector: sel,
            method: "exactInputSingle".into(),
            approvals: vec![ApprovalRequirement {
                token: a,
                spender: target,
                amount,
                reset_to_zero_first: false,
            }],
            validation: base_validation(sig, sel, true, true, true, true, true),
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct V2Builder {
    pub venue: Venue,
}
impl ExecutableLegBuilder for V2Builder {
    fn venue(&self) -> Venue {
        self.venue
    }
    fn build_call(
        &self,
        leg: &RouteLeg,
        amount: U256,
        c: &ExecutionCallContext,
    ) -> Result<ExecutableCall, ExecutableCallError> {
        let (a, b, target) = common(leg, amount, c)?;
        let sig = "swapExactTokensForTokens(uint256,uint256,address[],address,uint256)";
        let sel = selector(sig);
        let mut data = sel.to_vec();
        data.extend(encode(&[
            Token::Uint(amount),
            Token::Uint(c.amount_out_min),
            Token::Array(vec![Token::Address(a), Token::Address(b)]),
            Token::Address(c.recipient),
            Token::Uint(c.deadline),
        ]));
        Ok(ExecutableCall {
            venue: self.venue,
            target,
            value: U256::zero(),
            calldata: prefix(data, sel)?,
            selector: sel,
            method: "swapExactTokensForTokens".into(),
            approvals: vec![ApprovalRequirement {
                token: a,
                spender: target,
                amount,
                reset_to_zero_first: false,
            }],
            validation: base_validation(sig, sel, true, true, true, true, true),
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CurveBuilder;
impl ExecutableLegBuilder for CurveBuilder {
    fn venue(&self) -> Venue {
        Venue::Curve
    }
    fn build_call(
        &self,
        leg: &RouteLeg,
        amount: U256,
        c: &ExecutionCallContext,
    ) -> Result<ExecutableCall, ExecutableCallError> {
        let (a, _, target) = common(
            leg,
            amount,
            &ExecutionCallContext {
                router: Some(
                    leg.pool_address
                        .as_deref()
                        .ok_or(ExecutableCallError::MissingPoolAddress)?
                        .parse()
                        .map_err(|_| ExecutableCallError::MissingPoolAddress)?,
                ),
                ..c.clone()
            },
        )?;
        let (i, j) = c
            .curve_indices
            .ok_or(ExecutableCallError::InvalidCurveIndices)?;
        if i < 0 || j < 0 || i == j {
            return Err(ExecutableCallError::InvalidCurveIndices);
        }
        let method = c.curve_method.as_deref().ok_or_else(|| {
            ExecutableCallError::UnsupportedPool("method metadata missing".into())
        })?;
        let (sig, name) = match method {
            "exchange" => ("exchange(int128,int128,uint256,uint256)", "exchange"),
            "exchange_underlying" => (
                "exchange_underlying(int128,int128,uint256,uint256)",
                "exchange_underlying",
            ),
            other => return Err(ExecutableCallError::UnsupportedPool(other.into())),
        };
        let sel = selector(sig);
        let mut data = sel.to_vec();
        data.extend(encode(&[
            Token::Int(i.into()),
            Token::Int(j.into()),
            Token::Uint(amount),
            Token::Uint(c.amount_out_min),
        ]));
        Ok(ExecutableCall {
            venue: Venue::Curve,
            target,
            value: U256::zero(),
            calldata: prefix(data, sel)?,
            selector: sel,
            method: name.into(),
            approvals: vec![ApprovalRequirement {
                token: a,
                spender: target,
                amount,
                reset_to_zero_first: false,
            }],
            validation: base_validation(sig, sel, true, true, true, true, true),
        })
    }
}

#[derive(Default)]
pub struct ExecutableCallBuilderRegistry {
    builders: HashMap<Venue, Arc<dyn ExecutableLegBuilder>>,
}
impl ExecutableCallBuilderRegistry {
    pub fn standard() -> Self {
        let mut r = Self::default();
        r.builders
            .insert(Venue::UniswapV3, Arc::new(UniswapV3Builder));
        r.builders.insert(
            Venue::QuickSwap,
            Arc::new(V2Builder {
                venue: Venue::QuickSwap,
            }),
        );
        r.builders.insert(
            Venue::SushiSwap,
            Arc::new(V2Builder {
                venue: Venue::SushiSwap,
            }),
        );
        r.builders.insert(Venue::Curve, Arc::new(CurveBuilder));
        r
    }
    pub fn build(
        &self,
        leg: &RouteLeg,
        amount: U256,
        c: &ExecutionCallContext,
    ) -> Result<ExecutableCall, ExecutableCallError> {
        let venue = match leg.venue.as_str() {
            "UniswapV3" => Venue::UniswapV3,
            "QuickSwap" => Venue::QuickSwap,
            "SushiSwap" => Venue::SushiSwap,
            "Curve" => Venue::Curve,
            other => return Err(ExecutableCallError::UnsupportedPool(other.into())),
        };
        self.builders
            .get(&venue)
            .ok_or(ExecutableCallError::UnsupportedVenue(venue))?
            .build_call(leg, amount, c)
    }
}
