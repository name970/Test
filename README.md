# Plauthium workspace: core + Public Testnet v1

Rust workspace for the Plauthium post-quantum layer-1. The six core crates are a frozen,
tested mathematical core that follows the *Plauthium Technical Specification*. Three
testnet crates wrap it into a multi-node P2P network, as specified in
[`../testnet_blueprint.md`](../testnet_blueprint.md). `unsafe` is forbidden everywhere,
and every public item is documented.

```sh
cargo test --workspace --release   # 88 tests, including the 3-node integration test (~2-3 min)
cargo run -p poc                   # core: mock block transition + ASCII summary
scripts/testnet.sh                 # join the public testnet, or start a local 3-node testnet
```

Run the tests in release mode. Every block involves at least one real 512×512 tile product.
The workspace needs Rust 1.89 or newer.

## Crates

| Crate        | Spec      | Contents |
|--------------|-----------|----------|
| `primitives` | §1.3–1.4, Table 3, §7.10 | `Atoms` (1 PLAU = 10^8 atoms), supply `M = 1.5·10^15`, `Mersenne61` field (`p = 2^61 − 1`), `DomainSeparatedHash` (SHA3-512) + `Shake256Xof` (SHAKE256), `BlockResourceVector` (5 components), PQ object sizes |
| `work`       | §3.1–3.2, §5.1–5.2 | 512×512 INT8 `WorkUnit` over `[-127, 127]`, INT32 `AccumulationTarget` with `\|z\| ≤ 8,258,048 < 2^23`, exact tile product, 16-field Fresh Work context, seed expansion by byte rejection, `verify_freivalds_rounds` (`r_F = 6`) |
| `scivm`      | §3.6      | `OzakiIIProfile` (pairwise-coprime moduli ≤ 255, `Q > 2·B_rec`), `split_residues`, centered CRT, exact residue dot product, zero consensus weight |
| `scheduler`  | §6.1–6.5  | `ShareID`, sequential D'Hondt (`w_s/(a_s+1)` by exact cross-multiplication, ties to least ShareID), owner capacity, persistent cohort counters |
| `ledger`     | §7.1–7.8  | 8 disjoint `StateCategory` balances, atomic zero-sum deltas, `FounderVesting` (1,460 daily tranches × 720 blocks), `FeeLot` with ceiling budgets, floor payouts and segregated dust |
| `poc`        | —         | Binary that runs Genesis → Fresh Work → D'Hondt → Base Fee lock/maturation/payout |
| `wire`       | blueprint §3–§5 | The six P2P messages (incl. `GossipShare`; the `Handshake` carries the genesis id and a node nonce), block/job/share types, `Digest`/`Target`, fixed-width ML-DSA-87 `PublicKey`/`Signature`, the bincode 1.3.3 codec (fixed-width little-endian, trailing bytes rejected), protocol constants |
| `chain`      | blueprint §6–§7 | Genesis validation, Stage A (signatures, then VerifyFW by recomputing the tile) and Stage B (context checks), windowed retarget, `apply_body` (the state transition on `ledger::Ledger`), D'Hondt leases, templates, `BlockTree` with heaviest-seal-weight fork choice, ML-DSA-87 owner keys (`chain::keys`) |
| `node`       | blueprint §8–§10 | `plauthium-node`: ChainState / Mempool actors, multi-threaded ConsensusEngine, length-delimited TCP PeerManager with sync, append-only `blocks.log`, persistent miner key, share pool with `GossipShare` relay, Stage A limits per source, host-name bootnodes, axum JSON RPC and the embedded explorer |

## Running a node

`bash plauthium_all_in_one.sh` writes the workspace, builds `plauthium-node` and asks
which of three to run. `scripts/testnet.sh` does the same from a checkout, and
`--join`, `--verify` and `--local` choose without asking (also in the bundle):

- **a) `--join`**: one node on Public Testnet v1 that mines once it is connected and
  caught up (`--mine-when-synced`).
- **b) `--verify`**: the same node without mining (`--no-mine`): it syncs, verifies and
  relays.
- **c) `--local`**: a local dev testnet of three fully meshed mining nodes with a fresh
  genesis (`--seal-attempts 128 --target-block-secs 5 --blocks-per-epoch 12`) and
  explorers at <http://127.0.0.1:8081/>, <http://127.0.0.1:8082/> and
  <http://127.0.0.1:8083/>. Node `i` peers on `127.0.0.1:3034i` and keeps its data in
  `target/testnet/n<i>`; every run resets the chain data.

Every node creates its ML-DSA-87 miner key on its first run and loads it on every later
run; the launcher shows the key file and owner id before starting
(`plauthium-node key --data-dir DIR` does the same by hand). `--reset` wipes the chain
data but keeps the key. Ctrl-C stops the nodes gracefully.

