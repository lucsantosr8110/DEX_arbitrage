// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/**
 * =============================================================
 * ⚡ FLASHLOAN EXECUTOR — v4.4.4-audit-fix (Polygon Mainnet)
 * -------------------------------------------------------------
 * ✅ Compatível com OpenZeppelin Contracts v5.4.x
 * ✅ Wrapper dinâmico + multi-wrapper (authorizedWrappers)
 * ✅ executeOperation valida:
 * • initiator == wrapperAddress
 * • authorizedWrappers[initiator]
 * • initiator == address(this) (execução direta)
 * ✅ AUDIT 2026-07-25:
 * • executeDirect ERC20 reverte em perda (require finalAmount >= amount)
 * • executeOperation chama _validateSteps (defesa em profundidade)
 * • executeOperation respeita paused (whenNotPaused)
 * ✅ USDT-safe approvals (zera antes quando necessário)
 * ✅ MIN_AMOUNT dinâmico por decimals() do token
 * ✅ Uniswap V3 fees (Polygon): 500 / 3000 / 10000
 * ✅ Métricas, anti-MEV, pausas e eventos de administração
 * ✅ Total compatibilidade com FlashloanCaller v4.4.3-final
 * ✅ NOVO: executeDirect para capital próprio
 * 🟢 NOVO: Compatibilidade com MATIC nativo (via wrap/unwrap em WMATIC)
 * =============================================================
 */

import "@openzeppelin/contracts/utils/ReentrancyGuard.sol";
import "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import "@openzeppelin/contracts/token/ERC20/extensions/IERC20Metadata.sol";
import "@openzeppelin/contracts/utils/Address.sol";
import "@openzeppelin/contracts/utils/math/Math.sol";

interface IAavePool {
    function flashLoanSimple(
        address receiver,
        address asset,
        uint256 amount,
        bytes calldata params,
        uint16 referralCode
    ) external;
}

interface IFlashLoanSimpleReceiver {
    function executeOperation(
        address asset,
        uint256 amount,
        uint256 premium,
        address initiator,
        bytes calldata params
    ) external returns (bool);
}

interface IUniswapV2Router {
    function swapExactTokensForTokens(
        uint256 amountIn,
        uint256 amountOutMin,
        address[] calldata path,
        address to,
        uint256 deadline
    ) external returns (uint256[] memory amounts);
}

interface IUniswapV3Router {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 deadline;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }
    function exactInputSingle(ExactInputSingleParams calldata params)
        external
        payable
        returns (uint256 amountOut);
}

// 🟢 Interface WMATIC (IWETH-style)
interface IWMATIC {
    function deposit() external payable;
    function withdraw(uint256 amount) external;
}

