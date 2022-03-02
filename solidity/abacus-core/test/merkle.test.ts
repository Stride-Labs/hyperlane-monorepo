import { ethers } from 'hardhat';
import { expect } from 'chai';

import { BytesArray } from './lib/types';
import { TestMerkle, TestMerkle__factory } from '../typechain';

const merkleTestCases = require('../../../vectors/merkle.json');

describe('Merkle', async () => {
  for (let testCase of merkleTestCases) {
    const { testName, leaves, expectedRoot, proofs } = testCase;

    describe(testName, async () => {
      let merkle: TestMerkle;

      before(async () => {
        const [signer] = await ethers.getSigners();

        const merkleFactory = new TestMerkle__factory(signer);
        merkle = await merkleFactory.deploy();

        //insert the leaves
        for (let leaf of leaves) {
          const leafHash = ethers.utils.hashMessage(leaf);
          await merkle.insert(leafHash);
        }
      });

      it('returns the correct leaf count', async () => {
        const leafCount = await merkle.count();
        expect(leafCount).to.equal(leaves.length);
      });

      it('produces the proper root', async () => {
        expect(await merkle.root()).to.equal(expectedRoot);
      });

      it("can verify the leaves' proofs", async () => {
        for (let proof of proofs) {
          const { leaf, path, index } = proof;

          const proofRoot = await merkle.branchRoot(
            leaf,
            path as BytesArray,
            index,
          );
          expect(proofRoot).to.equal(expectedRoot);
        }
      });
    });
  }
});