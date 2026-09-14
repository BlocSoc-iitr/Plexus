//! Block access list fetching for Nethermind nodes.
//!
//! Nethermind exposes EIP-7928 data only through `debug_getRawBlockAccessList`,
//! which returns the raw RLP as a hex string rather than JSON. Decoding it with
//! `alloy_eip7928` lands on the same `Vec<AccountChanges>` the Reth path
//! produces, so everything downstream ([`normalize_bal`](crate::bal::normalize_bal)
//! and the index classification) is shared between the two clients.

use alloy_eip7928::bal::DecodedBal;
use alloy_eip7928::AccountChanges;
use alloy_primitives::{Bytes, B256};

use crate::bal::commitment::verify_raw_bal_commitment;
use crate::bal::error::BalError;
use crate::fetcher::BlockId;
use crate::rpc::client::RpcClient;

/// Fetches a block's access list from a Nethermind node.
///
/// Nethermind serves the BAL as raw RLP hex from `debug_getRawBlockAccessList`;
/// this fetches those bytes and decodes them into the client-agnostic
/// `alloy_eip7928` types. A `null` result means the node has no such block.
///
/// Pass the block header's `blockAccessListHash` as `expected` to have the
/// response checked against the block's commitment. Because this path still
/// holds the bytes the node sent, that check is over the wire encoding itself
/// rather than a re-encoding of it — the strongest form available. `None` skips
/// verification, for blocks whose header carries no commitment.
pub async fn fetch_nethermind_bal(
    client: &RpcClient,
    block_id: &BlockId,
    expected: Option<B256>,
) -> Result<Vec<AccountChanges>, BalError> {
    let raw: Option<String> = client
        .request("debug_getRawBlockAccessList", (block_id.as_rpc_param(),))
        .await?;

    let raw = raw.ok_or_else(|| BalError::BlockNotFound(block_id.clone()))?;

    decode_raw_bal_verified(&raw, expected)
}