contract FlashloanExecutor is ReentrancyGuard, IFlashLoanSimpleReceiver {
    using SafeERC20 for IERC20;
    using Address for address;
    using Math for uint256;

    // =============================================================
    // CUSTOM ERRORS (fail-closed, structured)
    // =============================================================
    /// @notice `CallbackData.profitRecipient` must be the immutable owner.
    error UnauthorizedProfitRecipient(address recipient);
    /// @notice V3 hop requires abi.encode(uint24) fee (exactly 32 bytes); no silent 3000 fallback.
    error InvalidV3Fee(uint24 fee);
    error InvalidV3FeeExtraDataLength(uint256 length);

    // =============================================================
    // ENUMS & STRUCTS
    // =============================================================
    enum DexType { QUICKSWAP, SUSHISWAP, UNISWAP_V3 }

    struct SwapStep {
        DexType dexType;
        address tokenIn;
        address tokenOut;
        uint256 amountOutMin;
        bytes  extraData;
    }

    struct SlippageConfig {
        uint16 baseSlippage;
        uint16 maxSlippage;
        bool   enabled;
    }

    /// @dev Economic fields for flashloan callback.
    /// Wire encoding is FLAT: `abi.encode(profitRecipient, steps, minProfit)`.
    /// Do NOT `abi.encode(CallbackData)` / `abi.decode(params, (CallbackData))` —
    /// struct typing adds an outer dynamic offset that breaks Rust + executeFlashloan.
    struct CallbackData {
        address profitRecipient;
        SwapStep[] steps;
        uint256 minProfit;
    }

    // =============================================================
    // CONSTANTS - Polygon Mainnet
    // =============================================================
    address public immutable owner;
    bool public paused;

    address public constant AAVE_POOL          = 0x794a61358D6845594F94dc1DB02A252b5b4814aD;
    address public constant QUICKSWAP_ROUTER   = 0xa5E0829CaCEd8fFDD4De3c43696c57F7D7A678ff;
    address public constant SUSHISWAP_ROUTER   = 0x1b02dA8Cb0d097eB8D57A175b88c7D8b47997506;
    address public constant UNISWAP_V3_ROUTER  = 0xE592427A0AEce92De3Edee1F18E0157C05861564;

    // Circle USDC nativo Polygon PoS. USDC.e permanece para rotas/pools legados.
    address public constant TOKEN_USDC         = 0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359;
    address public constant TOKEN_USDC_E       = 0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174;
    address public constant TOKEN_DAI          = 0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063;
    address public constant TOKEN_WMATIC       = 0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270;
    address public constant TOKEN_USDT         = 0xc2132D05D31c914a87C6611C10748AEb04B58e8F;

    // 🟢 Identificador para MATIC nativo
    address public constant NATIVE_ASSET       = address(0);

    // =============================================================
    // DYNAMIC CONFIGURATION
    // =============================================================
    address public wrapperAddress;
    mapping(address => bool) public authorizedWrappers;
    mapping(address => bool) public allowedExecutors;
    mapping(address => SlippageConfig) public slippageConfigs;
    mapping(address => uint256) public minAmountByAsset;

    // =============================================================
    // EXECUTION VARIABLES & METRICS
    // =============================================================
    uint256 public lastExecutionBlock;
    uint256 public constant MAX_SLIPPAGE_BPS      = 500;
    uint256 public constant MAX_STEPS             = 5;
    uint256 public constant SWAP_DEADLINE_SECONDS = 300;

    uint256 public totalFlashloans;
    uint256 public totalProfit;
    uint256 public totalPremiumPaid;
    uint256 public failedFlashloans;

    // =============================================================
    // EVENTS
    // =============================================================
    event FlashLoanSuccess(address indexed asset, uint256 amount, uint256 premium, uint256 profit, address indexed profitRecipient);
    event FlashLoanFailure(address indexed asset, uint256 amount, string reason, address indexed executor);
    event SwapExecuted(uint256 indexed stepIndex, DexType dexType, address tokenIn, address tokenOut, uint256 amountIn, uint256 amountOut);
    event ExecutorUpdated(address indexed executor, bool allowed);
    event SlippageConfigUpdated(address indexed token, uint16 baseSlippage, uint16 maxSlippage, bool enabled);
    event MinAmountUpdated(address indexed token, uint256 minAmount);
    event WrapperUpdated(address indexed oldWrapper, address indexed newWrapper);
    event WrapperAuthorized(address indexed wrapper, bool allowed);
    event Withdrawn(address indexed token, uint256 amount);
    event EmergencyStop();
    event Resumed();
    event MetricsUpdated(uint256 flashloans, uint256 profit, uint256 premiumPaid, uint256 failed);

    // ✅ Capital próprio (direto)
    event DirectExecution(
        address indexed asset,
        uint256 amountIn,
        uint256 amountOut,
        address indexed executor,
        uint256 profit
    );

    // =============================================================
    // MODIFIERS
    // =============================================================
    modifier onlyOwner() {
        require(msg.sender == owner, "Not owner");
        _;
    }
    modifier onlyExecutor() {
        require(allowedExecutors[msg.sender], "Not executor");
        _;
    }
    modifier whenNotPaused() {
        require(!paused, "Paused");
        _;
    }
    modifier antiMEV() {
        require(block.number != lastExecutionBlock, "Same block");
        lastExecutionBlock = block.number;
        _;
    }

    // =============================================================
    // CONSTRUCTOR
    // =============================================================
    constructor(address _wrapperAddress) {
        owner = msg.sender;
        wrapperAddress = _wrapperAddress;
        allowedExecutors[msg.sender] = true;

        _setDefaultSlippageConfigs();
        _setMaxAllowancesRoutersOnly();
    }

    // =============================================================
    // MAIN EXECUTION
    // =============================================================
    function executeFlashloan(
        address asset,
        uint256 amount,
        SwapStep[] calldata steps,
        uint256 minProfit
    )
        external
        onlyExecutor
        nonReentrant
        whenNotPaused
        antiMEV
        returns (bool)
    {
        // Flashloan exige ERC20 (para MATIC use WMATIC)
        require(asset != address(0), "Invalid asset");
        require(amount >= _minAmount(asset), "Amount too low");
        _validateSteps(steps, asset);

        // Profit always settles to immutable `owner`, never msg.sender.
        // Relayer/allowedExecutor may call; economic recipient stays the owner.
        bytes memory params = abi.encode(owner, steps, minProfit);

        try IAavePool(AAVE_POOL).flashLoanSimple(address(this), asset, amount, params, 0) {
            return true;
        } catch {
            failedFlashloans++;
            emit FlashLoanFailure(asset, amount, "Flashloan initiation failed", msg.sender);
            return false;
        }
    }

    // ✅ EXECUÇÃO COM CAPITAL PRÓPRIO (suporta MATIC nativo)
    function executeDirect(
        address asset,
        uint256 amount,
        SwapStep[] calldata steps,
        uint256 minProfit
    )
        external
        payable
        onlyExecutor
        nonReentrant
        whenNotPaused
        antiMEV
        returns (uint256 finalAmount)
    {
        // Permite duas formas:
        // 1) ERC20: asset != 0x0 e msg.value == 0
        // 2) MATIC nativo: asset == 0x0 e msg.value == amount
        require(
            (asset != NATIVE_ASSET && msg.value == 0) ||
            (asset == NATIVE_ASSET && msg.value == amount),
            "Invalid asset/value"
        );

        address initialAsset = asset;
        bool isNative = (asset == NATIVE_ASSET);

        // Snapshot de WMATIC antes (previne drenar saldo prévio do contrato)
        uint256 wmaticBefore = IERC20(TOKEN_WMATIC).balanceOf(address(this));

        if (isNative) {
            // Valor mínimo será baseado em WMATIC
            uint256 minAmt = _minAmount(TOKEN_WMATIC);
            require(amount >= minAmt, "Amount too low");
            // Wrap MATIC -> WMATIC
            IWMATIC(TOKEN_WMATIC).deposit{value: amount}();
            // A partir daqui, a arbitragem roda em WMATIC
            asset = TOKEN_WMATIC;
        } else {
            // ERC20
            require(amount >= _minAmount(asset), "Amount too low");
            IERC20(asset).safeTransferFrom(msg.sender, address(this), amount);
        }

        // Steps devem iniciar e terminar no mesmo asset em que a arbitragem ocorrerá (WMATIC/erc20)
        _validateSteps(steps, asset);

        // Executa o ciclo retornando ao mesmo 'asset' (WMATIC se isNative)
        finalAmount = this._executeArbitrage(asset, amount, steps);

        uint256 profit = 0;

        if (isNative) {
            // Calcula o delta gerado (capital + lucro) somente deste ciclo
            uint256 wmaticAfter = IERC20(TOKEN_WMATIC).balanceOf(address(this));
            uint256 delta = wmaticAfter - wmaticBefore; // deve ser >= amount
            require(delta >= amount + minProfit, "Profit below minimum");

            profit = delta - amount;
            if (profit > 0) totalProfit += profit;

            // Unwrap do delta e devolução em MATIC
            IWMATIC(TOKEN_WMATIC).withdraw(delta);

            (bool ok, ) = msg.sender.call{value: delta}("");
            require(ok, "MATIC transfer failed");

            finalAmount = delta; // Valor total devolvido em MATIC
        } else {
            // ERC20: devolve capital + lucro
            // AUDIT fix #1: reverter em perda — paridade com o caminho nativo (delta >= amount).
            require(finalAmount >= amount + minProfit, "Profit below minimum");
            if (finalAmount > amount) {
                profit = finalAmount - amount;
                totalProfit += profit;
            }
            IERC20(asset).safeTransfer(msg.sender, finalAmount);
        }

        emit DirectExecution(initialAsset, amount, finalAmount, msg.sender, profit);
        emit MetricsUpdated(totalFlashloans, totalProfit, totalPremiumPaid, failedFlashloans);
        return finalAmount;
    }

    // =============================================================
    // AAVE CALLBACK
    // =============================================================
    function executeOperation(
        address asset,
        uint256 amount,
        uint256 premium,
        address initiator,
        bytes calldata params
    ) external nonReentrant whenNotPaused returns (bool) {
        require(msg.sender == AAVE_POOL, "Only Aave");
        require(amount >= _minAmount(asset), "Amount below minimum");

        bool isValidInitiator =
            (wrapperAddress != address(0) && initiator == wrapperAddress) ||
            authorizedWrappers[initiator] ||
            initiator == address(this);
        require(isValidInitiator, "Invalid initiator");

        // Flat ABI: abi.encode(address, SwapStep[], uint256) — NOT abi.encode(CallbackData).
        // Struct-typed decode expects an outer offset prefix and would revert on Rust/ethers
        // and on this contract's own executeFlashloan params.
        CallbackData memory data;
        (data.profitRecipient, data.steps, data.minProfit) =
            abi.decode(params, (address, SwapStep[], uint256));
        _requireAuthorizedProfitRecipient(data.profitRecipient);

        // AUDIT fix #2: validar steps vindos via callback (FlashloanCaller passa params cru).
        // Defesa em profundidade — alinha com executeFlashloan/executeDirect.
        _validateStepsMemory(data.steps, asset);

        totalFlashloans++;
        totalPremiumPaid += premium;
        uint256 totalOwed = amount + premium;

        // 🔧 CORREÇÃO (ESTADO_ATUAL.md §4.3): Removido try/catch que engolia falhas
        // da arbitragem interna. Antes, executeOperation sempre retornava true para
        // Aave — mesmo quando a arbitragem falhava — fazendo o bot perder o premium
        // do flashloan em cada tentativa falha. Agora, se _executeArbitrage reverter
        // (swap falhou, slippage alto, rota inviável) ou se finalAmount < totalOwed
        // (não lucrativo), a transação inteira reverte. Aave devolve os fundos sem
        // cobrar premium. O bot perde só gás, não gás + premium.
        //
        // A simulação Rust (simulate_bool_transaction / simulate_transaction via
        // eth_call) agora detecta reverts corretamente — falso positivo eliminado.
        uint256 finalAmount = this._executeArbitrage(asset, amount, data.steps);
        require(finalAmount >= totalOwed + data.minProfit, "Profit below minimum");

        _approveTo(AAVE_POOL, asset, totalOwed);

        uint256 profit = finalAmount - totalOwed;
        if (profit > 0) {
            totalProfit += profit;
            IERC20(asset).safeTransfer(data.profitRecipient, profit);
        }
        emit FlashLoanSuccess(asset, amount, premium, profit, data.profitRecipient);

        emit MetricsUpdated(totalFlashloans, totalProfit, totalPremiumPaid, failedFlashloans);
        return true;
    }

    // =============================================================
    // ARBITRAGE EXECUTION (EXTERNAL FOR TRY-CATCH)
    // =============================================================
    function _executeArbitrage(address initialAsset, uint256 initialAmount, SwapStep[] memory steps)
        external
        returns (uint256)
    {
        require(msg.sender == address(this), "Only internal call");

        uint256 currentAmount = initialAmount;
        address currentToken = initialAsset;

        for (uint256 i = 0; i < steps.length; ) {
            SwapStep memory step = steps[i];
            require(currentToken == step.tokenIn, "Token mismatch");

            // Rust calcula amountOutMin uma única vez a partir do quote. Aplicar
            // slippage do contrato aqui criava segundo haircut e abria margem MEV.
            uint256 received = _executeSingleSwap(step, currentAmount, step.amountOutMin);
            require(received >= step.amountOutMin, "Insufficient output");

            emit SwapExecuted(i, step.dexType, step.tokenIn, step.tokenOut, currentAmount, received);

            currentToken = step.tokenOut;
            currentAmount = received;
            unchecked { ++i; }
        }

        require(currentToken == initialAsset, "Cycle incomplete");
        return currentAmount;
    }

    // =============================================================
    // SWAP EXECUTION
    // =============================================================
    function _executeSingleSwap(SwapStep memory step, uint256 amount, uint256 minReturn)
        internal
        returns (uint256)
    {
        if (step.dexType == DexType.QUICKSWAP)
            return _execV2(QUICKSWAP_ROUTER, step, amount, minReturn);
        if (step.dexType == DexType.SUSHISWAP)
            return _execV2(SUSHISWAP_ROUTER, step, amount, minReturn);
        if (step.dexType == DexType.UNISWAP_V3)
            return _execV3(step, amount, minReturn);
        revert("Unsupported DEX");
    }

    function _execV2(address router, SwapStep memory step, uint256 amount, uint256 minReturn)
        internal
        returns (uint256)
    {
        // 🟢 CORREÇÃO DE SINTAXE APLICADA AQUI
        address[] memory path = new address[](2);
        path[0] = step.tokenIn;
        path[1] = step.tokenOut;

        _ensureAllowance(step.tokenIn, router, amount);

        uint256[] memory amounts = IUniswapV2Router(router).swapExactTokensForTokens(
            amount,
            minReturn,
            path,
            address(this),
            block.timestamp + SWAP_DEADLINE_SECONDS
        );

        return amounts[amounts.length - 1];
    }

    function _execV3(SwapStep memory step, uint256 amount, uint256 minReturn)
        internal
        returns (uint256)
    {
        uint24 fee = _decodeV3Fee(step.extraData);

        _ensureAllowance(step.tokenIn, UNISWAP_V3_ROUTER, amount);

        return IUniswapV3Router(UNISWAP_V3_ROUTER).exactInputSingle(
            IUniswapV3Router.ExactInputSingleParams({
                tokenIn: step.tokenIn,
                tokenOut: step.tokenOut,
                fee: fee,
                recipient: address(this),
                deadline: block.timestamp + SWAP_DEADLINE_SECONDS,
                amountIn: amount,
                amountOutMinimum: minReturn,
                sqrtPriceLimitX96: 0
            })
        );
    }

    /// @dev Rust encodes `abi.encode(uint24)` → 32 bytes. Empty/short payload must not
    /// silently fall back to fee 3000 (wrong pool / economics).
    function _decodeV3Fee(bytes memory extraData) internal pure returns (uint24 fee) {
        if (extraData.length != 32) revert InvalidV3FeeExtraDataLength(extraData.length);
        fee = abi.decode(extraData, (uint24));
        if (fee != 500 && fee != 3000 && fee != 10000) revert InvalidV3Fee(fee);
    }

    function _requireAuthorizedProfitRecipient(address recipient) internal view {
        if (recipient != owner) revert UnauthorizedProfitRecipient(recipient);
    }

    // =============================================================
    // HELPERS
    // =============================================================
    function _minAmount(address asset) internal view returns (uint256) {
        uint256 cachedMin = minAmountByAsset[asset];
        if (cachedMin > 0) return cachedMin;

        // Para assets ERC20, pega decimals(); para casos extremos, default 1e6
        try IERC20Metadata(asset).decimals() returns (uint8 dec) {
            require(dec <= 18, "decimals too large");
            return 10 ** uint256(dec);
        } catch {
            return 1_000_000;
        }
    }

    // USDT-safe: zero antes de aprovar quando necessário
    function _ensureAllowance(address token, address spender, uint256 amount) internal {
        IERC20 erc = IERC20(token);
        uint256 cur = erc.allowance(address(this), spender);
        if (cur < amount) {
            if (cur != 0) {
                erc.approve(spender, 0);
            }
            erc.approve(spender, type(uint256).max);
        }
    }

    function _approveTo(address spender, address token, uint256 amount) internal {
        IERC20 erc = IERC20(token);
        uint256 cur = erc.allowance(address(this), spender);
        if (cur < amount) {
            if (cur != 0) {
                erc.approve(spender, 0);
            }
            erc.approve(spender, amount);
        }
    }

    function _validateSteps(SwapStep[] calldata steps, address initialAsset) internal pure {
        require(steps.length > 0, "No steps provided");
        require(steps.length <= MAX_STEPS, "Too many steps");
        require(steps[steps.length - 1].tokenOut == initialAsset, "Final token mismatch");

        for (uint256 i = 0; i < steps.length; ) {
            // Steps sempre usam ERC20 (WMATIC para o caso nativo)
            require(steps[i].tokenIn != address(0) && steps[i].tokenOut != address(0), "Zero token in swap");
            require(steps[i].amountOutMin > 0, "Min out is zero");

            if (i == 0) {
                require(steps[0].tokenIn == initialAsset, "First token mismatch");
            } else {
                require(steps[i-1].tokenOut == steps[i].tokenIn, "Token sequence broken");
            }
            unchecked { ++i; }
        }
    }

    // AUDIT fix #2: versão memory para o callback (params decodificados vêm em memory).
    function _validateStepsMemory(SwapStep[] memory steps, address initialAsset) internal pure {
        require(steps.length > 0, "No steps provided");
        require(steps.length <= MAX_STEPS, "Too many steps");
        require(steps[steps.length - 1].tokenOut == initialAsset, "Final token mismatch");

        for (uint256 i = 0; i < steps.length; ) {
            require(steps[i].tokenIn != address(0) && steps[i].tokenOut != address(0), "Zero token in swap");
            require(steps[i].amountOutMin > 0, "Min out is zero");

            if (i == 0) {
                require(steps[0].tokenIn == initialAsset, "First token mismatch");
            } else {
                require(steps[i - 1].tokenOut == steps[i].tokenIn, "Token sequence broken");
            }
            unchecked { ++i; }
        }
    }

    function _setDefaultSlippageConfigs() internal {
        slippageConfigs[TOKEN_USDC]   = SlippageConfig(100, 300, true);
        slippageConfigs[TOKEN_USDC_E] = SlippageConfig(100, 300, true);
        slippageConfigs[TOKEN_DAI]    = SlippageConfig(100, 300, true);
        slippageConfigs[TOKEN_WMATIC] = SlippageConfig(150, 400, true);
        slippageConfigs[TOKEN_USDT]   = SlippageConfig(100, 300, true);
    }

    function _setMaxAllowancesRoutersOnly() internal {
        address[5] memory tokens  = [TOKEN_USDC, TOKEN_USDC_E, TOKEN_DAI, TOKEN_WMATIC, TOKEN_USDT];
        address[3] memory routers = [QUICKSWAP_ROUTER, SUSHISWAP_ROUTER, UNISWAP_V3_ROUTER];

        for (uint256 i = 0; i < tokens.length; ) {
            for (uint256 j = 0; j < routers.length; ) {
                _ensureAllowance(tokens[i], routers[j], type(uint256).max);
                unchecked { ++j; }
            }
            unchecked { ++i; }
        }
    }

    // =============================================================
    // ADMIN FUNCTIONS
    // =============================================================
    function updateExecutor(address exec, bool allowed) external onlyOwner {
        allowedExecutors[exec] = allowed;
        emit ExecutorUpdated(exec, allowed);
    }

    function updateSlippageConfig(address token, uint16 base, uint16 max, bool enabled)
        external onlyOwner
    {
        require(max <= MAX_SLIPPAGE_BPS, "Max slippage too high");
        slippageConfigs[token] = SlippageConfig(base, max, enabled);
        emit SlippageConfigUpdated(token, base, max, enabled);
    }

    function updateMinAmount(address token, uint256 minAmount) external onlyOwner {
        require(minAmount > 0, "Min amount zero");
        minAmountByAsset[token] = minAmount;
        emit MinAmountUpdated(token, minAmount);
    }

    function emergencyStop() external onlyOwner {
        paused = true;
        emit EmergencyStop();
    }

    function resume() external onlyOwner {
        paused = false;
        emit Resumed();
    }

    function withdrawToken(address token, uint256 amount) external onlyOwner {
        IERC20(token).safeTransfer(owner, amount);
        emit Withdrawn(token, amount);
    }

    function withdrawMATIC() external onlyOwner {
        uint256 bal = address(this).balance;
        require(bal > 0, "No MATIC");
        (bool success, ) = owner.call{value: bal}("");
        require(success, "MATIC transfer failed");
        emit Withdrawn(address(0), bal);
    }

    // =============================================================
    // WRAPPER MANAGEMENT
    // =============================================================
    function setWrapper(address newWrapper) external onlyOwner {
        address old = wrapperAddress;
        wrapperAddress = newWrapper;
        emit WrapperUpdated(old, newWrapper);
    }

    function authorizeWrapper(address w, bool allowed) external onlyOwner {
        authorizedWrappers[w] = allowed;
        emit WrapperAuthorized(w, allowed);
    }

    // =============================================================
    // VIEW HELPERS
    // =============================================================
    function getMetrics() external view returns (uint256, uint256, uint256, uint256) {
        return (totalFlashloans, totalProfit, totalPremiumPaid, failedFlashloans);
    }

    function getTokenBalance(address token) external view returns (uint256) {
        return IERC20(token).balanceOf(address(this));
    }

    function getMATICBalance() external view returns (uint256) {
        return address(this).balance;
    }

    function getMinAmount(address asset) external view returns (uint256) {
        return _minAmount(asset);
    }

    receive() external payable {}
}