Each explorer shows the 8 ledger categories with a live `Σ = M` check, the block feed and a
job-submission form. Jobs become fundable once the first epoch reward is paid into `U`, at
height `2 · blocks_per_epoch` (24 here). Before that, every atom of `U` is a locked founder
tranche.

To submit a commercial job from the command line (fees default to the WP 7.17 budget; the
node signs it with its miner key):

```sh
curl -s localhost:8081/api/submit_job -H 'content-type: application/json' \
  -d "{\"payload\":{\"type\":\"exact_dot\",\"xs\":[$(seq -s, 1 512)],\"ys\":[$(seq -s, 512 -1 1)]}}"
```

Other endpoints: `GET /api/status` and `GET /api/blocks?limit=20&before=<height>`. Run
`plauthium-node --help`, `plauthium-node genesis --help` and `plauthium-node run --help` for
the CLI.

## Public Testnet v1

All nodes built from this source share one genesis: `node/genesis.testnet.json`,
embedded in the binary, with `genesis_time_bucket` fixed at `1792540800`
(2026-10-21T00:00:00Z, the v1 launch). Its genesis id is

```text
2804064dac4e30a4cb47aa0d87c4da05427c6bcdb8d1d5aa45a1fd69c3677d5cd8d441d3beb877317277096139b6205287472d078dedea8e525be60ee5f5114e
```

and `node/tests/public_testnet.rs` fails if it changes. Retargeting never reads the
genesis time (its windows start at height `2 · window`), so a genesis in the past is
safe. `WIRE_VERSION` 4 puts the genesis id in the `Handshake`: a node on another genesis
(a local dev chain shares the `chain_id`) is not kept as a peer, and its dialer backs off
for 10 minutes.

Modes a and b dial the bootnodes `seed1.plauthium.network:30333` and
`seed2.plauthium.network:30333`. `--peer` takes `host:port` as well as `IP:port`, and
names are resolved again before every connection attempt. The node's key is
`~/.local/share/plauthium/miner_key.bin` and its chain data
`~/.local/share/plauthium/testnet-v1` (under `$XDG_DATA_HOME` if set), outside the
workspace, so `cargo clean` or a fresh copy of the bundle keeps the identity. A node locks
its data directory, so a second node on it refuses to start. Environment variables override the defaults:
`PLAUTHIUM_BOOTNODES`, `PLAUTHIUM_HOME`, `PLAUTHIUM_P2P_LISTEN` (default
`0.0.0.0:30333`; forward the port to accept inbound peers), `PLAUTHIUM_RPC_LISTEN`
(default `127.0.0.1:8080`; keep it local, since `/api/submit_job` pays with the miner
key), `PLAUTHIUM_MINING_THREADS` and `PLAUTHIUM_RESET=1`.

`--mine-when-synced` keeps a node that cannot reach the network from mining a private
fork it could never leave: it mines only while a peer is connected, once its tip has
reached the best height its peers report, or once its tip has not moved for a minute
while connected. A peer's claimed height drops to what it delivers when a sync request to
it goes unanswered, or its answer is empty, starts at another height or brings nothing
new; gossip raises it to at most one block past our tip. So a peer cannot hold the gate
closed by claiming a height it does not have. A node with peers configured warns every minute while none is
connected.

**Bootnode operators.** The seed names must resolve to the bootnodes before the bundle is
published. A bootnode is mode a with its own name left out: the first one runs with
`PLAUTHIUM_BOOTNODES=` (empty, so it mines alone from genesis), and every other one lists
the bootnodes already running. A bootnode that does list itself drops the connection to
itself (the `Handshake` nonce), even behind 1:1 NAT.

## Verification limits (v1)

Stage B (context checks) needs no work, so a peer could otherwise make a node recompute
tiles for free. Limits apply per **source**: an IP address, the /64 prefix for IPv6, or
the full address for loopback peers (local testnets). Bans and the inbound cap of 16
connections are per source too.

