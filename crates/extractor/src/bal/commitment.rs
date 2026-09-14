//! EIP-7928 block access list commitment verification.
//!
//! Every block header commits to its access list through `blockAccessListHash`,
//! defined as the Keccak-256 of the RLP-encoded BAL. Checking a fetched BAL
//! against that commitment before using it turns a bad fetch, a corrupt cache
//! entry, or a client bug into an immediate [`BalError::BalHashMismatch`]
//! instead of silently wrong dependency edges further downstream.
//!
//! # Two functions, two strengths
//!
//! The clients hand us different things, which buys different amounts of trust:
//!
//! - [`verify_raw_bal_commitment`] hashes the exact bytes the node sent. This is
//!   a true integrity check over the wire encoding, and is what Nethermind's
//!   `debug_getRawBlockAccessList` path can use because it carries raw RLP.
//! - [`verify_bal_commitment`] hashes a *re-encoding* of the decoded BAL, which
//!   is all that's possible for Reth's JSON path and for anything read back out
//!   of the `bal.json` cache, since neither keeps the original bytes.
//!
//! The re-encoding check catches dropped fields, reordered accounts, corrupted
//! values, and decoder bugs — the failures that actually matter here. What it
//! cannot catch is a non-canonical encoding that decodes to the same value,
//! because re-encoding normalizes exactly that difference away. Don't read it as
//! a byte-for-byte guarantee about what the node sent.
//!
//! `raw_and_decoded_hashes_agree` in this module's tests is what justifies
//! treating the two as interchangeable for canonically-encoded input.
//!
//! An empty BAL needs no special-casing: RLP-encoding an empty list yields
//! `0xc0`, so it hashes to the spec's `EMPTY_BLOCK_ACCESS_LIST_HASH` sentinel
//! (`0x1dcc4de8…`) on its own. `empty_bal_matches_the_sentinel_observed_on_a_real_node`
//! pins that against a value read off a real node.

use alloy_eip7928::{compute_block_access_list_hash, AccountChanges};
use alloy_primitives::{keccak256, B256};

use crate::bal::error::BalError;

/// Checks a decoded BAL against the block's committed `blockAccessListHash`.
///
/// The BAL is RLP-encoded and hashed, so this reproduces the header's
/// commitment only for a canonically-encoded access list — see the module docs.
///
/// # Errors
///
/// Returns [`BalError::BalHashMismatch`] carrying both hashes if the BAL does
/// not match `expected`.
pub fn verify_bal_commitment(bal: &[AccountChanges], expected: B256) -> Result<(), BalError> {
    ensure_hash(compute_block_access_list_hash(bal), expected)
}

/// Checks raw RLP bytes, as received from the node, against the block's
/// committed `blockAccessListHash`.
///
/// Prefer this over [`verify_bal_commitment`] wherever the original bytes are
/// still in hand: it hashes what the node actually sent rather than a
/// re-encoding of it. `raw` is the decoded byte string, not a hex string.
///
/// # Errors
///
/// Returns [`BalError::BalHashMismatch`] carrying both hashes if the bytes do
/// not match `expected`.
pub fn verify_raw_bal_commitment(raw: &[u8], expected: B256) -> Result<(), BalError> {
    ensure_hash(keccak256(raw), expected)
}

fn ensure_hash(computed: B256, expected: B256) -> Result<(), BalError> {
    if computed == expected {
        Ok(())
    } else {
        Err(BalError::BalHashMismatch { computed, expected })
    }
}

#[cfg(test)]
mod tests {
    use alloy_eip7928::{bal::Bal, EMPTY_BLOCK_ACCESS_LIST_HASH};
    use alloy_primitives::{b256, U256};

    use super::*;

    /// Two accounts across two transactions plus a system write at index 0 —
    /// the shape a real Glamsterdam block produces.
    fn sample_bal() -> Vec<AccountChanges> {
        serde_json::from_value(serde_json::json!([
            {
                "address": "0x000f3df6d732807ef1319fb7b8bb8522d0beac02",
                "storageChanges": [
                    { "slot": "0x2a", "changes": [ { "blockAccessIndex": "0x0", "newValue": "0x99" } ] }
                ],
                "storageReads": [],
                "balanceChanges": [],
                "nonceChanges": [],
                "codeChanges": []
            },
            {
                "address": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "storageChanges": [
                    { "slot": "0x1", "changes": [
                        { "blockAccessIndex": "0x1", "newValue": "0x64" },
                        { "blockAccessIndex": "0x2", "newValue": "0xc8" }
                    ] }
                ],
                "storageReads": ["0x5"],
                "balanceChanges": [ { "blockAccessIndex": "0x1", "postBalance": "0xde0b6b3a7640000" } ],
                "nonceChanges": [ { "blockAccessIndex": "0x1", "newNonce": "0x1" } ],
                "codeChanges": []
            }
        ]))
        .unwrap()
    }

