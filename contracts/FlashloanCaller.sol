// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/**
 * =============================================================
 * FLASHLOAN CALLER — v4.4.3-final-patch1
 * -------------------------------------------------------------
 * ✅ Compatível com FlashloanExecutor v4.4.3
 * ✅ Simples, seguro e sem dependências externas
 * ✅ Dispara empréstimos Aave v3 via Executor designado
 * =============================================================
 */

interface IAavePool {
    function flashLoanSimple(
        address receiver,
        address asset,
        uint256 amount,
        bytes calldata params,
        uint16 referralCode
    ) external;
}

contract FlashloanCaller {
    address public immutable owner;
    address public executor; // endereço do FlashloanExecutor
    address public constant AAVE_POOL = 0x794a61358D6845594F94dc1DB02A252b5b4814aD;

    event FlashloanTriggered(address indexed initiator, address indexed asset, uint256 amount);
    event ExecutorUpdated(address indexed newExecutor);

    modifier onlyOwner() {
        require(msg.sender == owner, "Not owner");
        _;
    }

    constructor(address _executor) {
        owner = msg.sender;
        executor = _executor;
    }

    /// Atualiza o endereço do executor (contrato principal)
    function updateExecutor(address _executor) external onlyOwner {
        require(_executor != address(0), "Invalid executor");
        require(_executor != address(this), "Invalid executor self");
        executor = _executor;
        emit ExecutorUpdated(_executor);
    }

    /// Dispara o flashloan da Aave v3 chamando o Executor
    function triggerFlashloan(
        address asset,
        uint256 amount,
        bytes calldata params
    ) external onlyOwner {
        require(executor != address(0), "Executor not set");
        require(asset != address(0), "Invalid asset");
        require(amount > 0, "Invalid amount");

        IAavePool(AAVE_POOL).flashLoanSimple(
            executor,   // receiver é o executor (implementa executeOperation)
            asset,
            amount,
            params,
            0
        );

        emit FlashloanTriggered(msg.sender, asset, amount);
    }

    receive() external payable {}
}
