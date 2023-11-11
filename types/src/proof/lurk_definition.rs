use anyhow::{bail, ensure, format_err, Context, Result};
use aptos_crypto::{
    hash::{CryptoHash, CryptoHasher, TestOnlyHasher},
    HashValue,
};
use serde::{Deserialize, Serialize};
use std::marker::PhantomData;

#[cfg(any(test, feature = "fuzzing"))]
pub type TestMerkleLurkProof = LurkMerkleProof<TestOnlyHasher>;
// what is TestOnlyHasher

// how does the fuzzing works in this test suite?

// TBD
/// This is the LurkMerkleProof struct. It is used to authenticate an element in an accumulator given trusted root hash.
/// It contains a vector of siblings and a PhantomData type.
/// The siblings are all siblings in this proof, including the default ones. Siblings are ordered from the bottom level to the root level.
/// The PhantomData is a zero-sized type used to mark things that "act like" they own a T.
#[derive(Clone, Serialize, Deserialize)]
pub struct LurkMerkleProof<H> {
    siblings: Vec<HashValue>,
    phantom: PhantomData<H>,
}

impl<H: CryptoHasher> LurkMerkleProof<H> {
    pub fn new(siblings: Vec<HashValue>) -> Self {
        Self {
            siblings,
            phantom: PhantomData,
        }
    }

    pub fn siblings(&self) -> &[HashValue] {
        &self.siblings
    }

    pub fn verify(
        &self,
        expected_root_hash: HashValue,
        element_hash: HashValue,
        element_index: u64,
    ) -> Result<()> {
        // ensure!(
        // Here we would call the Lurk ZK engine to produce the verification
        Ok(())
    }
}

// TBD
impl<H> PartialEq for LurkMerkleProof<H> {
    fn eq(&self, other: &Self) -> bool {
        self.siblings == other.siblings
    }
}

// TBD
impl<H> Eq for LurkMerkleProof<H> {}

// #[cfg(any(test, feature = "fuzzing"))]
// pub type TestLurkAccumulatorProof = AccumulatorProof<TestOnlyHasher>;

impl<H> std::fmt::Debug for LurkMerkleProof<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AccumulatorProof {{ siblings: {:?} }}", self.siblings)
    }
}
