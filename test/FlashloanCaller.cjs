const { expect } = require("chai");
const { loadFixture } = require("@nomicfoundation/hardhat-toolbox/network-helpers");

describe("FlashloanCaller", function () {
  async function deployFixture() {
    const [owner, other] = await ethers.getSigners();
    const Caller = await ethers.getContractFactory("FlashloanCaller");
    const caller = await Caller.deploy(owner.address);
    return { caller, owner, other };
  }

  it("sets owner and executor", async function () {
    const { caller, owner } = await loadFixture(deployFixture);
    expect(await caller.owner()).to.equal(owner.address);
    expect(await caller.executor()).to.equal(owner.address);
  });

  it("restricts executor updates to owner", async function () {
    const { caller, other } = await loadFixture(deployFixture);
    await expect(caller.connect(other).updateExecutor(other.address))
      .to.be.revertedWith("Not owner");
  });

  it("rejects zero and self executor", async function () {
    const { caller } = await loadFixture(deployFixture);
    await expect(caller.updateExecutor(ethers.ZeroAddress))
      .to.be.revertedWith("Invalid executor");
    await expect(caller.updateExecutor(await caller.getAddress()))
      .to.be.revertedWith("Invalid executor self");
  });

  it("rejects invalid flashloan inputs before external call", async function () {
    const { caller } = await loadFixture(deployFixture);
    await expect(caller.triggerFlashloan(ethers.ZeroAddress, 1, "0x"))
      .to.be.revertedWith("Invalid asset");
    await expect(caller.triggerFlashloan("0x0000000000000000000000000000000000000001", 0, "0x"))
      .to.be.revertedWith("Invalid amount");
  });
});
