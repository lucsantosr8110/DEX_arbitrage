// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import "@openzeppelin/contracts/token/ERC20/IERC20.sol";

/// @dev Mirrors FlashloanExecutor profitRecipient + V3 fee fail-closed rules for unit tests
/// without deploying allowances against live Polygon routers.
contract EconomicGateProbe {
    using SafeERC20 for IERC20;

    address public immutable owner;

    error UnauthorizedProfitRecipient(address recipient);
    error InvalidV3Fee(uint24 fee);
    error InvalidV3FeeExtraDataLength(uint256 length);

    constructor() {
        owner = msg.sender;
    }

    /// @dev Keep in sync with FlashloanExecutor._decodeV3Fee
    function decodeV3Fee(bytes calldata extraData) external pure returns (uint24 fee) {
        if (extraData.length != 32) revert InvalidV3FeeExtraDataLength(extraData.length);
        fee = abi.decode(extraData, (uint24));
        if (fee != 500 && fee != 3000 && fee != 10000) revert InvalidV3Fee(fee);
    }

    /// @dev Keep in sync with FlashloanExecutor._requireAuthorizedProfitRecipient
    function requireProfitRecipient(address recipient) external view {
        if (recipient != owner) revert UnauthorizedProfitRecipient(recipient);
    }

    function payoutProfit(address asset, address profitRecipient, uint256 profit) external {
        if (profitRecipient != owner) revert UnauthorizedProfitRecipient(profitRecipient);
        if (profit > 0) {
            IERC20(asset).safeTransfer(profitRecipient, profit);
        }
    }
}
