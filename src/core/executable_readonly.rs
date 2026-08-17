//! E1-C executable read-only verification against a loopback Anvil fork.
//! No swap transaction is sent by this module.

use crate::core::{
    executable_call::ExecutableCall, fork_execution_domain::validate_loopback_endpoint,
};
use async_trait::async_trait;
use ethers::{
    abi::decode,
    providers::{Http, Middleware, Provider},
    types::{Address, BlockId, BlockNumber, Bytes, TransactionRequest, H256, U256},
};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct ExecutableReadOnlyRequest {
    pub route_key: String,
    pub leg_index: usize,
    pub anchor_block: u64,
    pub caller: Address,
    pub call: ExecutableCall,
    pub expected_amount_in: U256,
    pub reset_before_call: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutableReadOnlyStatus {
    Pass,
    Revert,
    InvalidCalldata,
    ContractMissing,
    SetupFailure,
    Unsupported,
    EnvironmentFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutableReadOnlyErrorCode {
    NonLoopbackForkEndpoint,
    InvalidForkChainId,
    AnchorBlockMismatch,
    ContractCodeMissing,
    SelectorMismatch,
    CalldataTooShort,
    EthCallFailed,
    DecodeFailed,
    UnsupportedReturnEncoding,
}

#[derive(Debug, Clone)]
pub struct ExecutableReadOnlyResult {
    pub route_key: String,
    pub leg_index: usize,
    pub anchor_block: u64,
    pub target: Address,
    pub selector: [u8; 4],
    pub contract_code_hash: H256,
    pub contract_code_size: usize,
    pub status: ExecutableReadOnlyStatus,
    pub return_data: Bytes,
    pub return_data_hash: H256,
    pub decoded_amount_out: Option<U256>,
    pub revert_data: Bytes,
    pub revert_data_hash: Option<H256>,
    pub error_code: Option<ExecutableReadOnlyErrorCode>,
    pub setup_writes: u64,
    pub setup_transactions: u64,
}

#[derive(Debug, Error)]
pub enum ExecutableReadOnlyError {
    #[error("NON_LOOPBACK_FORK_ENDPOINT")]
    NonLoopbackForkEndpoint,
    #[error("INVALID_FORK_CHAIN_ID: {0}")]
    InvalidForkChainId(u64),
    #[error("ANCHOR_BLOCK_MISMATCH expected={expected} actual={actual}")]
    AnchorBlockMismatch { expected: u64, actual: u64 },
    #[error("RPC_FAILURE: {0}")]
    RpcFailure(String),
    #[error("CONTRACT_CODE_MISSING")]
    ContractCodeMissing,
    #[error("SELECTOR_MISMATCH")]
    SelectorMismatch,
    #[error("CALLDATA_TOO_SHORT")]
    CalldataTooShort,
    #[error("DECODE_FAILED")]
    DecodeFailed,
    #[error("UNSUPPORTED_RETURN_ENCODING")]
    UnsupportedReturnEncoding,
}

#[async_trait]
pub trait ExecutableReadOnlyVerifier {
    async fn verify(
        &self,
        request: &ExecutableReadOnlyRequest,
    ) -> Result<ExecutableReadOnlyResult, ExecutableReadOnlyError>;
}

pub struct AnvilExecutableReadOnlyVerifier {
    pub endpoint: String,
    pub provider: Arc<Provider<Http>>,
}
impl AnvilExecutableReadOnlyVerifier {
    pub fn new(
        endpoint: String,
        provider: Arc<Provider<Http>>,
    ) -> Result<Self, ExecutableReadOnlyError> {
        validate_loopback_endpoint(&endpoint)
            .map_err(|_| ExecutableReadOnlyError::NonLoopbackForkEndpoint)?;
        Ok(Self { endpoint, provider })
    }
}

#[async_trait]
impl ExecutableReadOnlyVerifier for AnvilExecutableReadOnlyVerifier {
    async fn verify(
        &self,
        request: &ExecutableReadOnlyRequest,
    ) -> Result<ExecutableReadOnlyResult, ExecutableReadOnlyError> {
        validate_loopback_endpoint(&self.endpoint)
            .map_err(|_| ExecutableReadOnlyError::NonLoopbackForkEndpoint)?;
        let chain = self
            .provider
            .get_chainid()
            .await
            .map_err(|e| ExecutableReadOnlyError::RpcFailure(e.to_string()))?
            .as_u64();
        if chain != 137 {
            return Err(ExecutableReadOnlyError::InvalidForkChainId(chain));
        }
        let actual = self
            .provider
            .get_block_number()
            .await
            .map_err(|e| ExecutableReadOnlyError::RpcFailure(e.to_string()))?
            .as_u64();
        if actual != request.anchor_block {
            return Err(ExecutableReadOnlyError::AnchorBlockMismatch {
                expected: request.anchor_block,
                actual,
            });
        }
        let code = self
            .provider
            .get_code(
                request.call.target,
                Some(BlockId::Number(BlockNumber::Number(
                    request.anchor_block.into(),
                ))),
            )
            .await
            .map_err(|e| ExecutableReadOnlyError::RpcFailure(e.to_string()))?;
        if code.0.is_empty() {
            return Err(ExecutableReadOnlyError::ContractCodeMissing);
        }
        if request.call.calldata.len() < 4 {
            return Err(ExecutableReadOnlyError::CalldataTooShort);
        }
        if request.call.calldata[..4] != request.call.selector {
            return Err(ExecutableReadOnlyError::SelectorMismatch);
        }
        let tx = TransactionRequest {
            from: Some(request.caller),
            to: Some(request.call.target.into()),
            data: Some(request.call.calldata.clone()),
            value: Some(request.call.value),
            ..Default::default()
        };
        let result = self
            .provider
            .call(
                &tx.into(),
                Some(BlockId::Number(BlockNumber::Number(
                    request.anchor_block.into(),
                ))),
            )
            .await;
        let hash = H256::from(ethers::utils::keccak256(&code.0));
        match result {
            Ok(data) => {
                let decoded = decode_amount(&request.call.method, &data)?;
                Ok(ExecutableReadOnlyResult {
                    route_key: request.route_key.clone(),
                    leg_index: request.leg_index,
                    anchor_block: request.anchor_block,
                    target: request.call.target,
                    selector: request.call.selector,
                    contract_code_hash: hash,
                    contract_code_size: code.0.len(),
                    status: ExecutableReadOnlyStatus::Pass,
                    return_data_hash: H256::from(ethers::utils::keccak256(&data.0)),
                    return_data: data,
                    decoded_amount_out: decoded,
                    revert_data: Bytes::new(),
                    revert_data_hash: None,
                    error_code: None,
                    setup_writes: 0,
                    setup_transactions: 0,
                })
            }
            Err(_e) => Ok(ExecutableReadOnlyResult {
                route_key: request.route_key.clone(),
                leg_index: request.leg_index,
                anchor_block: request.anchor_block,
                target: request.call.target,
                selector: request.call.selector,
                contract_code_hash: hash,
                contract_code_size: code.0.len(),
                status: ExecutableReadOnlyStatus::Revert,
                return_data: Bytes::new(),
                return_data_hash: H256::zero(),
                decoded_amount_out: None,
                revert_data: Bytes::new(),
                revert_data_hash: None,
                error_code: Some(ExecutableReadOnlyErrorCode::EthCallFailed),
                setup_writes: 0,
                setup_transactions: 0,
            }),
        }
    }
}

fn decode_amount(method: &str, data: &Bytes) -> Result<Option<U256>, ExecutableReadOnlyError> {
    if method == "exactInputSingle" {
        return decode(&[ethers::abi::ParamType::Uint(256)], data)
            .map_err(|_| ExecutableReadOnlyError::DecodeFailed)
            .and_then(|v| {
                v.first()
                    .and_then(|x| x.clone().into_uint())
                    .ok_or(ExecutableReadOnlyError::DecodeFailed)
                    .map(Some)
            });
    }
    if method == "swapExactTokensForTokens" {
        let vals = decode(
            &[ethers::abi::ParamType::Array(Box::new(
                ethers::abi::ParamType::Uint(256),
            ))],
            data,
        )
        .map_err(|_| ExecutableReadOnlyError::DecodeFailed)?;
        return vals
            .first()
            .and_then(|x| x.clone().into_array())
            .and_then(|a| a.last().cloned())
            .and_then(|x| x.into_uint())
            .map(Some)
            .ok_or(ExecutableReadOnlyError::DecodeFailed);
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readonly_rejects_non_loopback_endpoint() {
        let p = Provider::<Http>::try_from("http://127.0.0.1:8545").unwrap();
        assert!(matches!(
            AnvilExecutableReadOnlyVerifier::new("https://polygon-rpc.com".into(), Arc::new(p)),
            Err(ExecutableReadOnlyError::NonLoopbackForkEndpoint)
        ));
    }
}
