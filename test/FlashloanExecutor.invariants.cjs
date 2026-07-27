const { expect } = require("chai");
const { loadFixture } = require("@nomicfoundation/hardhat-toolbox/network-helpers");
const { ethers } = require("hardhat");

describe("FlashloanExecutor economic invariants", function () {
  async function deployProbeFixture() {
    const [owner, relayer, stranger] = await ethers.getSigners();
    const Probe = await ethers.getContractFactory("EconomicGateProbe");
    const probe = await Probe.deploy();
    await probe.waitForDeployment();
    return { probe, owner, relayer, stranger };
  }

  describe("V3 extraData fail-closed", function () {
    it("reverts on empty extraData", async function () {
      const { probe } = await loadFixture(deployProbeFixture);
      await expect(probe.decodeV3Fee("0x")).to.be.revertedWithCustomError(
        probe,
        "InvalidV3FeeExtraDataLength"
      );
    });

    it("reverts on truncated extraData", async function () {
      const { probe } = await loadFixture(deployProbeFixture);
      await expect(probe.decodeV3Fee("0x0001")).to.be.revertedWithCustomError(
        probe,
        "InvalidV3FeeExtraDataLength"
      );
    });

    it("reverts on invalid fee tier", async function () {
      const { probe } = await loadFixture(deployProbeFixture);
      const bad = ethers.AbiCoder.defaultAbiCoder().encode(["uint24"], [100]);
      await expect(probe.decodeV3Fee(bad)).to.be.revertedWithCustomError(probe, "InvalidV3Fee");
    });

    it("accepts exact fee 500/3000/10000", async function () {
      const { probe } = await loadFixture(deployProbeFixture);
      for (const fee of [500, 3000, 10000]) {
        const payload = ethers.AbiCoder.defaultAbiCoder().encode(["uint24"], [fee]);
        expect(await probe.decodeV3Fee(payload)).to.equal(fee);
      }
    });
  });

  describe("profitRecipient authorization", function () {
    it("allows owner as profitRecipient", async function () {
      const { probe, owner } = await loadFixture(deployProbeFixture);
      await expect(probe.requireProfitRecipient(owner.address)).to.not.be.reverted;
    });

    it("rejects relayer/stranger as profitRecipient", async function () {
      const { probe, relayer, stranger } = await loadFixture(deployProbeFixture);
      await expect(probe.requireProfitRecipient(relayer.address))
        .to.be.revertedWithCustomError(probe, "UnauthorizedProfitRecipient")
        .withArgs(relayer.address);
      await expect(probe.requireProfitRecipient(stranger.address))
        .to.be.revertedWithCustomError(probe, "UnauthorizedProfitRecipient")
        .withArgs(stranger.address);
    });

    it("pays profit only to owner, not to relayer", async function () {
      const { probe, owner, relayer } = await loadFixture(deployProbeFixture);

      const MockFactory = await ethers.getContractFactory("MockERC20Profit");
      const token = await MockFactory.deploy("MockUSD", "mUSD", 6);
      await token.waitForDeployment();

      const profit = 1_000_000n;
      await token.mint(await probe.getAddress(), profit);

      const ownerBefore = await token.balanceOf(owner.address);
      const probeBefore = await token.balanceOf(await probe.getAddress());
      const relayerBefore = await token.balanceOf(relayer.address);

      await expect(
        probe.connect(relayer).payoutProfit(await token.getAddress(), relayer.address, profit)
      )
        .to.be.revertedWithCustomError(probe, "UnauthorizedProfitRecipient")
        .withArgs(relayer.address);

      // Relayer may *call*, but recipient must be owner.
      await probe.connect(relayer).payoutProfit(await token.getAddress(), owner.address, profit);

      expect(await token.balanceOf(owner.address)).to.equal(ownerBefore + profit);
      expect(await token.balanceOf(await probe.getAddress())).to.equal(probeBefore - profit);
      expect(await token.balanceOf(relayer.address)).to.equal(relayerBefore);
    });
  });
});
