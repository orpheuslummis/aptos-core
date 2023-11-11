use crate::proof::{lurk_definition::TestMerkleLurkProof, TestAccumulatorInternalNode};
use aptos_crypto::hash::{CryptoHash, TestOnlyHash, ACCUMULATOR_PLACEHOLDER_HASH};

#[test]
fn test_verify_empty_accumulator() {
    let element_hash = b"hello".test_only_hash();
    let root_hash = *ACCUMULATOR_PLACEHOLDER_HASH;
    let proof = TestMerkleLurkProof::new(vec![]);
    assert!(proof.verify(root_hash, element_hash, 0).is_err());
}

#[test]
fn test_verify_single_element_accumulator() {
    let element_hash = b"hello".test_only_hash();
    let root_hash = element_hash;
    let proof = TestMerkleLurkProof::new(vec![]);
    assert!(proof.verify(root_hash, element_hash, 0).is_ok());
}

#[test]
fn test_verify_two_element_accumulator() {
    let element0_hash = b"hello".test_only_hash();
    let element1_hash = b"world".test_only_hash();
    let root_hash = TestAccumulatorInternalNode::new(element0_hash, element1_hash).hash();

    assert!(TestMerkleLurkProof::new(vec![element1_hash])
        .verify(root_hash, element0_hash, 0)
        .is_ok());
    assert!(TestMerkleLurkProof::new(vec![element0_hash])
        .verify(root_hash, element1_hash, 1)
        .is_ok());
}

#[test]
fn test_verify_three_element_accumulator() {
    let element0_hash = b"hello".test_only_hash();
    let element1_hash = b"world".test_only_hash();
    let element2_hash = b"!".test_only_hash();
    let internal0_hash = TestAccumulatorInternalNode::new(element0_hash, element1_hash).hash();
    let internal1_hash =
        TestAccumulatorInternalNode::new(element2_hash, *ACCUMULATOR_PLACEHOLDER_HASH).hash();
    let root_hash = TestAccumulatorInternalNode::new(internal0_hash, internal1_hash).hash();

    assert!(
        TestMerkleLurkProof::new(vec![element1_hash, internal1_hash])
            .verify(root_hash, element0_hash, 0)
            .is_ok()
    );
    assert!(
        TestMerkleLurkProof::new(vec![element0_hash, internal1_hash])
            .verify(root_hash, element1_hash, 1)
            .is_ok()
    );
    assert!(
        TestMerkleLurkProof::new(vec![*ACCUMULATOR_PLACEHOLDER_HASH, internal0_hash])
            .verify(root_hash, element2_hash, 2)
            .is_ok()
    );
}
