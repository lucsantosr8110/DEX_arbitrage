// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @dev Pure ABI decoder matching FlashloanExecutor callback wire layout.
/// CRITICAL: params use flat `abi.encode(address, SwapStep[], uint256)` —
/// NOT `abi.encode(CallbackData)` (struct encode adds an outer offset).
contract CallbackDataDecoder {
    enum DexType { QUICKSWAP, SUSHISWAP, UNISWAP_V3 }

    struct SwapStep {
        DexType dexType;
        address tokenIn;
        address tokenOut;
        uint256 amountOutMin;
        bytes extraData;
    }

    struct CallbackData {
        address profitRecipient;
        SwapStep[] steps;
        uint256 minProfit;
    }

    /// Same decode path as FlashloanExecutor.executeOperation (flat).
    function decode(bytes calldata params) external pure returns (CallbackData memory data) {
        (data.profitRecipient, data.steps, data.minProfit) =
            abi.decode(params, (address, SwapStep[], uint256));
    }

    function decodeV3Fee(bytes calldata extraData) external pure returns (uint24 fee) {
        require(extraData.length == 32, "bad len");
        fee = abi.decode(extraData, (uint24));
    }
}