/// Decodes a `0x`-prefixed raw RLP block access list, first checking it against
/// a block commitment. Pass `None` for `expected` to decode without verifying.
///
/// Split out from the fetch so the RLP decoding can be tested without a node and
/// reused wherever raw BAL bytes need decoding. The `0x` prefix is optional.
///
/// Verification runs on the raw bytes *before* decoding, so a BAL belonging to
/// another block is rejected as a mismatch rather than surfacing as whatever
/// unrelated decode error its contents happen to produce.
///
/// # Errors
///
/// Returns [`BalError::BalHashMismatch`] if `expected` is `Some` and the bytes
/// don't hash to it, or the usual hex/RLP errors otherwise.
pub fn decode_raw_bal_verified(
    raw_hex: &str,
    expected: Option<B256>,
) -> Result<Vec<AccountChanges>, BalError> {
    let stripped = raw_hex.strip_prefix("0x").unwrap_or(raw_hex);
    let bytes = hex::decode(stripped)?;

    if let Some(expected) = expected {
        verify_raw_bal_commitment(&bytes, expected)?;
    }

    let decoded = DecodedBal::from_rlp_bytes(Bytes::from(bytes))?;
    Ok(decoded.split().0.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eip7928::bal::Bal;
    use serde_json::Value;
    use wiremock::matchers::method as http_method;
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn rpc_ok_body(req: &Request, result: Value) -> Value {
        let id = serde_json::from_slice::<Value>(&req.body)
            .ok()
            .and_then(|v| v.get("id").cloned())
            .unwrap_or(serde_json::json!(0));
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
    }

    fn sample_accounts() -> Vec<AccountChanges> {
        serde_json::from_value(serde_json::json!([
            {
                "address": "0x1111111111111111111111111111111111111111",
                "storageChanges": [
                    { "slot": "0x1", "changes": [ { "blockAccessIndex": "0x1", "newValue": "0x2a" } ] }
                ],
                "storageReads": ["0x7"],
                "balanceChanges": [ { "blockAccessIndex": "0x1", "postBalance": "0xde0b6b3a7640000" } ],
                "nonceChanges": [],
                "codeChanges": []
            }
        ]))
        .unwrap()
    }

    fn rlp_hex(accounts: &[AccountChanges]) -> String {
        let bal = Bal::from(accounts.to_vec());
        format!("0x{}", hex::encode(alloy_rlp::encode(&bal)))
    }

    async fn server_returning(result: Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .respond_with(move |req: &Request| {
                ResponseTemplate::new(200).set_body_json(rpc_ok_body(req, result.clone()))
            })
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn fetches_and_decodes_a_nethermind_bal() {
        let server = server_returning(Value::String(rlp_hex(&sample_accounts()))).await;
        let client = RpcClient::new(server.uri()).unwrap();

        let bal = fetch_nethermind_bal(&client, &BlockId::Number(100), None)
            .await
            .unwrap();

        assert_eq!(bal.len(), 1);
        assert_eq!(bal[0].storage_changes[0].changes[0].block_access_index, 1);
        assert_eq!(bal[0].storage_reads.len(), 1);
    }

    #[tokio::test]
    async fn empty_bal_decodes_to_an_empty_list() {
        let server = server_returning(Value::String(rlp_hex(&[]))).await;
        let client = RpcClient::new(server.uri()).unwrap();

        let bal = fetch_nethermind_bal(&client, &BlockId::Number(100), None)
            .await
            .unwrap();

        assert!(bal.is_empty());
    }

    #[tokio::test]
    async fn null_result_is_block_not_found() {
        let server = server_returning(Value::Null).await;
        let client = RpcClient::new(server.uri()).unwrap();

        let err = fetch_nethermind_bal(&client, &BlockId::Number(100), None)
            .await
            .unwrap_err();

        assert!(matches!(err, BalError::BlockNotFound(BlockId::Number(100))));
    }

    #[tokio::test]
    async fn invalid_hex_is_reported() {
        let server = server_returning(Value::String("0xnothex".into())).await;
        let client = RpcClient::new(server.uri()).unwrap();

        let err = fetch_nethermind_bal(&client, &BlockId::Number(100), None)
            .await
            .unwrap_err();

        assert!(matches!(err, BalError::BalHex(_)));
    }

    #[tokio::test]
    async fn invalid_rlp_is_reported() {
        // 0x80 is an empty string, not the list the BAL wrapper expects
        let server = server_returning(Value::String("0x80".into())).await;
        let client = RpcClient::new(server.uri()).unwrap();

        let err = fetch_nethermind_bal(&client, &BlockId::Number(100), None)
            .await
            .unwrap_err();

        assert!(matches!(err, BalError::BalRlp(_)));
    }

    // ── commitment verification on the raw-bytes path ────────────────────────

    fn commitment_of(accounts: &[AccountChanges]) -> B256 {
        alloy_primitives::keccak256(alloy_rlp::encode(Bal::from(accounts.to_vec())))
    }

    #[tokio::test]
    async fn bal_matching_the_commitment_is_returned() {
        let accounts = sample_accounts();
        let server = server_returning(Value::String(rlp_hex(&accounts))).await;
        let client = RpcClient::new(server.uri()).unwrap();

        let bal = fetch_nethermind_bal(
            &client,
            &BlockId::Number(100),
            Some(commitment_of(&accounts)),
        )
        .await
        .unwrap();

        assert_eq!(bal, accounts);
    }

    #[tokio::test]
    async fn bal_not_matching_the_commitment_is_rejected() {
        let server = server_returning(Value::String(rlp_hex(&sample_accounts()))).await;
        let client = RpcClient::new(server.uri()).unwrap();

        let err =
            fetch_nethermind_bal(&client, &BlockId::Number(100), Some(B256::from([0x11; 32])))
                .await
                .unwrap_err();

        assert!(matches!(err, BalError::BalHashMismatch { .. }));
    }

    // Verification runs before RLP decoding, so a payload that is both wrong for
    // this block and undecodable reports the mismatch — the actionable cause —
    // rather than an RLP error that says nothing about which block it came from.
    #[test]
    fn mismatch_is_reported_ahead_of_an_rlp_error() {
        let err = decode_raw_bal_verified("0x80", Some(B256::from([0x11; 32]))).unwrap_err();

        assert!(matches!(err, BalError::BalHashMismatch { .. }));
    }

    #[tokio::test]
    async fn tag_is_passed_through_as_an_rpc_param() {
        let server = server_returning(Value::String(rlp_hex(&sample_accounts()))).await;
        let client = RpcClient::new(server.uri()).unwrap();

        fetch_nethermind_bal(&client, &BlockId::Tag("latest".into()), None)
            .await
            .unwrap();

        let body =
            serde_json::from_slice::<Value>(&server.received_requests().await.unwrap()[0].body)
                .unwrap();
        assert_eq!(body["method"], "debug_getRawBlockAccessList");
        assert_eq!(body["params"][0], "latest");
    }
}