- **Stage A slots:** at most 2 blocks of a source in Stage A, 16 in total. A block that
  passes Stage B beyond that waits in a FIFO queue: at most 8 per source and 64 in total.
  A newly arrived block over these limits gets `Busy`: not remembered, nobody penalized,
  and net asks that peer again 2 s later. When the queue is full, the newest entry of the
  source holding the most (clearly more than the newcomer's) makes room. Parked children
  and deferred blocks released for verification are not refused.
- **Order:** signatures, then the header's tile, then the share tiles. A block's share
  tiles are only recomputed once its seal is known to be genuine work.
- **Cancellation:** when a block or share of a source fails verification, the chain actor
  bans the source on the spot: its other Stage A tasks are aborted (a tile already
  computing finishes, as it cannot be interrupted, and its result is discarded) and its
  queued, parked and deferred blocks, share checks and parked shares are dropped. Nothing
  new from it is admitted until the ban ends; net closes its connections and sends the
  same cancellation for bans of its own (protocol violations). Semaphore permits stay with
  the computation, so aborted work never lets more tiles run than `--verify-threads`.
- **Second senders:** each pending block remembers up to 3 other sources that delivered
  the same bytes. A block whose first sender is banned stays pending (a running Stage A
  goes on) for one of them, since honest peers relay a block only once.
- **Settled only:** net remembers a block or share as seen only once it is accepted or
  rejected, so copies of pending ones still reach the chain actor (a cheap duplicate
  check, which records the second sender).
- **Parked orphans** (parent not yet known; they cost nothing to make): at most 64 per
  source, a full sync batch, and 256 in total. A source over its share evicts its own
  oldest orphan; a full set evicts the oldest of the source holding the most.
- **Deferred blocks** (time bucket ahead of the local clock): at most 8 per source and 64
  in total, and none more than 75 s ahead. Orphans released by their parent count too.
- **Errors** of the verification machinery itself (a panicked or cancelled task) drop the
  block or share without penalizing anyone.
- **Jobs:** the mempool also refuses a copy of a job that is already on the canonical
  chain (until it expires), so replaying old jobs cannot fill it.

The RPC reports these counters under `verification` in `GET /api/status`.

## Owner identity (v1)

An owner is an ML-DSA-87 key pair (FIPS 204, RustCrypto `ml-dsa`), stored as its 32-byte
seed. Its **key id** is `H(PLAU/TESTNET/OWNER/v1, public key)`.

- **On the wire:** `BlockHeader.miner_key`, `ShareProof.owner_key` and
  `JobTransaction.client_key` carry the 2,592-byte public key. Each object ends with a
  4,627-byte `signature` over a domain-separated digest of all its other fields:
  `PLAU/TESTNET/SIG/{HEADER,SHARE,JOB}/v1`. The job digest also binds `genesis_id`, and
  every signature uses the FIPS 204 context string `PLAU/TX/v2`.
- **Bound to the work:** context fields 11 and 12 are `H(PLAU/TESTNET/{MINER,PAYOUT}/v2,
  key id)`. Re-signing someone else's block or share under another key breaks Stage B.
- **Verified before the STF:** Stage A checks every signature before any tile is
  recomputed. `apply_body` only accepts a `VerifiedOwner`, which `chain::core_adapter` alone
  can create, after a successful verification.
- **Bridged to the frozen core:** the scheduler's `OwnerId(u64)` is the first 8 bytes of
  the key id. It only bounds owner capacity. Rewards (`OwnerReward`), leases and receipts
  carry the full 512-bit key id.
- **Stable job identity:** `tx_id` hashes the job without its signature, so re-signing never
  changes a job's identity.
- **Relay ids:** `block_id` and `tx_id` leave out signatures, so anyone can forge an invalid
  copy that shares an honest object's id. The node's relay and rejection caches therefore
  key on a hash of the full signed encoding (`chain::block_relay_id`, `chain::job_relay_id`),
  and a forged copy cannot shadow the genuine block or job. A block's relay id covers its
  body as well, so a genuine header carrying a forged body cannot shadow the block either.

`crypto_suite_id` is 1 (SHA3-512 + SHAKE256 + ML-DSA-87).

## Share gossip (v1)

`WIRE_VERSION` 3 adds `GossipShare(ShareProof)`. Every node keeps a share pool (`node::share_pool`)
of verified shares, its own and gossiped ones, one per `work_id`. Templates take up to
`max_shares_per_block` of them, whoever mined them, oldest height first, and emit them sorted by
`work_id`, so a share counts for rewards and leases even if its owner never seals a block.

A gossiped share enters the pool, and is relayed onward, only after every check passes,
cheapest first:

1. Dedupe: by relay id (`chain::share_relay_id`, the full signed encoding), by `work_id`
   against the pool (work-checked shares only), and by (owner key id, `work_id`) against
   shares still awaiting their work check. A slot is only taken once the owner signature
   verified, so a forged copy, or a claim to another owner's `work_id`, cannot shadow the
   genuine share, and re-signed copies (ML-DSA signing is randomized) cost nothing.
2. No-work checks (`BlockTree::check_gossip_share`): context constants, owner binding,
   height, anchor and target on the share's own branch, inclusion window, not yet
   registered.
3. Limits on shares awaiting their work check: 8 per source, 64 in total.
4. The ML-DSA-87 signature, checked on the spot.
5. Its tile, recomputed off the chain actor on a separate, smaller verification budget,
   so a block's Stage A never queues behind share gossip.

A share whose parent block is in Stage A waits for it (signature already checked, counted
in the limits), and is dropped if that block fails. Such a share, a stale one and one whose
parent is unknown are dropped without penalty and not remembered as seen, so a later relay
can still deliver them. Useless shares (ignored for any reason, including the limits above,
or duplicates under a new encoding) are budgeted at 256 a minute per peer; past that, the
peer's shares are dropped unread until the minute is up. The peer is throttled, not banned,
since an honest peer that is syncing sends many such shares. A peer that sends an invalid
share is banned, even if it disconnected first, and a ban closes every connection from its
source (see [Verification limits](#verification-limits-v1)). A dialer backs off unless the
connection lasts a minute, so a peer that drops it at once is not redialed every second.

Templates include only fully verified shares, and `validate_genesis` requires every block
to fit a full set of `max_shares_per_block` shares plus 16 KiB of headroom. The pool is
pruned when shares leave the inclusion window or are registered on the canonical chain,
and the shares of abandoned blocks return to it on a reorg.

## Known gaps (blueprint §1.2)

Gaps 1 (placeholder hashing), 2 (self-declared integer owners, no signatures) and 4 (shares
not gossiped) are closed in v1. The numbering of the remaining gaps follows the blueprint.

- **Shared faucet.** The core ledger has no per-account balances (see
  `ledger/src/lib.rs` lines 18–21), so a job's signature authenticates its client, but its
  deposit is debited from the aggregate `U`.

3. **VerifyFW by recomputation.** Validators recompute every claimed tile instead of
   verifying a Circle STARK. There is no P-DAL (Reed–Solomon layer).
5. **Commercial execution is replicated.** Every validator recomputes each job's result.
   The lessee is the executor by attribution only. Commercial weight stays 0
   (`scivm::CONSENSUS_WEIGHT`).
6. **No refunds or timeouts.** The core ledger has no `ESCROW_REFUND` or `LEASE_TIMEOUT`,
   so every admitted job settles deterministically after `settle_delay` blocks.
7. **Founder vesting is tied to `BLOCKS_PER_DAY = 720` blocks**, regardless of the testnet
   block time.
8. **State only grows.** Fee lots are never pruned by the core, and every block stays in
   memory, so memory grows with chain length and job count.

## Core adapter (blueprint §13.2 A1)

The core math is unchanged. `primitives` gained one additive method, and v1 swapped its
hash backend to SHA3-512/SHAKE256 behind the same interface (the XOF type is now
`Shake256Xof`, formerly `MockXof`):

```rust
impl DomainSeparatedHash {
    /// The digest bytes by value; inverse of [`DomainSeparatedHash::from_bytes`].
    pub const fn to_bytes(&self) -> [u8; DIGEST_LEN] { self.0 }
}
```

`from_bytes` and `as_bytes` already existed. Every conversion between wire digests and core
hashes lives in `chain::core_adapter`, and so does the bridge from verified ML-DSA-87 keys
to the scheduler's `OwnerId`.

## PoC placeholders in the core

- **Hashing (real since v1):** `DomainSeparatedHash` is SHA3-512 and `Shake256Xof` is
  SHAKE256, both over the normative encoding: a domain label, an injective length-prefixed
  encoding, and 64-byte digests.
- **Signatures:** ML-DSA-87 lives in the `chain` crate since v1. The core crates take only the
  derived `OwnerId`.
- **Not implemented:** SLH-DSA, ML-KEM, the Circle STARK, P-DAL and zk-TWL notes.
- **Share weights:** `ShareWeight` is `u64`. The normative `w_s = ⌊2^640/Θ_S⌋` needs a bignum.
  With `u64` weights, every D'Hondt and payout cross-product is exact in `u128`.
- **Aggregate balances:** `U`, `S_z`, `B_bond` and `V_L2` hold totals only, with no per-UTXO
  ownership.
- **Deployment parameters:** epoch length, maturation delay `L`, `η` and fee tariffs are
  illustrative values in `LedgerParams::poc()`. The testnet takes them from its genesis file.

## Ledger category mapping

The 8 categories are the ones in eq. (7.19). `FounderAllocation` and `MatureFeeLots` are
sub-ledgers, not extra balances. The spec says vesting "adds no ninth balance", and matured
lots live in `R_a`.

| Variant           | Symbol      | Holds |
|-------------------|-------------|-------|
| `Circulating`     | `U`         | public UTXOs, incl. the 1,460 founder tranches |
| `ShieldedPool`    | `S_z`       | shielded notes |
| `Escrow`          | `E_esc`     | per-job escrows |
| `Bonds`           | `B_bond`    | executor bonds |
| `L2Vaults`        | `V_L2`      | rollup settlement vaults |
| `EmissionReserve` | `R_a`       | Genesis emission lot + mature fee lots + segregated dust |
| `LockedBaseFees`  | `R_l`       | Base Fees and queue fees awaiting maturation |
| `PendingRewards`  | `P_pending` | booked, unpaid Fresh Work rewards |
