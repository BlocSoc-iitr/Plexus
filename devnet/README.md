# BAL devnet

A local two-client Glamsterdam devnet that produces real EIP-7928 block access
lists, so Plexus's BAL fetch, cache, normalize, and verify paths can be
exercised against actual client output instead of only against fixtures.

[`network_params.yaml`](network_params.yaml) runs Reth and Nethermind, each
paired with Lighthouse. Those are the two clients Plexus can read a BAL from,
and they cover both wire formats:

| Client | Method | Response |
| --- | --- | --- |
| Reth | `eth_getBlockAccessList` | flat JSON, every field always present |
| Nethermind | `debug_getRawBlockAccessList` | raw RLP hex, no JSON getter |

Nethermind's raw bytes are what make it the correctness anchor: hashing them
locally reproduces the header's `blockAccessListHash` exactly, so the
commitment is checkable end to end without trusting the client's own JSON.

Geth, Erigon, and Besu are deliberately absent — see issue #17 for the full
compatibility matrix and issue #26 for why the scope stops here.

## Prerequisites

- Docker, running
- [Kurtosis CLI](https://docs.kurtosis.com/install)
- `jq` and `curl`, for `refresh-tags.sh`

## 1. Refresh the image tags

**Do this first, every time.** The pins in `network_params.yaml` were the
newest `glamsterdam-devnet-8` builds on 2026-08-21 and they will not stay
current: devnet numbers bump every few weeks, and new commits land under the
same tag within days. A tag that no longer exists fails Kurtosis's label
validation before a single container starts.

```sh
./refresh-tags.sh          # newest tags on the highest devnet found
./refresh-tags.sh 8        # newest tags on glamsterdam-devnet-8 specifically
```

It prints one `ethpandaops/<client>:<tag>` line per client — paste those into
`network_params.yaml`. If it reports that a client has no build for a devnet
yet, that devnet is not usable for a two-client network; fall back to the
previous number. All three images must come from the **same** devnet number.

## 2. Start it

```sh
kurtosis run --enclave plexus-bal-devnet \
  github.com/ethpandaops/ethereum-package \
  --args-file ./network_params.yaml
```

First run pulls several GB of images. The enclave is ready when Kurtosis prints
its service table.

## 3. Find the RPC ports

Kurtosis maps each node's RPC to a random host port, so it changes on every
`kurtosis run`. Never hard-code one.

```sh
kurtosis enclave inspect plexus-bal-devnet
```

The EL services are named after their position and client pair. To get just the
URLs:

```sh
kurtosis port print plexus-bal-devnet el-1-reth-lighthouse rpc
kurtosis port print plexus-bal-devnet el-2-nethermind-lighthouse rpc
```

`dora` runs as an additional service and gives a block explorer over the same
network, which is the quickest way to eyeball whether blocks are being produced.

## 4. Confirm BAL is actually live

Do this before debugging anything else, because a devnet with BAL switched off
looks completely healthy. Every block should carry a non-null
`blockAccessListHash`:

```sh
RETH=$(kurtosis port print plexus-bal-devnet el-1-reth-lighthouse rpc)

curl -s "$RETH" -X POST -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' \
  | jq '.result.blockAccessListHash'
```

`null` means `gloas_fork_epoch` did not take effect — see troubleshooting below.
A hash means BAL is live, and both getters should now return data:

```sh
NETHERMIND=$(kurtosis port print plexus-bal-devnet el-2-nethermind-lighthouse rpc)

curl -s "$RETH" -X POST -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockAccessList","params":["latest"]}' | jq '.'

curl -s "$NETHERMIND" -X POST -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"debug_getRawBlockAccessList","params":["latest"]}' | jq '.'
```

## 5. Run the tests

Live tests are `#[ignore]`d, so `cargo test` and CI never touch them and never
need a devnet. Run them explicitly, with URLs from step 3:

```sh
export PLEXUS_RETH_RPC_URL=$(kurtosis port print plexus-bal-devnet el-1-reth-lighthouse rpc)
export PLEXUS_NETHERMIND_RPC_URL=$(kurtosis port print plexus-bal-devnet el-2-nethermind-lighthouse rpc)

make devnet-test
```

`make devnet-test` is shorthand for:

```sh
cargo test -p parser --test devnet_bal_e2e -- --ignored --nocapture
```

Note the crate in `crates/extractor` is named `parser`, hence `-p parser`.

Three tests run:

| Test | What it covers |
| --- | --- |
| `reth_end_to_end` | fetch → commitment check → `bal.json` cache → cache hit → normalize, via JSON |
| `nethermind_end_to_end` | the same, via raw RLP |
| `clients_agree_on_the_same_block` | both clients decode the same block to byte-identical data and normalize to the same access sets |

Each test only needs the clients it names, so a Reth-only run can set just
`PLEXUS_RETH_RPC_URL` and add a `reth_end_to_end` filter.

**Picking a block.** By default the tests use the newest block that has
transactions, walking back up to 64 blocks from `latest`. Pin one instead with
`PLEXUS_BLOCK`, in hex or decimal:

```sh
PLEXUS_BLOCK=0x63 make devnet-test
```

An idle devnet mines empty blocks, and a BAL over zero transactions would let
most assertions pass vacuously — so rather than quietly testing nothing, the
tests fail with a note to send some traffic or pin a block. Any transaction
will do; the devnet's prefunded accounts are listed in the Kurtosis output.

## 6. Tear it down

```sh
kurtosis enclave rm -f plexus-bal-devnet
```

Leaving an enclave running keeps its containers and volumes alive. Tear down
before re-running with new image tags rather than reusing the enclave name, so
a stale genesis cannot survive into the new network.

## Troubleshooting

**`blockAccessListHash` is null on every block.** `gloas_fork_epoch` is not
active. The ethereum-package defaults it to a "never" sentinel, so BAL stays
inert even on fully BAL-capable builds and the nodes look perfectly healthy
while producing no access lists at all. `network_params.yaml` sets it to `0`;
confirm your edits did not drop it, and that you passed the right `--args-file`.
This was the single biggest blocker in #17.

**A service launch hangs forever with no error.** Almost always
`el_extra_params`. It **replaces** a client's default API list rather than
extending it, and Kurtosis's own orchestration calls `admin_nodeInfo` to wire up
peering — so omitting `admin` from a custom `--http.api` leaves it waiting on a
state that will never arrive. Keep `admin` alongside whatever `eth`/`debug`
namespaces you add.

**A client won't peer, `net_peerCount` stays `0x0`.** Check genesis-hash
agreement *before* suspecting networking. The devp2p handshake includes the
genesis hash, and a mismatch makes clients silently reject each other with
nothing in the logs pointing at the real cause. Compare what each client
computed from the identical `genesis.json`; this is exactly how #17 diagnosed
Erigon, which derived a different hash from the same input and could never join.

**Kurtosis rejects the config before starting containers.** A malformed or
nonexistent image tag fails label validation up front. Re-run
`./refresh-tags.sh` — the pinned commits have almost certainly rotated.

**Mixed devnet numbers.** All participants must run builds from the same
`glamsterdam-devnet-N`. Mixing numbers gives divergent fork configs and lands
you back in the genesis-mismatch case above.
