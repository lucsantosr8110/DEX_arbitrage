const { expect } = require("chai");
const { ethers, network } = require("hardhat");

/**
 * Polygon fork tests for V3 extraData + profitRecipient rules.
 * Run: HARDHAT_FORK_POLYGON=1 npx hardhat test test/polygon_fork_v3.cjs
 *
 * Uses EconomicGateProbe (same fail-closed rules as FlashloanExecutor) so we
 * don't need live Uniswap routers for fee/recipient validation. Fork proves
 * chainId/block pinning and that RPC state is Polygon.
 */
describe("Polygon fork — V3 fee + profitRecipient", function () {
  before(function () {
    if (process.env.HARDHAT_FORK_POLYGON !== "1") {
      this.skip();
    }
  });

  let probe;
  let owner;
  let relayer;
  let forkBlock;

  before(async function () {
    [owner, relayer] = await ethers.getSigners();
    const Probe = await ethers.getContractFactory("EconomicGateProbe");
    probe = await Probe.deploy();
    await probe.waitForDeployment();
    forkBlock = await ethers.provider.getBlockNumber();
    console.log(`  [fork] chainId=${(await ethers.provider.getNetwork()).chainId} block=${forkBlock}`);
  });

  it("fork carries Polygon mainnet state at pinned block", async function () {
    // Hardhat's local "hardhat" network reports its own configured chainId
    // (31337) even while forking — that's expected Hardhat behavior, not a
    // sign the fork failed. We prove the fork is really Polygon by checking
    // on-chain state (deployed bytecode) that only exists there, not by
    // trusting eth_chainId.
    const net = await ethers.provider.getNetwork();
    expect(Number(net.chainId)).to.equal(31337);
    expect(forkBlock).to.be.greaterThan(90_000_000);

    // WMATIC exists on Polygon mainnet
    const wmaticCode = await ethers.provider.getCode(
      "0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270"
    );
    expect(wmaticCode.length).to.be.greaterThan(2);

    // Old FlashloanExecutor deployment exists on Polygon mainnet
    const oldExecutorCode = await ethers.provider.getCode(
      "0xB78C7513B4d546456ba0E0cC38980799C34E6D67"
    );
    expect(oldExecutorCode.length).to.be.greaterThan(2);
  });

  it("fee 500 explicit — no silent 3000 fallback", async function () {
    const payload = ethers.AbiCoder.defaultAbiCoder().encode(["uint24"], [500]);
    expect(payload.length).to.equal(66);
    expect(await probe.decodeV3Fee(payload)).to.equal(500);
  });

  it("fee 3000 only when explicitly encoded", async function () {
    const payload = ethers.AbiCoder.defaultAbiCoder().encode(["uint24"], [3000]);
    expect(await probe.decodeV3Fee(payload)).to.equal(3000);
  });

  it("empty / truncated / oversized extraData revert", async function () {
    await expect(probe.decodeV3Fee("0x")).to.be.revertedWithCustomError(
      probe,
      "InvalidV3FeeExtraDataLength"
    );
    for (const bad of ["0x00", "0x000000", "0x" + "00".repeat(31), "0x" + "00".repeat(33)]) {
      await expect(probe.decodeV3Fee(bad)).to.be.revertedWithCustomError(
        probe,
        "InvalidV3FeeExtraDataLength"
      );
    }
  });

  it("invalid fees revert", async function () {
    for (const fee of [0, 100, 1234]) {
      const payload = ethers.AbiCoder.defaultAbiCoder().encode(["uint24"], [fee]);
      await expect(probe.decodeV3Fee(payload)).to.be.revertedWithCustomError(
        probe,
        "InvalidV3Fee"
      );
    }
  });

  it("relayer may call but only owner is valid profitRecipient", async function () {
    await expect(probe.connect(relayer).requireProfitRecipient(owner.address)).to.not.be
      .reverted;
    await expect(probe.connect(relayer).requireProfitRecipient(relayer.address))
      .to.be.revertedWithCustomError(probe, "UnauthorizedProfitRecipient")
      .withArgs(relayer.address);
  });

  it("CallbackData decode: minProfit raw preserved (pass/fail semantics off-chain)", async function () {
    const Decoder = await ethers.getContractFactory("CallbackDataDecoder");
    const decoder = await Decoder.deploy();
    const fee500 = ethers.AbiCoder.defaultAbiCoder().encode(["uint24"], [500]);
    const coder = ethers.AbiCoder.defaultAbiCoder();
    const types = ["address", "tuple(uint8,address,address,uint256,bytes)[]", "uint256"];
    const passMin = 1n;
    const failMin = 10n ** 24n; // absurdly high raw floor
    for (const [label, minProfit] of [
      ["pass_floor", passMin],
      ["fail_floor", failMin],
    ]) {
      const hex = coder.encode(types, [
        owner.address,
        [
          [
            2,
            "0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270",
            "0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359",
            1n,
            fee500,
          ],
        ],
        minProfit,
      ]);
      const d = await decoder.decode(hex);
      expect(d.profitRecipient).to.equal(owner.address);
      expect(d.minProfit).to.equal(minProfit);
      expect(Number(await decoder.decodeV3Fee(d.steps[0].extraData))).to.equal(500);
      void label;
    }
  });
});
