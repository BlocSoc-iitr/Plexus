use alloy_eip7928::AccountChanges;
use alloy_primitives::B256;
use serde_json::Value;

use crate::bal::commitment::verify_bal_commitment;
use crate::bal::error::BalError;
use crate::fetcher::BlockId;
use crate::rpc::client::RpcClient;

/// Fetches a block's access list from a Reth node.
///
/// Reth serves EIP-7928 data as JSON matching the `alloy_eip7928` types, so the
/// response decodes without a client-specific representation of its own.
///
/// Pass the block header's `blockAccessListHash` as `expected` to have the
/// response checked against the block's commitment. Reth's JSON path does not
/// carry the bytes the node sent, so that check is over a re-encoding of the
/// decoded response rather than the wire form — weaker than the raw-bytes check
/// [`fetch_nethermind_bal`](crate::bal::fetch_nethermind_bal) can do, and the
/// module docs on [`commitment`](crate::bal::commitment) spell out the
/// difference. `None` skips verification, for blocks whose header carries no
/// commitment.
pub async fn fetch_reth_bal(
    client: &RpcClient,
    block_id: &BlockId,
    expected: Option<B256>,
) -> Result<Vec<AccountChanges>, BalError> {
    let raw: Option<Value> = client
        .request("eth_getBlockAccessList", (block_id.as_rpc_param(),))
        .await?;

    let raw = raw.ok_or_else(|| BalError::BlockNotFound(block_id.clone()))?;

    let bal: Vec<AccountChanges> =
        serde_json::from_value(raw).map_err(|_| BalError::MalformedResponse {
            field: "blockAccessList",
        })?;

    if let Some(expected) = expected {
        verify_bal_commitment(&bal, expected)?;
    }

    Ok(bal)
}

#[cfg(test)]
mod tests {
    use alloy_eip7928::compute_block_access_list_hash;

    use super::*;
    use wiremock::matchers::method as http_method;
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn rpc_ok_body(req: &Request, result: Value) -> Value {
        let id = serde_json::from_slice::<Value>(&req.body)
            .ok()
            .and_then(|v| v.get("id").cloned())
            .unwrap_or(serde_json::json!(0));
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
    }

    fn sample_bal_json() -> Value {
        serde_json::json!([
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
        ])
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
    async fn fetches_and_decodes_a_reth_bal() {
        let server = server_returning(sample_bal_json()).await;
        let client = RpcClient::new(server.uri()).unwrap();

        let bal = fetch_reth_bal(&client, &BlockId::Number(100), None)
            .await
            .unwrap();

        assert_eq!(bal.len(), 1);
        assert_eq!(bal[0].storage_changes[0].changes[0].block_access_index, 1);
        assert_eq!(bal[0].storage_reads.len(), 1);
    }

    #[tokio::test]
    async fn empty_bal_decodes_to_an_empty_list() {
        let server = server_returning(serde_json::json!([])).await;
        let client = RpcClient::new(server.uri()).unwrap();

        let bal = fetch_reth_bal(&client, &BlockId::Number(100), None)
            .await
            .unwrap();

        assert!(bal.is_empty());
    }

    #[tokio::test]
    async fn null_result_is_block_not_found() {
        let server = server_returning(Value::Null).await;
        let client = RpcClient::new(server.uri()).unwrap();

        let err = fetch_reth_bal(&client, &BlockId::Number(100), None)
            .await
            .unwrap_err();

        assert!(matches!(err, BalError::BlockNotFound(BlockId::Number(100))));
    }

    #[tokio::test]
    async fn malformed_result_reports_the_field() {
        let server = server_returning(serde_json::json!([{ "address": "not-an-address" }])).await;
        let client = RpcClient::new(server.uri()).unwrap();

        let err = fetch_reth_bal(&client, &BlockId::Number(100), None)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            BalError::MalformedResponse {
                field: "blockAccessList"
            }
        ));
    }

    // Verification now lives in the fetch, so these cover it where it runs
    // rather than only through `fetch_bal`.

    #[tokio::test]
    async fn bal_matching_the_commitment_is_returned() {
        let server = server_returning(sample_bal_json()).await;
        let client = RpcClient::new(server.uri()).unwrap();

        let accounts: Vec<AccountChanges> = serde_json::from_value(sample_bal_json()).unwrap();
        let expected = compute_block_access_list_hash(&accounts);

        let bal = fetch_reth_bal(&client, &BlockId::Number(100), Some(expected))
            .await
            .unwrap();

        assert_eq!(bal, accounts);
    }

    #[tokio::test]
    async fn bal_not_matching_the_commitment_is_rejected() {
        let server = server_returning(sample_bal_json()).await;
        let client = RpcClient::new(server.uri()).unwrap();

        let err = fetch_reth_bal(&client, &BlockId::Number(100), Some(B256::from([0x11; 32])))
            .await
            .unwrap_err();

        assert!(matches!(err, BalError::BalHashMismatch { .. }));
    }

    #[tokio::test]
    async fn tag_is_passed_through_as_an_rpc_param() {
        let server = server_returning(sample_bal_json()).await;
        let client = RpcClient::new(server.uri()).unwrap();

        fetch_reth_bal(&client, &BlockId::Tag("latest".into()), None)
            .await
            .unwrap();

        let body =
            serde_json::from_slice::<Value>(&server.received_requests().await.unwrap()[0].body)
                .unwrap();
        assert_eq!(body["method"], "eth_getBlockAccessList");
        assert_eq!(body["params"][0], "latest");
    }
}
