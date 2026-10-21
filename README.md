# Plauthium workspace: core + multi-node P2P testnet (v1 in progress)

Rust workspace for the Plauthium post-quantum layer-1. The six core crates are a frozen,
tested mathematical core that follows the *Plauthium Technical Specification*. Three
testnet crates wrap it into a multi-node P2P network, as specified in
[`../testnet_blueprint.md`](../testnet_blueprint.md). `unsafe` is forbidden everywhere,
and every public item is documented.

```sh
cargo test --workspace --release   # 64 tests, including the 3-node integration test (~2 min)
cargo run -p poc                   # core: mock block transition + ASCII summary
scripts/local_testnet.sh           # 3 mining nodes on localhost with live explorers
```

Run the tests in release mode. Every block involves at least one real 512×512 tile product.

## Crates

| Crate        | Spec      | Contents |
|--------------|-----------|----------|
| `primitives` | §1.3–1.4, Table 3, §7.10 | `Atoms` (1 PLAU = 10^8 atoms), supply `M = 1.5·10^15`, `Mersenne61` field (`p = 2^61 − 1`), `DomainSeparatedHash` (SHA3-512) + `Shake256Xof` (SHAKE256), `BlockResourceVector` (5 components), PQ object sizes |
| `work`       | §3.1–3.2, §5.1–5.2 | 512×512 INT8 `WorkUnit` over `[-127, 127]`, INT32 `AccumulationTarget` with `\|z\| ≤ 8,258,048 < 2^23`, exact tile product, 16-field Fresh Work context, seed expansion by byte rejection, `verify_freivalds_rounds` (`r_F = 6`) |
| `scivm`      | §3.6      | `OzakiIIProfile` (pairwise-coprime moduli ≤ 255, `Q > 2·B_rec`), `split_residues`, centered CRT, exact residue dot product, zero consensus weight |
| `scheduler`  | §6.1–6.5  | `ShareID`, sequential D'Hondt (`w_s/(a_s+1)` by exact cross-multiplication, ties to least ShareID), owner capacity, persistent cohort counters |
| `ledger`     | §7.1–7.8  | 8 disjoint `StateCategory` balances, atomic zero-sum deltas, `FounderVesting` (1,460 daily tranches × 720 blocks), `FeeLot` with ceiling budgets, floor payouts and segregated dust |
| `poc`        | —         | Binary that runs Genesis → Fresh Work → D'Hondt → Base Fee lock/maturation/payout |
| `wire`       | blueprint §3–§5 | The five P2P messages, block/job/share types, `Digest`/`Target`, fixed-width ML-DSA-87 `PublicKey`/`Signature`, the bincode 1.3.3 codec (fixed-width little-endian, trailing bytes rejected), protocol constants |
| `chain`      | blueprint §6–§7 | Genesis validation, Stage A (signatures, then VerifyFW by recomputing the tile) and Stage B (context checks), windowed retarget, `apply_body` (the state transition on `ledger::Ledger`), D'Hondt leases, templates, `BlockTree` with heaviest-seal-weight fork choice, ML-DSA-87 owner keys (`chain::keys`) |
| `node`       | blueprint §8–§10 | `plauthium-node`: ChainState / Mempool actors, multi-threaded ConsensusEngine, length-delimited TCP PeerManager with sync, append-only `blocks.log`, persistent miner key, axum JSON RPC and the embedded explorer |

## Local 3-node testnet

```sh
scripts/local_testnet.sh
```

The script builds the release binary and writes `target/testnet/genesis.json`
(`--seal-attempts 128 --target-block-secs 5 --blocks-per-epoch 12`). It then starts three
fully meshed mining nodes. Their explorers are at:

- Node 1: <http://127.0.0.1:8081/>
- Node 2: <http://127.0.0.1:8082/>
- Node 3: <http://127.0.0.1:8083/>

On its first run each node generates an ML-DSA-87 miner key in
`target/testnet/n<i>/miner_key.bin` and loads it on every later run. `--reset` wipes the
chain data but keeps the key, so a node keeps its identity across restarts.

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
  and a forged copy cannot shadow the genuine block or job.

`crypto_suite_id` is 1 (SHA3-512 + SHAKE256 + ML-DSA-87) and `WIRE_VERSION` is 2.

## Known gaps (blueprint §1.2)

Gaps 1 (placeholder hashing) and 2 (self-declared integer owners, no signatures) are closed
in v1. The numbering of the remaining gaps follows the blueprint.

- **Shared faucet.** The core ledger has no per-account balances (see
  `ledger/src/lib.rs` lines 18–21), so a job's signature authenticates its client, but its
  deposit is debited from the aggregate `U`.

3. **VerifyFW by recomputation.** Validators recompute every claimed tile instead of
   verifying a Circle STARK. There is no P-DAL (Reed–Solomon layer).
4. **Shares are not gossiped.** A miner includes only its own shares in its own blocks, so
   reward fairness is approximate. `GossipShare` is deferred to v1.
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
