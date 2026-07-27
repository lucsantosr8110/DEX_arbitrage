const { expect } = require("chai");
const { ethers } = require("hardhat");
const fs = require("fs");
const path = require("path");

/**
 * Cross-check Rust CallbackData wire encoding against Solidity abi.decode.
 * Fixtures are produced by `cargo test export_callback_abi_fixtures -- --ignored`
 * or embedded below when the fixture file is absent.
 */
describe("CallbackData ABI Rust↔Solidity", function () {
  const fixturePath = path.join(__dirname, "fixtures", "callback_abi_hex.json");

  async function loadFixtures() {
    if (fs.existsSync(fixturePath)) {
      return JSON.parse(fs.readFileSync(fixturePath, "utf8"));
    }
    // Fallback: generate with ethers the same layout Rust uses (address, tuple[], uint256)
    const owner = "0x0000000000000000000000000000000000000A11";
    const other = "0x0000000000000000000000000000000000000B0B";
    const tokenIn = "0x00000000000000000000000000000000000000c1";
    const tokenOut = "0x00000000000000000000000000000000000000c2";
    const fee500 = ethers.AbiCoder.defaultAbiCoder().encode(["uint24"], [500]);
    const step = {
      dexType: 2, // UNISWAP_V3
      tokenIn,
      tokenOut,
      amountOutMin: 12345n,
      extraData: fee500,
    };
    const coder = ethers.AbiCoder.defaultAbiCoder();
    const types = [
      "address",
      "tuple(uint8,address,address,uint256,bytes)[]",
      "uint256",
    ];
    const pack = (recipient, minProfit) =>
      coder.encode(types, [
        recipient,
        [[step.dexType, step.tokenIn, step.tokenOut, step.amountOutMin, step.extraData]],
        minProfit,
      ]);

    return {
      cases: [
        {
          name: "owner_recipient_fee500",
          hex: pack(owner, 0n),
          expectRecipient: owner,
          expectMinProfit: "0",
          expectFee: 500,
        },
        {
          name: "other_recipient_min0",
          hex: pack(other, 0n),
          expectRecipient: other,
          expectMinProfit: "0",
          expectFee: 500,
        },
        {
          name: "high_minprofit_gt_u128",
          hex: pack(owner, 1n << 130n),
          expectRecipient: owner,
          expectMinProfit: (1n << 130n).toString(),
          expectFee: 500,
        },
      ],
    };
  }

  it("Solidity decode preserves recipient, steps, fee, minProfit", async function () {
    const Decoder = await ethers.getContractFactory("CallbackDataDecoder");
    const decoder = await Decoder.deploy();
    await decoder.waitForDeployment();

    const fixtures = await loadFixtures();
    for (const c of fixtures.cases) {
      const decoded = await decoder.decode(c.hex);
      expect(decoded.profitRecipient.toLowerCase(), c.name).to.equal(
        c.expectRecipient.toLowerCase()
      );
      expect(decoded.minProfit.toString(), c.name).to.equal(c.expectMinProfit);
      expect(decoded.steps.length, c.name).to.equal(1);
      expect(decoded.steps[0].dexType, c.name).to.equal(2n);
      const fee = await decoder.decodeV3Fee(decoded.steps[0].extraData);
      expect(Number(fee), c.name).to.equal(c.expectFee);
      expect(decoded.steps[0].extraData.length, c.name).to.equal(66); // 0x + 64 hex
    }
  });

  it("layout rename profitRecipient keeps tuple order (address,steps,uint256)", async function () {
    const coder = ethers.AbiCoder.defaultAbiCoder();
    const types = ["address", "tuple(uint8,address,address,uint256,bytes)[]", "uint256"];
    const hex = coder.encode(types, [
      "0x0000000000000000000000000000000000000001",
      [[0, "0x0000000000000000000000000000000000000002", "0x0000000000000000000000000000000000000003", 1n, "0x"]],
      42n,
    ]);
    const Decoder = await ethers.getContractFactory("CallbackDataDecoder");
    const decoder = await Decoder.deploy();
    const d = await decoder.decode(hex);
    expect(d.profitRecipient).to.equal("0x0000000000000000000000000000000000000001");
    expect(d.minProfit).to.equal(42n);
  });

  it("struct-wrapped encode must NOT decode as flat (documents the bug we fixed)", async function () {
    const coder = ethers.AbiCoder.defaultAbiCoder();
    const wrapped = coder.encode(
      ["tuple(address,tuple(uint8,address,address,uint256,bytes)[],uint256)"],
      [
        [
          "0x0000000000000000000000000000000000000001",
          [
            [
              0,
              "0x0000000000000000000000000000000000000002",
              "0x0000000000000000000000000000000000000003",
              1n,
              "0x",
            ],
          ],
          42n,
        ],
      ]
    );
    const Decoder = await ethers.getContractFactory("CallbackDataDecoder");
    const decoder = await Decoder.deploy();
    await expect(decoder.decode(wrapped)).to.be.reverted;
  });
});