    /// The raw RLP a node would serve for a given BAL.
    fn raw_rlp(bal: &[AccountChanges]) -> Vec<u8> {
        alloy_rlp::encode(Bal::from(bal.to_vec()))
    }

    // ── the matching case ────────────────────────────────────────────────────

    #[test]
    fn matching_bal_passes_verification() {
        let bal = sample_bal();
        let committed = compute_block_access_list_hash(&bal);

        assert!(verify_bal_commitment(&bal, committed).is_ok());
    }

    #[test]
    fn matching_raw_bytes_pass_verification() {
        let bal = sample_bal();
        let raw = raw_rlp(&bal);

        assert!(verify_raw_bal_commitment(&raw, keccak256(&raw)).is_ok());
    }

    // The commitment #17 reproduced by hand: keccak-256 of the raw RLP equals
    // the header's blockAccessListHash. This is what lets the Reth (JSON,
    // re-encoded) and Nethermind (raw bytes) paths be checked against the same
    // header commitment — without it the decoded-BAL check would only be
    // self-consistent, not tied to the wire form.
    #[test]
    fn raw_and_decoded_hashes_agree() {
        let bal = sample_bal();

        assert_eq!(
            compute_block_access_list_hash(&bal),
            keccak256(raw_rlp(&bal))
        );
    }

    // ── the corrupted / mismatched case ──────────────────────────────────────
    //
    // Both verify functions end in the same `computed == expected` comparison,
    // and keccak is indifferent to *how* two inputs differ — a flipped byte, a
    // truncation, a dropped account and a BAL from another block all reach it
    // identically. So this is one case per function, not one per way to corrupt.

    #[test]
    fn corrupted_raw_bytes_are_caught() {
        let bal = sample_bal();
        let committed = keccak256(raw_rlp(&bal));

        let mut corrupted = raw_rlp(&bal);
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0xff;

        let err = verify_raw_bal_commitment(&corrupted, committed).unwrap_err();

        assert!(matches!(err, BalError::BalHashMismatch { .. }));
    }

    // A BAL that still decodes cleanly but carries a wrong value is the failure
    // this whole check exists for — nothing else downstream would notice.
    #[test]
    fn mutated_storage_value_is_caught() {
        let bal = sample_bal();
        let committed = compute_block_access_list_hash(&bal);

        let mut tampered = bal.clone();
        tampered[1].storage_changes[0].changes[0].new_value = U256::from(0xdead_u64);

        let err = verify_bal_commitment(&tampered, committed).unwrap_err();

        assert!(matches!(err, BalError::BalHashMismatch { .. }));
    }

    // The error is the operator-facing signal, so it has to carry both hashes as
    // data and render both in its message.
    #[test]
    fn mismatch_error_reports_both_hashes() {
        let bal = sample_bal();
        let expected = B256::from([0x11; 32]);

        let err = verify_bal_commitment(&bal, expected).unwrap_err();
        let msg = err.to_string();

        match err {
            BalError::BalHashMismatch {
                computed,
                expected: got,
            } => {
                assert_eq!(got, expected);
                assert_eq!(computed, compute_block_access_list_hash(&bal));
            }
            other => panic!("expected BalHashMismatch, got {other:?}"),
        }
        assert!(msg.contains(&compute_block_access_list_hash(&bal).to_string()));
        assert!(msg.contains(&expected.to_string()));
    }

    // ── the empty-BAL case from #17 ──────────────────────────────────────────

    /// The value #17 read off Erigon's genesis block on a real devnet node.
    ///
    /// Pinned as a literal rather than reused from `EMPTY_BLOCK_ACCESS_LIST_HASH`
    /// on purpose: comparing our computation against alloy's constant only proves
    /// the two agree with each other. This ties both of them to what a node
    /// actually reported, so the check still holds if that constant ever moves.
    const OBSERVED_EMPTY_BAL_HASH: B256 =
        b256!("0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347");

    #[test]
    fn empty_bal_matches_the_sentinel_observed_on_a_real_node() {
        // no special-casing: RLP-encoding an empty list gives 0xc0, and the
        // sentinel is simply its hash
        assert_eq!(raw_rlp(&[]), vec![0xc0]);
        assert_eq!(compute_block_access_list_hash(&[]), OBSERVED_EMPTY_BAL_HASH);
        assert_eq!(EMPTY_BLOCK_ACCESS_LIST_HASH, OBSERVED_EMPTY_BAL_HASH);
    }
}
