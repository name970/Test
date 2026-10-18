# Plauthium Testnet Blueprint: multi-node P2P testnet / MVP

Version 0.1.1 · 2026-10-18 · Normative specification for code generation (implementation errata: Appendix C)

**Inputs**

* `plauthium_core_dump.txt`: source of the frozen core workspace (`primitives`, `work`, `scivm`, `scheduler`, `ledger`, `poc`).
* `PoUW_AppChain_Technical_Whitepaper_v2.0.pdf`: equations are cited as "WP (5.1)".

**Output of the generator:** three new crates (`wire`, `chain`, `node`) added to the workspace, plus a launch script and tests. The core crates do not change.

---

## 0. How to use this document

### 0.1 State of the inputs

`plauthium_core_dump.txt` holds the complete core: all 15 workspace and crate files (`Cargo.toml` and sources of the six core crates, the root `Cargo.toml`, `Cargo.lock` and `.gitignore`), with a sha256 per file. The core itself lives in `plauthium-workspace/` next to this file, together with the testnet crates built from this blueprint. To regenerate the dump:

```sh
CRATES="primitives work scivm scheduler ledger poc" \
  scripts/make_core_dump.sh plauthium-workspace plauthium_core_dump.txt
```

Section 13 lists every core item this specification uses. Each item is marked *confirmed* (visible in dumped source), *observed* (visible only at a PoC call site, as written for v0.1) or *required* (must exist; see the adapter rules in §13.2). With the complete core available, every observed item has since been confirmed against the source (Appendix C).

### 0.2 Normative language

MUST, MUST NOT, SHOULD and MAY are used as in RFC 2119. In Rust blocks marked `// normative`, the field order is the serialization order and is part of the protocol. Pseudocode fixes the order of operations, not the shape of the code.

### 0.3 Ground rules for the implementer

| # | Rule |
|---|------|
| G1 | **The core is frozen.** Do not edit `primitives`, `work`, `scivm`, `scheduler`, `ledger` or `poc`, except for the additive items in §13.2. All existing tests MUST pass unchanged. |
| G2 | **Consensus uses integers only.** No `f32`/`f64` in any consensus decision. The only floats are the SciVM job operands; the node treats them as opaque data and only `scivm` interprets them. |
| G3 | **One hash function.** Every identifier and commitment is `primitives::DomainSeparatedHash::new(label, parts)`, or is computed inside `work`. Do not add sha2/sha3/blake crates in v0. |
| G4 | **Determinism.** A block's validity and its derived fields are a pure function of (parent `NodeState`, ancestor headers, block). Wall-clock time, randomness, `HashMap` iteration order and thread timing MUST NOT influence them. Consensus state uses `BTreeMap` or sorted `Vec`. The local clock only *defers* blocks from the future (§6.6). |
| G5 | **Peer and RPC input never panics.** No `unwrap`, `expect`, `panic!` or unchecked indexing on those paths. New crates use `#![forbid(unsafe_code)]`. |
| G6 | **Only the ledger changes balances.** The node never computes balances itself. It calls `ledger::Ledger` and records the `BalanceDelta` each call returns. |
| G7 | **Wire types are format-agnostic.** Serde wire types MUST NOT use `#[serde(tag, untagged, flatten, skip_serializing_if, default)]`, because these break non-self-describing formats. The JSON DTOs of the RPC are separate types. |
| G8 | **Async never blocks.** CPU-heavy work (tile products, verification) runs in `spawn_blocking` or on dedicated OS threads. No lock is held across `.await`. Actors never await each other in a cycle (§8.2). |

---

## 1. Scope

### 1.1 In scope (testnet v0)

* N nodes over TCP with static peering, gossip of blocks and commercial jobs, and catch-up sync.
* Fresh Work consensus on the real 512 × 512 INT8 tile from `work`, using the dual seal/share lottery with configurable difficulty.
* Fork choice by heaviest accumulated seal weight, with reorgs up to `MAX_REORG_DEPTH`.
* Commercial jobs (SciVM exact dot products) that go through `ESCROW_FUND_AND_QUEUE → JOB_ADMIT_AND_LEASE → RESULT_SETTLE` on `ledger::Ledger`, with leases allocated by `scheduler::dhondt_allocation`.
* Epoch processing: `RESERVE_MATURE`, epoch booking and `FW_REWARD` from the Genesis lot and from the fee lots.
* Axum JSON RPC and an embedded single-page explorer showing the eight ledger categories, which always sum to `M = 1.5 × 10^15` atoms.

### 1.2 Known gaps, accepted for v0

These gaps MUST appear in the node's `--help` text and in the README the generator writes.

1. **Placeholder cryptography.** `DomainSeparatedHash` and the XOF are non-cryptographic placeholders (`poc/src/main.rs` line 11), so no identifier is collision resistant. The testnet assumes honest participants.
2. **No signatures.** Owners are self-declared integers. Jobs debit the aggregate `U` (the core ledger has no per-account ownership; see `ledger/src/lib.rs` lines 18–21), so `U` acts as a shared testnet faucet.
3. **VerifyFW by recomputation.** Validators recompute every claimed tile instead of verifying a Circle STARK. There is no P-DAL (Reed–Solomon layer).
4. **Shares are not gossiped.** A miner includes only its own shares in its own blocks (§6.7), so reward fairness is approximate. `GossipShare` is deferred to v1.
5. **Commercial execution is replicated.** Every validator recomputes each job's result. The lessee is the executor by attribution only. Commercial weight stays 0 (`scivm::CONSENSUS_WEIGHT`).
6. **No refunds or timeouts.** The core ledger has no `ESCROW_REFUND` or `LEASE_TIMEOUT`, so every admitted job MUST settle deterministically after `settle_delay` blocks.
7. **Founder vesting is tied to `BLOCKS_PER_DAY = 720` blocks** regardless of the testnet block time.
8. **State only grows.** Fee lots are never pruned by the core, so memory grows linearly with the number of jobs.

---

## 2. Workspace layout and dependencies

```text
plauthium-workspace/
├── Cargo.toml          # add members wire, chain, node; [workspace.dependencies]; profiles (§2.2)
├── primitives/ work/ scivm/ scheduler/ ledger/ poc/     # frozen core (G1)
├── wire/               # lib: serde types, codec, constants. deps: serde, bincode
├── chain/              # lib: genesis, validation, STF, fork choice, templates.
│                       #      synchronous, no tokio. deps: wire + the five core libs
├── node/               # bin `plauthium-node`: tokio actors, P2P, axum RPC, miner, explorer
│   ├── src/{main,cli,chain_actor,mempool,engine,net,rpc,store}.rs
│   ├── static/index.html
│   └── genesis.testnet.json
└── scripts/local_testnet.sh
```

The crate names follow the existing unprefixed style. Every new crate uses `edition = "2024"` and `[lints] workspace = true`.

### 2.1 Pinned dependencies

| Crate | Version | Features | Used by |
|---|---|---|---|
| serde | 1.0.228 | derive | wire, chain, node |
| bincode | **=1.3.3** | none | wire |
| serde_json | 1 | none | chain (genesis file), node |
| tokio | 1.53 | rt-multi-thread, macros, net, sync, time, signal, fs | node |
| tokio-util | 0.7.20 | codec | node |
| futures | 0.3.34 | none | node |
| bytes | 1 | none | node |
| axum | 0.8.9 | default | node |
| clap | 4.6 | derive | node |
| tracing | 0.1.44 | none | chain, node |
| tracing-subscriber | 0.3.23 | env-filter | node |

> **bincode warning.** `bincode 3.0.0` on crates.io is a tombstone whose `lib.rs` is a single `compile_error!`. RUSTSEC-2025-0141 marks the crate unmaintained (informational), and its maintainers declare 1.3.3 complete. Pin `bincode = "=1.3.3"` exactly. Only `wire::codec` may call bincode, so replacing it later (for example with `postcard`) changes one file. Such a replacement changes every wire byte and every identifier, so it requires a new `chain_id`.

### 2.2 Additions to the root Cargo.toml

Merge these into the existing file. Do not replace it.

```toml
[workspace]
members = [ "...existing...", "wire", "chain", "node" ]

[workspace.dependencies]
serde = { version = "1.0.228", features = ["derive"] }
bincode = "=1.3.3"
# ... the rest of §2.1

# One 512^3 tile at opt-level 0 is roughly 30x slower; tests mine and verify real tiles.
[profile.dev.package.work]
opt-level = 3
[profile.dev.package.primitives]
opt-level = 3
[profile.dev.package.scivm]
opt-level = 3
```

---

## 3. Parameters, labels and genesis

### 3.1 Protocol constants (`wire::consts`)

| Constant | Value | Meaning |
|---|---|---|
| `WIRE_VERSION: u32` | 1 | P2P protocol version in `Handshake` |
| `CORE_PROTOCOL_VERSION: u64` | 2 | `protocol_version` context field (core `*/v2` labels) |
| `PLACEHOLDER_SUITE_ID: u64` | 0 | `crypto_suite_id`; 0 means "non-cryptographic placeholder suite" |
| `GENERATOR_VERSION: u64` | 2 | `generator_version` context field |
| `DIGEST_LEN: usize` | 64 | bytes per digest (see §13.2 A1) |
| `MIN_TARGET: u128` | `1 << 64` | lower clamp for any lottery target (§6.4) |
| `DOT_LEN: usize` | 512 | operand length of an `ExactDot` job (= `work::TILE_DIM`) |
| `MAX_JOB_BYTES: usize` | 16 384 | encoded `JobTransaction` limit |
| `MAX_FRAME_BYTES: usize` | 16 777 216 | TCP frame limit |
| `MAX_SYNC_BLOCKS: usize` | 64 | blocks per `SyncResponse` |
| `MAX_SYNC_BYTES: usize` | 8 388 608 | soft byte limit per `SyncResponse` |
| `SYNC_LOOKBACK: u64` | 8 | initial overlap when starting a sync |
| `MAX_REORG_DEPTH: u64` | 128 | deepest reorg a node follows; also snapshot retention |
| `MAX_FUTURE_SECS: u64` | 15 | allowed clock lead of `time_bucket` before a block is deferred |
| `HANDSHAKE_TIMEOUT` | 5 s | wait for the peer's `Handshake` |
| `SYNC_TIMEOUT` | 30 s | one outstanding `SyncRequest` per peer |
| `ORPHAN_LIMIT` | 256 | blocks with an unknown parent kept in memory |
| `MEMPOOL_CAPACITY` | 4 096 | transactions |

### 3.2 Genesis parameters (`GenesisConfig`, consensus-critical)

| Field | Default | Meaning |
|---|---|---|
| `name` | `"plauthium-testnet-0"` | label only |
| `chain_id: u64` | `0x504C_4154` ("PLAT") | context field 1, checked in `Handshake` |
| `genesis_time_bucket: u64` | set by `plauthium-node genesis` to now | `time_bucket` of height 0 |
| `time_bucket_secs: u64` | 1 | seconds per time bucket |
| `target_block_secs: u64` | 10 | retarget goal (the core's `T_B = 120 s` applies to mainnet) |
| `initial_seal_target: Target` | `u128::MAX / 32` | expected attempts per seal ≈ 32 |
| `share_ratio: u64` | 4 | `Θ_S / Θ_B` (whitepaper: 120, reduced because VerifyFW recomputes, §1.2 item 3) |
| `difficulty: DifficultyMode` | `Windowed { window: 32 }` | or `Fixed` |
| `blocks_per_epoch: u64` | 30 | `LedgerParams::blocks_per_epoch` |
| `maturation_delay: u64` | 2 | `L`, in epochs |
| `emission_numerator / emission_denominator: u64` | 1 / 1000 | `η` |
| `anchor_depth: u64` | 6 | `stable_anchor_id` = ancestor at `h − anchor_depth` |
| `settle_delay: u64` | 2 | a job admitted at `h` settles at `h + settle_delay` |
| `share_maturity_depth: u64` | 3 | a share counts for leases this many blocks after inclusion |
| `snapshot_window: u64` | 60 | D'Hondt cohort covers inclusions in `[h − 60, h − 3]` |
| `share_inclusion_window: u64` | 30 | a share mined at height `s` can be included up to `s + 30` |
| `max_jobs_per_block: u64` | 32 | |
| `max_shares_per_block: u64` | 16 | |
| `max_block_bytes: u64` | 1 048 576 | encoded `Block` limit |

Genesis validation, done at load time with a hard error on failure:

* `share_ratio ≥ 1`
* `initial_seal_target ≥ MIN_TARGET`
* `time_bucket_secs ≥ 1`
* `target_block_secs ≥ time_bucket_secs` and `target_block_secs % time_bucket_secs == 0`
* `blocks_per_epoch ≥ 1`
* `anchor_depth ≥ 1`
* `settle_delay ≥ 1`
* `1 ≤ share_maturity_depth < snapshot_window`
* `share_inclusion_window ≥ 1`
* `window ≥ 2` when `Windowed`
* `ledger::EmissionRate::new(num, den)` returns `Ok`
* `ledger::Ledger::genesis(params)` returns `Ok`

### 3.3 Domain labels

The node computes every one of its own digests as `DomainSeparatedHash::new(label, parts)`. Integers are encoded big-endian at fixed width (`u64` → 8 bytes, `u128`/`i128` → 16 bytes, two's complement for `i128`). "codec(x)" means `wire::codec::encode(&x)`.

| Label | Parts | Produces |
|---|---|---|
| `PLAU/TESTNET/GENESIS/v1` | codec(GenesisConfig) | `genesis_id` |
| `PLAU/TESTNET/TAG/v1` | name bytes | `program_id` = tag(`"program/fresh-work-int8-512"`), `air_id` = tag(`"air/int8-tile"`) |
| `PLAU/TESTNET/MINER/v1` | owner (u64) | `miner_policy_digest` |
| `PLAU/TESTNET/PAYOUT/v1` | owner (u64) | `payout_policy_digest` |
| `PLAU/TESTNET/DIFFICULTY/v1` | seal_target (u128), share_target (u128) | `parent_derived_difficulty_context` |
| `PLAU/TESTNET/BODY/v1` | codec(BlockBody) | `body_core_root` |
| `PLAU/TESTNET/JOB/v1` | codec(JobTransaction) | `tx_id` |
| `PLAU/TESTNET/CLIENT/v1` | client text (≤ 64 bytes) | `JobTransaction.client` (RPC convenience) |
| `PLAU/TESTNET/LEDGER/v1` | §5.8 | `ledger_digest` |
| `PLAU/TESTNET/RESULT/v1` | job_id (u64), result (i128) | `result_digest` |

All labels share the prefix `PLAU/TESTNET/` and differ at the next byte (G, T, M, P, D, B, J, C, L, R). Every multi-part input above is fixed-width. Domain separation therefore holds even if `DomainSeparatedHash::new` simply concatenates its inputs.

The core's own labels (`FW/INPUT/v2`, `FW/A`, `FW/B`, `FW/OUTPUT/v2`, `FW/ID/v2`, `BLOCK/v2`, `FW/SHARE/v2`, `FW/SEAL/v2`) are applied inside `work`. The node MUST NOT re-implement them.

### 3.4 Genesis

```rust
// normative (wire crate)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DifficultyMode { Fixed, Windowed { window: u64 } }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenesisConfig {
    pub name: String,
    pub chain_id: u64,
    pub genesis_time_bucket: u64,
    pub time_bucket_secs: u64,
    pub target_block_secs: u64,
    pub initial_seal_target: Target,
    pub share_ratio: u64,
    pub difficulty: DifficultyMode,
    pub blocks_per_epoch: u64,
    pub maturation_delay: u64,
    pub emission_numerator: u64,
    pub emission_denominator: u64,
    pub anchor_depth: u64,
    pub settle_delay: u64,
    pub share_maturity_depth: u64,
    pub snapshot_window: u64,
    pub share_inclusion_window: u64,
    pub max_jobs_per_block: u64,
    pub max_shares_per_block: u64,
    pub max_block_bytes: u64,
}
```

* `genesis_id = H(PLAU/TESTNET/GENESIS/v1, codec(config))`. Height 0 is virtual: there is no genesis `Block`. `block_id(0) = genesis_id`, `time_bucket(0) = genesis_time_bucket`, and the seal and share targets of height 0 are `initial_seal_target` and its share target. Height 0 contributes zero weight.
* `LedgerParams { genesis_height: 0, blocks_per_epoch, maturation_delay, emission_rate: EmissionRate::new(num, den)? }`.
* The genesis `NodeState` is `Ledger::genesis(params)` with empty share and job registries and `pending_payout = 0`.
* The genesis file is JSON (`serde_json` of `GenesisConfig`; `Target` is a hex string in JSON, §4.3). `node/genesis.testnet.json` is embedded with `include_str!` as the default. Every node of one testnet MUST load a byte-identical config.

### 3.5 Owner identity

A node runs with `--owner-id <u64 ≥ 1>`, the `scheduler::OwnerId` of everything it mines. The policy digests are `miner_policy_digest = H(MINER, owner)` and `payout_policy_digest = H(PAYOUT, owner)`. Validators recompute both from `BlockHeader.miner_owner` or `ShareProof.owner` (§6.3). This binds the beneficiary before work starts (WP 5.1), but without keys.

---

## 4. Wire protocol and serialization

### 4.1 Transport and framing

* TCP. Each frame is a `u32` big-endian length followed by that many bytes of `codec(WireMessage)`. Use `tokio_util::codec::LengthDelimitedCodec::builder().length_field_type::<u32>().big_endian().max_frame_length(MAX_FRAME_BYTES)`.
* A frame that fails to decode, or decodes with trailing bytes, is a protocol violation (§4.8).

### 4.2 Codec (`wire::codec`, the only module that calls bincode)

```rust
// normative
use bincode::Options;
pub fn options() -> impl Options + Copy {
    bincode::DefaultOptions::new()
        .with_limit(MAX_FRAME_BYTES as u64)
        .with_little_endian()
        .with_fixint_encoding()
        .reject_trailing_bytes()
}
pub fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>, CodecError>;          // options().serialize(v)
pub fn decode<T: DeserializeOwned>(b: &[u8]) -> Result<T, CodecError>;     // options().deserialize(b)
```

With these options, integers are fixed-width little-endian, an enum variant is a `u32` LE index in declaration order, `Vec`/`String` carry a `u64` LE length prefix, and `Option` is a one-byte tag. The same bytes are hashed for `tx_id`, `body_core_root` and `genesis_id`, so **declaration order of every field and variant in `wire` is frozen**.

### 4.3 Primitive wire types

```rust
// normative
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest(pub [u8; DIGEST_LEN]);     // canonical bytes of a DomainSeparatedHash

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Target(pub u128);                 // lottery target over the top 128 bits (§6.1)
```

Serde for both types is hand-written:

* **Binary** (`!serializer.is_human_readable()`): `Digest` is a 64-element tuple of `u8` (exactly 64 raw bytes, no length prefix), and `Target` is a plain `u128`.
* **Human-readable** (JSON): `Digest` is 128 lowercase hex characters, and `Target` is `"0x"` followed by 32 hex characters.

`Digest` also has a hand-written `Debug` that prints the first 8 hex characters. Its derived `Ord` over the bytes is the canonical sort order for `tx_id`, `work_id` and `ShareID`. A reference implementation of `Digest` was compiled and round-tripped against bincode 1.3.3 and serde_json while this document was written:

```rust
impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() { return s.serialize_str(&to_hex(&self.0)); }
        use serde::ser::SerializeTuple;
        let mut t = s.serialize_tuple(DIGEST_LEN)?;
        for b in &self.0 { t.serialize_element(b)?; }
        t.end()
    }
}
// Deserialize: human-readable -> parse exactly 128 hex chars; binary -> deserialize_tuple(DIGEST_LEN, visitor)
// that reads DIGEST_LEN u8 elements. `to_hex`/`from_hex` are 10-line local helpers; no `hex` crate.
```

### 4.4 Messages (exact)

```rust
// normative — variant order is the bincode discriminant
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WireMessage {
    /* 0 */ Handshake { version: u32, chain_id: u64, best_height: u64, best_block_id: Digest },
    /* 1 */ GossipBlock(Box<Block>),   // boxed in memory only; serde encodes Box<Block> exactly like Block
    /* 2 */ GossipJob(JobTransaction),
    /* 3 */ SyncRequest { from_height: u64 },
    /* 4 */ SyncResponse { blocks: Vec<Block> },
}
```

| Message | Sent when | Receiver action |
|---|---|---|
| `Handshake` | first frame on every connection, by both sides at once | check `version == WIRE_VERSION` and `chain_id == genesis.chain_id`, else close. Record `best_*` and start a sync if needed (§4.7). |
| `GossipBlock` | a block was accepted locally (mined or relayed) | import it (§8.3). On `Accepted`, relay to all peers except the sender. |
| `GossipJob` | the mempool accepted a job (RPC or relayed) | insert it into the mempool. On `Ok`, relay to all peers except the sender. |
| `SyncRequest` | sync driver (§4.7) | reply with `SyncResponse` holding the responder's canonical blocks from `from_height`, ascending and contiguous, at most `MAX_SYNC_BLOCKS`. Stop adding blocks once the encoded size would pass `MAX_SYNC_BYTES` (always at least 1 block). Reply with empty `blocks` if `from_height > tip`, or if `from_height == 0` (genesis is not a block). |
| `SyncResponse` | answer to `SyncRequest` | import the blocks in order (§4.7). A response with no outstanding request is ignored: it may be the late answer to a request that timed out. |

`best_block_id` is the tip's `block_id`, or `genesis_id` at height 0. `best_height` is only a sync hint: fork choice uses weight, never height.

### 4.5 Connection lifecycle

1. Dial each `--peer` address, or accept on `--p2p-listen`. Wrap the stream in `Framed` (§4.1).
2. Send our `Handshake` immediately, built from the current tip snapshot.
3. Wait at most `HANDSHAKE_TIMEOUT` for the first frame. It MUST be a valid `Handshake`; otherwise close and penalize (§4.8). A second `Handshake` later is a protocol violation.
4. Register the peer with the PeerManager. Peers are deduplicated by socket address: if an outbound connection to the address already exists, keep the older connection.
5. Run the read loop and the write loop (the write loop drains an `mpsc` with capacity 256). When the queue is full, drop *gossip* frames for that peer and log. Never drop `SyncResponse` frames.
6. When an outbound connection drops, redial with backoff of 1 s, 2 s, 4 s and so on, capped at 30 s. There is no idle timeout in v0.

### 4.6 Gossip and deduplication

* Keep LRU sets of seen `block_id`s (8 192) and `tx_id`s (16 384). Ignore a message whose id was seen recently and do not relay it.
* Relay only after a positive local result: `ChainEvent::BlockAccepted` for blocks (main chain or side chain), and `MempoolEvent::Accepted` for jobs.
* Do not relay accepted blocks more than `2 · SYNC_LOOKBACK` below the tip. A node that is syncing would otherwise re-gossip its whole backlog; peers that need old blocks fetch them with `SyncRequest`.
* An `Orphan` result (unknown parent) is not a fault. Keep the block in the orphan pool and start a sync with the sender.

### 4.7 Sync algorithm (per peer, at most one outstanding request)

```text
start_sync(peer, hint_height):
    from = max(1, min(hint_height, our_best_height + 1).saturating_sub(SYNC_LOOKBACK))
    lookback = SYNC_LOOKBACK
    send SyncRequest { from_height: from }; arm SYNC_TIMEOUT

on Handshake:  if !chain.has_block(peer.best_block_id): start_sync(peer, peer.best_height)
on Orphan ack for a GossipBlock b:                     start_sync(peer, b.header.context.height)

on SyncResponse { blocks }:
    if blocks.is_empty(): done
    acks = blocks.map(import)               # in order; each ack is returned before Stage A (§8.3)
    if acks[0] == Orphan:                   # parent unknown AND not pending: the fork point is lower
        lookback *= 2
        if lookback > MAX_REORG_DEPTH: log "no common ancestor within reorg depth"; disconnect; return
        from = max(1, from.saturating_sub(lookback)); send SyncRequest { from }; return
    more = blocks.len() == MAX_SYNC_BLOCKS || encoded size hit MAX_SYNC_BYTES || peer.best_height > last.height
    if more: awaiting = last.block_id       # flow control: verification is sequential
    else: done

on ChainEvent::BlockAccepted(id == awaiting):  from = last.height + 1; send SyncRequest { from }
on ChainEvent::BlockRejected(id == awaiting) or SYNC_TIMEOUT:  done
```

A block whose parent is *pending* (in Stage A, deferred, or itself parked) is acked `Queued`, not `Orphan` (§6.8 step 3). The next batch is requested only after the previous batch's last block was applied. Together these keep the orphan pool bounded and prevent a sync from re-requesting blocks it already has.

### 4.8 Limits and penalties

| Event | Action |
|---|---|
| Undecodable frame, trailing bytes, oversize frame, a first frame that is not a `Handshake`, or a second `Handshake` | close, ban for 60 s |
| I/O error, connection reset, EOF, or no `Handshake` within `HANDSHAKE_TIMEOUT` | close (no ban: these are not misbehaviour) |
| `version` or `chain_id` mismatch | close (no ban) |
| Invalid block (any §6.8 failure other than Duplicate, Orphan, Queued, Deferred or TooDeep) | close, ban for 600 s |
| Invalid job (mempool rejects it as `Invalid`, not `Duplicate`/`Full`) | count it; at 10 per peer, close and ban for 600 s |
| More than 10 `SyncRequest` per second from one peer | drop the excess requests |

Bans are keyed by IP address, except for loopback peers, which are keyed by full socket address. On a localhost testnet every peer shares 127.0.0.1, so an IP ban would cut a node off from all the other local nodes.

A genesis mismatch shows up as a permanent Orphan at height 1. If the parent of the peer's block at height 1 is not our `genesis_id`, close the connection with the log reason `genesis mismatch`.

---

## 5. Block and transaction structures

### 5.1 Fresh Work context: the 16 fields of WP (5.1)

```rust
// normative — field order = WP (5.1) = serialization order
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FwContext {
    pub chain_id: u64,                              //  1
    pub genesis_id: Digest,                         //  2
    pub protocol_version: u64,                      //  3
    pub crypto_suite_id: u64,                       //  4
    pub program_id: Digest,                         //  5
    pub air_id: Digest,                             //  6
    pub generator_version: u64,                     //  7
    pub parent_canonical_id: Digest,                //  8  parent link
    pub stable_anchor_id: Digest,                   //  9  parent link (anchor)
    pub height: u64,                                // 10
    pub miner_policy_digest: Digest,                // 11
    pub payout_policy_digest: Digest,               // 12
    pub attempt_nonce: u64,                         // 13
    pub body_core_root: Digest,                     // 14
    pub parent_derived_difficulty_context: Digest,  // 15
    pub time_bucket: u64,                           // 16
}
```

Integer fields are `u64` on the wire whatever the core widths are. Conversion to `work::FreshWorkContext` uses `TryFrom`; a failed conversion makes the object invalid (§5.9).

### 5.2 Block header

```rust
// normative
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockHeader {
    pub context: FwContext,       // the 16 bound fields; carry both parent links
    // Fresh Work outputs, WP (5.4)-(5.5); each is recomputed by validators
    pub output_root: Digest,      // H(FW/OUTPUT/v2, XY)
    pub work_id: Digest,          // = ShareID
    pub block_id: Digest,         // H(BLOCK/v2, x, outputRoot): the block's identity
    pub share_ticket: Digest,     // θ_S
    pub seal_ticket: Digest,      // θ_B
    // Bound through context fields 15 and 11/12
    pub seal_target: Target,      // Θ_B for this height
    pub share_target: Target,     // Θ_S for this height
    pub miner_owner: u64,         // scheduler::OwnerId of the beneficiary
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]      // not Eq: the body holds f64 operands
pub struct Block { pub header: BlockHeader, pub body: BlockBody }
```

Every header field is either one of the 16 context fields, a deterministic function of them (`output_root`, the identifiers, the tickets), or bound through a context digest (`seal_target` and `share_target` through field 15, `miner_owner` through fields 11 and 12). `block_id` therefore commits to the whole header, and to the whole body through `body_core_root`. Parent links: `parent_canonical_id` (field 8) is the parent's `block_id`, and `stable_anchor_id` (field 9) is the anchor ancestor.

### 5.3 Block body

```rust
// normative
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlockBody {
    // inputs chosen by the producer
    pub jobs: Vec<JobTransaction>,            // newly funded+admitted jobs; sorted by tx_id, unique
    pub shares: Vec<ShareProof>,              // Fresh Work shares (rights); sorted by work_id, unique
    // derived commitments: validators recompute them with apply_body (§7) and require equality
    pub leases: Vec<LeaseAssignment>,         // leases[i] belongs to jobs[i]
    pub receipts: Vec<SettlementReceipt>,     // executed commercial jobs (admitted at h - settle_delay)
    pub fee_transitions: Vec<FeeTransition>,  // every ledger operation of this block, in execution order
    pub ledger_digest: Digest,                // commitment to the post-state (§5.8)
}
```

`jobs` and `receipts` are the commercial transactions: jobs entering, and jobs executed and settled. `fee_transitions` is the complete, ordered list of ledger transitions with their exact per-category deltas. The explorer renders it directly.

### 5.4 Commercial job transaction

```rust
// normative
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobTransaction {
    pub client: Digest,        // free tag, unauthenticated in v0
    pub queue_fee: u64,        // f_queue, atoms, > 0
    pub base_fee: u64,         // F_base, atoms, > 0
    pub v_exec: u64,           // > 0
    pub v_da: u64,             // >= 0
    pub v_ref: u64,            // >= 0
    pub expiry_height: u64,    // last height at which the job may be included
    pub nonce: u64,            // client-chosen; makes tx_id unique
    pub payload: JobPayload,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum JobPayload {
    /* 0 */ ExactDot { xs: Vec<f64>, ys: Vec<f64> },   // SciVM Ozaki-II exact dot product
}
```

* `deposit = base_fee + v_exec + v_da + v_ref` (checked addition), as in WP (7.17). The total client charge is `deposit + queue_fee`.
* `tx_id = H(JOB, codec(tx))`. `job_id = u64::from_be_bytes(tx_id[0..8])` as `ledger::JobId`.
* **Stateless validity**, checked by the mempool and again by validators:
  * fee rules above, with no overflow;
  * `xs.len() == ys.len() == DOT_LEN`;
  * every operand finite with zero fractional part;
  * `codec(tx).len() ≤ MAX_JOB_BYTES`;
  * `TESTNET_PROFILE.exact_dot(&xs, &ys)` returns `Ok`. The core decides the operand bounds.
* `TESTNET_PROFILE` is exactly the `OzakiIIProfile` literal of `poc/src/main.rs` lines 133–140 (version 1, moduli `[255, 254, 253, 251, 247, 241, 239, 233]`, `inner_dim = 512`, `bound_x = bound_y = 1 << 23`, `RoundingMode::NearestTiesToEven`). `validate()` is checked once at startup.

### 5.5 Share proof

```rust
// normative
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareProof {
    pub context: FwContext,       // full context of the attempt (its own parent/height/body root)
    pub owner: u64,               // must match context fields 11/12
    pub output_root: Digest,
    pub work_id: Digest,          // = ShareID
    pub share_ticket: Digest,
    pub share_target: Target,     // Θ_S at context.height on the including chain
}
```

Shares carry their complete context, so they can be verified without chain state (Stage A, §6.2). The cost is about 0.9 KiB per share.

### 5.6 Leases and receipts

```rust
// normative
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseAssignment { pub job_id: u64, pub share_id: Digest, pub owner: u64 }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementReceipt {
    pub job_id: u64,
    pub tx_id: Digest,
    pub executor_owner: u64,      // owner of the leased ShareID
    pub admitted_at: u64,
    pub result: i128,             // TESTNET_PROFILE.exact_dot(xs, ys)
    pub result_digest: Digest,    // H(RESULT, job_id, result)
    pub v_exec: u64,
    pub v_da: u64,
    pub v_ref: u64,
}
```

### 5.7 Fee transitions

```rust
// normative
/// Signed change per category in `ledger::StateCategory::ALL` order:
/// [U, S_z, E_esc, B_bond, V_L2, R_a, R_l, P_pending]; built as ALL.map(|c| delta.get(c)).
pub type CategoryDelta = [i128; 8];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerReward { pub owner: u64, pub atoms: u64 }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeeTransition {
    /* 0 */ ReserveMature { epoch: u64, matured_lots: Vec<u64>, delta: CategoryDelta },
    /* 1 */ FwReward { epoch: u64, amount: u64, delta: CategoryDelta },
    /* 2 */ EpochBooked { epoch: u64, budget: u64, booked: u64, dust: u64, lots: u32, shares: u32,
                          per_owner: Vec<OwnerReward>, delta: CategoryDelta },
    /* 3 */ ResultSettle { job_id: u64, v_exec: u64, v_da: u64, v_ref: u64, delta: CategoryDelta },
    /* 4 */ EscrowFundAndQueue { job_id: u64, deposit: u64, queue_fee: u64, delta: CategoryDelta },
    /* 5 */ JobAdmitAndLease { job_id: u64, base_fee: u64, owner: u64, delta: CategoryDelta },
}
```

Every `delta` MUST sum to zero (WP 7.13). This holds by construction because the deltas come from the ledger, and validators re-derive them anyway.

### 5.8 Identifiers and commitments

| Name | Definition |
|---|---|
| `genesis_id` | `H(GENESIS, codec(GenesisConfig))` |
| `block_id` | `work::FreshWorkIds::derive(&ctx, &product).block_id`, the core's `H(BLOCK/v2, x, outputRoot)` |
| `work_id` = ShareID | `FreshWorkIds.work_id` |
| `body_core_root` | `H(BODY, codec(body))` |
| `tx_id`, `job_id` | §5.4 |
| `result_digest` | `H(RESULT, job_id, result)` |
| `ledger_digest` | `H(LEDGER, height, epoch, balance(c) for c in StateCategory::ALL (8 × u64), segregated_dust, fee_lots().len() as u64, locked_fees().len() as u64, pending_payout)`: 15 fixed-width parts |

### 5.9 Conversion to core types (`chain::core_adapter`)

* `Digest ⇄ DomainSeparatedHash` goes through the adapter of §13.2 A1.
* `FwContext → work::FreshWorkContext` copies the fields one by one. Integers use `try_into()`, and `Err` means invalid. Digests use `from_bytes`.
* `FreshWorkIds → (Digest × 5)` uses `to_bytes`.
* `ShareRecord → scheduler::ShareID { id, weight, owner: OwnerId(owner) }`.

---

## 6. Consensus rules

### 6.1 Lottery comparison

`top128(d) = u128::from_be_bytes(d.0[0..16])`. A seal is valid when `top128(seal_ticket) < seal_target.0`, and a share is valid when `top128(share_ticket) < share_target.0`. This is exactly WP (5.6) `θ < Θ` with `Θ = target · 2^384`, using integers only.

### 6.2 Stage A: VerifyFW (expensive, chain-independent)

```rust
/// Recomputes the tile for `ctx` and checks the claimed outputs. Pure: needs no chain state.
pub fn verify_fresh_work(ctx: &FwContext, claimed: &Claimed, target: Target, lottery: Lottery)
    -> Result<(), WorkError>
{
    let core = ctx.to_core()?;                         // §5.9
    let inst = core.instance();                        // poc/src/main.rs:66
    let z = inst.x.product(&inst.y);                   // 512^3 MACs (poc:75)
    let ids = work::FreshWorkIds::derive(&core, &z);   // poc:95
    // compare output_root, work_id, share_ticket (+ block_id, seal_ticket for Lottery::Seal)
    // then the §6.1 comparison of the relevant ticket against `target`
}
```

Stage A checks the header (lottery `Seal`) and every `ShareProof` (lottery `Share`). It runs in `spawn_blocking`, bounded by a semaphore of `--verify-threads` permits, and a block's tiles are verified in parallel. A Freivalds check is not used here, because the product `Z` (1 MiB) is not transmitted. The miner uses Freivalds as a self-check (§8.5). Blocks with `Origin::Local` or `Origin::Disk` skip Stage A.

### 6.3 Stage B: expected context (cheap, relative to the parent chain)

For a block at height `h` with parent `P` (`P.height = h − 1`):

| Field | Required value |
|---|---|
| 1 `chain_id` | `genesis.chain_id` |
| 2 `genesis_id` | `genesis_id` |
| 3 `protocol_version` | `CORE_PROTOCOL_VERSION` |
| 4 `crypto_suite_id` | `PLACEHOLDER_SUITE_ID` |
| 5 `program_id` | tag(`"program/fresh-work-int8-512"`) |
| 6 `air_id` | tag(`"air/int8-tile"`) |
| 7 `generator_version` | `GENERATOR_VERSION` |
| 8 `parent_canonical_id` | `P.block_id` (`genesis_id` when `h = 1`) |
| 9 `stable_anchor_id` | `block_id` of the ancestor at height `h.saturating_sub(anchor_depth)` on P's chain (`genesis_id` for height 0) |
| 10 `height` | `P.height + 1` |
| 11, 12 | `H(MINER, miner_owner)` and `H(PAYOUT, miner_owner)`, with `miner_owner ≥ 1` |
| 13 `attempt_nonce` | any value |
| 14 `body_core_root` | `H(BODY, codec(body))` |
| 15 `parent_derived_difficulty_context` | `H(DIFFICULTY, seal_target, share_target)` |
| 16 `time_bucket` | `≥ P.time_bucket` (§6.6) |
| `seal_target`, `share_target` | `expected_targets(h, chain)` (§6.4) |

A `ShareProof` with `s = share.context.height` is checked against the same rules evaluated at height `s` on the **including block's chain**: fields 1–7 are constants; field 8 is the ancestor at `s − 1`; field 9 is the ancestor at `s.saturating_sub(anchor_depth)`; fields 11 and 12 come from `share.owner`; field 15 and `share_target` match the targets of the block at height `s`. Fields 13, 14 and 16 are free. A share does not need its own template to have become a block.

### 6.4 Difficulty

```rust
pub fn share_target_for(seal: Target, ratio: u64) -> Target {
    Target(seal.0.saturating_mul(ratio as u128))
}

/// Targets of the block at height h >= 1, from its ancestors only.
pub fn expected_targets(h: u64, chain: &impl ChainView, g: &GenesisConfig) -> (Target, Target) {
    let prev = if h == 1 { g.initial_seal_target } else { chain.meta_at(h - 1).seal_target };
    let seal = match g.difficulty {
        DifficultyMode::Windowed { window: n } if (h - 1) % n == 0 && h - 1 >= 2 * n => {
            let actual = chain.time_bucket_at(h - 1).saturating_sub(chain.time_bucket_at(h - 1 - n));
            let expected = n * (g.target_block_secs / g.time_bucket_secs);
            Target(scale_target(prev.0, actual, expected))
        }
        _ => prev,
    };
    (seal, share_target_for(seal, g.share_ratio))
}

/// floor(T * actual / expected), with actual clamped to [ceil(expected/4), 4*expected],
/// exact in u128 (T = q*E + r), saturating at u128::MAX, floored at MIN_TARGET.
pub fn scale_target(t: u128, actual: u64, expected: u64) -> u128 {
    let e = expected.max(1);
    let a = u128::from(actual.clamp(e.div_ceil(4), e.saturating_mul(4)));
    let e = u128::from(e);
    (t / e).checked_mul(a)
        .and_then(|x| x.checked_add((t % e) * a / e))
        .unwrap_or(u128::MAX)
        .max(MIN_TARGET)
}
```

The first retarget happens at `h = 2n + 1` and measures blocks `n..2n`. Blocks `1..2n` use `initial_seal_target`, so the gap between creating the genesis file and mining block 1 never distorts a window. For `--seal-attempts k`, `initial_seal_target = u128::MAX / k`. The test vectors are in Appendix B.

### 6.5 Seal weight, fork choice and reorgs

```rust
/// Work of a target in units of 2^-32 attempts: floor((2^128 - 1) / floor(T / 2^32)).
/// T >= MIN_TARGET = 2^64 keeps the result <= 2^96, so a u128 chain weight cannot overflow
/// in practice (checked_add; overflow = invalid block).
pub fn work_of(t: Target) -> u128 { u128::MAX / (t.0 >> 32) }
pub fn seal_weight(h: &BlockHeader) -> u128 { work_of(h.seal_target) }                    // d_B, WP (5.10)
pub fn share_weight(t: Target) -> u64 { u64::try_from(work_of(t)).unwrap_or(u64::MAX) } // w_s, WP (6.1)
```

* `W(b) = W(parent) + seal_weight(b)`, with `W(genesis) = 0`. Shares, jobs, fees and receipts add nothing (WP 5.11).
* The best tip is the fully validated block with the largest `W`. **On a tie the current tip is kept**, so the block seen first wins (WP 5.10 legend).
* A reorg switches the canonical pointer to the new branch. Post-state snapshots exist per block (§7.1), so nothing is undone by hand: the whole delta set reverts together (WP 8.2 item 9).
* A block whose parent is more than `MAX_REORG_DEPTH` below the tip, or whose parent's snapshot was pruned, is rejected with `TooDeep`.
* With the default target `u128::MAX / 32`, `seal_weight ≈ 32 · 2^32`.

### 6.6 Time

`now_bucket = unix_secs / time_bucket_secs`. The consensus rule is `time_bucket(h) ≥ time_bucket(h − 1)`. As a local rule, a block with `time_bucket > now_bucket + MAX_FUTURE_SECS / time_bucket_secs` is **Deferred**: it is not invalid, and it is retried every 5 s for up to 10 minutes. The template uses `max(P.time_bucket, now_bucket)`.

### 6.7 Shares: inclusion, registration and maturity

* **Inclusion** in block `h`:
  * `1 ≤ s ≤ h − 1` and `s ≥ h − share_inclusion_window`, where `s = share.context.height`;
  * Stage A and Stage B pass;
  * `work_id` is not in the parent's registry and not repeated within the body;
  * `shares.len() ≤ max_shares_per_block`.
* **Header share.** If `top128(header.share_ticket) < header.share_target`, the block's own work is registered as a share of `miner_owner` at inclusion height `h`. This happens after `apply_body`, so the body never depends on its own seal attempt.
* **Weight.** `share_weight(share_target)`, fixed by the consensus target in force at the share's height (WP 6.1).
* **Lease cohort for block `h`:** registered shares with `included_at ∈ [h.saturating_sub(snapshot_window), h − share_maturity_depth]`, sorted by `work_id`.
* **Epoch weights at a boundary `h = e · blocks_per_epoch` (`e ≥ 1`):** shares with `included_at ∈ [h − blocks_per_epoch, h − 1]`, sorted by `work_id`.
* **Registry retention.** At the end of `apply_body`, drop shares with `included_at < h − RETAIN`, where `RETAIN = max(snapshot_window, blocks_per_epoch, share_inclusion_window) + 1`. A dropped share can never be included again: its height is below the inclusion window.

### 6.8 Full validation order (cheap checks first)

1. Decode and size check: `codec(block).len() ≤ max_block_bytes`, list caps, sorting and uniqueness in the body.
2. Duplicate (`block_id` known, in flight, parked, or in the invalid cache) → `Duplicate`.
3. Parent not in the tree → park in the orphan pool, unverified, keyed by parent. Ack `Queued` if the parent is pending (in Stage A, deferred, or itself parked), otherwise `Orphan`. Parked blocks re-enter at step 1 when their parent is applied.
4. Parent snapshot missing, or fork deeper than `MAX_REORG_DEPTH` → `TooDeep`.
5. Stage B for the header (§6.3) and for each share (§6.3, §6.7 inclusion rules).
6. Future `time_bucket` → `Deferred` (§6.6).
7. Stage A for the header and all shares, in parallel (skipped for `Local` and `Disk`).
8. `apply_body` on a clone of the parent state (§7.2). Its derived `leases`, `receipts`, `fee_transitions` and `ledger_digest` MUST equal the body's. Any ledger error makes the block invalid.
9. Register the header share (§6.7) and compute `W`. Insert into the tree, persist (§8.7), run fork choice (§6.5), emit events.

---

## 7. State transition function (`chain::stf`)

### 7.1 Node state (one immutable snapshot per block)

```rust
#[derive(Clone)]
pub struct NodeState {
    pub height: u64,
    pub ledger: ledger::Ledger,                   // Clone (ledger/src/lib.rs:572)
    pub shares: BTreeMap<Digest, ShareRecord>,    // work_id -> {owner, weight, height, included_at}
    pub open_jobs: BTreeMap<u64, OpenJob>,        // job_id -> {tx_id, admitted_at, owner, share_id,
                                                  //            v_exec, v_da, v_ref, result: i128}
    pub used_tx: BTreeMap<Digest, u64>,           // tx_id  -> expiry_height (pruned once expired)
    pub used_job_ids: BTreeMap<u64, u64>,         // job_id -> expiry_height (pruned once expired)
    pub pending_payout: u64,                      // booked at the last boundary, paid at the next
}
```

Snapshots are kept as `Arc<NodeState>` for every block within `MAX_REORG_DEPTH` of the tip, and are pruned beyond that.

### 7.2 `apply_body`: normative order

```text
apply_body(parent: &NodeState, g, h, jobs: &[JobTransaction], shares: &[VerifiedShare])
    -> Result<(NodeState, Derived), StfError>

s = parent.clone(); ft = []                  # all-or-nothing: the clone is discarded on any error
s.ledger.advance_to(h)

# (1) epoch boundary, WP (7.24)-(7.25)
if h % g.blocks_per_epoch == 0:              # h >= blocks_per_epoch because h >= 1
    e = s.ledger.epoch()
    (d, lots) = s.ledger.reserve_mature()                      -> ReserveMature{e, lots, d}
    d = s.ledger.fw_reward(Atoms(s.pending_payout))            -> FwReward{e, s.pending_payout, d}
    w = epoch weights (§6.7) as Vec<ShareWeight> in work_id order
    (d, p) = s.ledger.book_epoch(&w)                           -> EpochBooked{e, p.budget, p.booked,
                                                                   p.dust, p.lots.len(), w.len(),
                                                                   per_owner(p.per_share), d}
    s.pending_payout = p.booked
    # per_owner: sum p.per_share[i] by the owner of share i; sorted by owner; zero entries omitted

# (2) settle the jobs admitted at h - settle_delay, in job_id order
for j in s.open_jobs where j.admitted_at + g.settle_delay == h:
    d = s.ledger.result_settle(JobId(j.id), j.v_exec, j.v_da, j.v_ref)
                                                               -> ResultSettle{..., d}
    receipts.push(SettlementReceipt{...}); s.open_jobs.remove(j.id)

# (3) fund the new jobs (jobs are sorted by tx_id)
require jobs.len() <= max_jobs_per_block; each job statelessly valid; h <= expiry_height;
        tx_id not in used_tx; job_id not in used_job_ids
for tx in jobs:
    d = s.ledger.escrow_fund_and_queue(JobId(tx.job_id), deposit, queue_fee)
                                                               -> EscrowFundAndQueue{..., d}

# (4) allocate leases with D'Hondt over the mature cohort, WP (6.4)
if !jobs.is_empty():
    cohort = lease cohort (§6.7) as Vec<scheduler::ShareID>; require !cohort.is_empty()
    alloc = scheduler::dhondt_allocation(&cohort, jobs.len())?; require alloc.assigned() == jobs.len()
    for (i, tx) in jobs: share = cohort[alloc.slots[i]]
        d = s.ledger.job_admit_and_lease(JobId(tx.job_id), base_fee)
                                                               -> JobAdmitAndLease{job_id, base_fee, share.owner, d}
        leases.push({job_id, share.id, share.owner})
        result = TESTNET_PROFILE.exact_dot(xs, ys)?
        s.open_jobs.insert(job_id, OpenJob{admitted_at: h, ...}); mark tx_id and job_id used

# (5) register the shares of the body
for sh in shares: s.shares.insert(sh.work_id, {owner, weight, height: s_h, included_at: h})

# (6) prune, check, commit
prune used_tx / used_job_ids with expiry < h; prune shares per RETAIN (§6.7)
s.ledger.check_invariants()?                 # WP (7.19): sum of 8 categories == M, sub-ledgers consistent
s.height = h
return (s, Derived{leases, receipts, fee_transitions: ft, ledger_digest: ledger_digest(&s)})
```

Properties the tests MUST assert:

* `apply_body(parent, h, [], [])` always succeeds. Empty blocks are always possible, which guarantees liveness.
* Escrow is exact: after settlement the job's escrow is `deposit − base_fee − (v_exec + v_da + v_ref) = 0`, and the ledger removes it.
* `ledger.total() == M` after every block.

Two consequences of the core's semantics:

* Jobs cannot be funded until `U` exceeds the founder tranches that are still locked (`FounderTrancheLocked`). On a fresh chain that first happens at the `FwReward` of epoch 2 (`h = 2 · blocks_per_epoch`).
* A lease needs a non-empty mature cohort.

The template builder (§7.4) leaves unfundable jobs in the mempool until they become fundable or expire.

### 7.3 Job lifecycle (simplification of WP 6.14)

```text
mempool: Pending ──(block h: EscrowFundAndQueue + JobAdmitAndLease, lease → ShareID owner)──▶ Admitted
Admitted ──(block h + settle_delay: ResultSettle + SettlementReceipt)──▶ Settled
Pending ──(tip > expiry_height)──▶ Expired (dropped from mempool, nothing on chain)
```

`Funded`, `Admitted` and `Running` collapse into a single block. `Retry` and `Refunded` do not exist in v0 (§1.2 item 6).

### 7.4 Template building (`chain::template`)

```text
build_template(tip, tip_state, mempool_view (FIFO), own_shares, now) -> Template
    h = tip.height + 1
    shares = own_shares with (§6.7 inclusion rules vs. tip chain) sorted by work_id, at most max_shares
    jobs = []
    for tx in mempool_view:                                    # FIFO: no fee priority, no auctions (WP 6.16)
        if jobs.len() == max_jobs: break
        cand = sort_by_tx_id(jobs + [tx])
        if apply_body(tip_state, h, &cand, &shares).is_ok(): jobs = cand
    (_, derived) = apply_body(tip_state, h, &jobs, &shares)    # cannot fail
    body = BlockBody { jobs, shares, ..derived }
    while codec(body).len() > max_block_bytes - 4096: drop the last-added job and re-derive
    (seal_target, share_target) = expected_targets(h)
    context = FwContext { constants, parent: tip.id, anchor, height: h, policy(owner), attempt_nonce: 0,
                          body_core_root: H(BODY, codec(body)), difficulty ctx, time_bucket: max(tip.tb, now) }
    Template { generation: next(), context, body: Arc, seal_target, share_target, miner_owner }
```

The `max_block_bytes − 4096` margin leaves room for the header. The greedy loop costs at most `4 · max_jobs` clones of the tip state; that is acceptable for v0.

---

## 8. Asynchronous node architecture (Tokio)

### 8.1 Components

```text
  HTTP (--rpc-listen)                                   TCP (--p2p-listen, --peer ...)
        │                                                         │
  ┌─────▼──────┐   GetBlocks/HasBlock            ImportBlock  ┌────▼────────┐
  │ RpcServer  │──────────────────┐        ┌──────────────────│ PeerManager │── PeerTask × n
  └─────┬──────┘                  ▼        ▼                  └────┬────────┘
        │ Insert            ┌─────────────────────┐                │ Insert
        │                   │     ChainState      │                │
        │                   │  BlockTree, Arc<    │                │
        │                   │  NodeState> per     │                │
        │                   │  block, Ledger,     │                │
        │                   │  template builder   │                │
        │                   └──┬───────┬───────┬──┘                │
        │      watch<Template> │       │       │ broadcast<ChainEvent>
        │                      ▼       │       └──────────▶ Mempool, PeerManager
        │          ┌──────────────────┐│ FilterIncluded (Mempool ─▶ ChainState only)
        │          │ ConsensusEngine  ││
        │          │ N std::threads   │└── watch<StatusSnapshot> ─▶ RpcServer
        │          └──────┬───────────┘
        │                 │ SubmitLocalBlock / SubmitLocalShare ─▶ ChainState
        ▼                 ▼
  ┌──────────────────────────────┐  watch<MempoolView> ─▶ ChainState
  │           Mempool            │  broadcast<MempoolEvent> ─▶ PeerManager
  └──────────────────────────────┘
```

The diagram is an overview. The channel table in §8.2 is authoritative.

### 8.2 Channels and the await graph

| From → To | Channel | Capacity | Awaits reply? |
|---|---|---|---|
| PeerTask, Rpc, Engine → ChainState | `mpsc<ChainCmd>` | 1 024 | `oneshot` ack only (never waits for Stage A) |
| PeerTask, Rpc → Mempool | `mpsc<MempoolCmd>` | 1 024 | yes |
| Mempool → ChainState | `ChainCmd::FilterIncluded` | (shared) | yes; only after `broadcast` lag |
| ChainState → all | `broadcast<ChainEvent>` | 1 024 | no |
| ChainState → Engine | `watch<Option<Arc<Template>>>` | latest only | no |
| ChainState → Rpc, PeerTask | `watch<Arc<StatusSnapshot>>` | latest only | no |
| Mempool → ChainState | `watch<Arc<MempoolView>>` (FIFO, ≤ `4 · max_jobs`) | latest only | no |
| Mempool → PeerManager | `broadcast<MempoolEvent>` | 1 024 | no |
| PeerManager → PeerTask | `mpsc<WireMessage>` per peer | 256 | no |

**Await graph.** Rpc → {ChainState, Mempool}; PeerTask → {ChainState, Mempool}; Mempool → ChainState. ChainState awaits no actor. The graph is acyclic, so the actors cannot deadlock. Every reverse-direction flow uses `watch` or `broadcast`.

```rust
pub enum Origin { Local, Rpc, Peer(PeerId), Disk }

pub enum ChainCmd {
    ImportBlock { block: Arc<Block>, origin: Origin, ack: Option<oneshot::Sender<ImportAck>> },
    SubmitLocalBlock(Block),
    SubmitLocalShare(ShareProof),
    WorkVerified { block: Arc<Block>, origin: Origin, result: Result<(), WorkError> }, // internal
    GetCanonicalRange { from_height: u64, reply: oneshot::Sender<Vec<Arc<Block>>> },  // §4.4 limits
    GetBlocks { before: Option<u64>, limit: usize, reply: oneshot::Sender<Vec<BlockSummary>> },
    HasBlock { id: Digest, reply: oneshot::Sender<bool> },
    FilterIncluded { tx_ids: Vec<Digest>, reply: oneshot::Sender<Vec<Digest>> },
}
pub enum ImportAck { Queued, Duplicate, Orphan, Deferred, Invalid(String), TooDeep }

pub enum ChainEvent {
    BlockAccepted { block: Arc<Block>, origin: Origin },             // any branch: relay
    BlockRejected { block_id: Digest, origin: Origin, reason: String }, // penalize
    TipChanged { tip: BlockMeta, connected: Vec<Arc<Block>>, disconnected: Vec<Arc<Block>> },
}
```

### 8.3 ChainState actor (`node::chain_actor`, wrapping `chain::BlockTree`)

* **Owns** `BlockTree { metas: HashMap<Digest, BlockMeta>, blocks: HashMap<Digest, Arc<Block>>, states: HashMap<Digest, Arc<NodeState>>, children, canonical: Vec<Digest> /* index = height */, tip }`, the orphan pool (keyed by parent id, at most `ORPHAN_LIMIT`, oldest evicted), the deferred queue, an invalid-id LRU (4 096) and `own_shares`. `BlockMeta = { id, parent, height, time_bucket, seal_target, share_target, seal_weight, cumulative_weight, miner_owner }`.
* **Import pipeline:**
  * `ImportBlock` runs §6.8 steps 1–6 in the actor and acks at once.
  * Step 7 is spawned (`spawn_blocking` under the verify semaphore). Its result comes back as `WorkVerified`.
  * Steps 8–9 run in the actor, which serializes all state changes.
  * After a block is applied, the actor re-submits the parked blocks whose parent is that block, recursively. Stage A therefore runs in parallel *within* a block (header and shares), while consecutive blocks are verified one after another.
* **Fork choice and events.** After each insert, run §6.5. On a tip change, emit `TipChanged` with the connected and disconnected blocks (walk both branches to the fork point), update `canonical`, and publish `StatusSnapshot`.
* **Templates.** Rebuild (§7.4) on `TipChanged`, on a `MempoolView` change or a new own share (debounced 500 ms), and every 5 s (to refresh `time_bucket`). Publish to `watch<Template>` with an increasing `generation`. Drop own shares that fell out of the inclusion window or were registered.
* **`StatusSnapshot`** contains the tip meta, the 8 balances, `ledger.total()`, founder locked at the tip height (`ledger.founder_vesting().locked_at(h)`), `segregated_dust`, the fee-lot and locked-fee counts, the epoch, `pending_payout`, open jobs, orphan count and the genesis summary.
* **Loop:** `tokio::select!` over the command channel, `mempool_view.changed()`, and a `sleep_until(next_tick)` that serves deferred blocks and template refresh.

### 8.4 Mempool actor (`node::mempool`)

* `Insert { tx, origin, reply }`:
  * Check stateless validity (§5.4); `exact_dot` is cheap, so it runs inline.
  * Reject if the tx is a duplicate by `tx_id` or `job_id`, or if `expiry_height < tip_height + 1`.
  * Reject with `Full` when the pool holds `MEMPOOL_CAPACITY` txs. There is no fee-based eviction (WP 7.20: no tips or auctions).
  * On success, return `{tx_id, job_id, expected_result}`, emit `MempoolEvent::Accepted { tx, origin }`, and republish `MempoolView` in FIFO arrival order.
* On `TipChanged`: remove the txs of connected blocks, and re-insert the txs of disconnected blocks that are not in a connected block (skipping stateless re-checks). Drop expired txs.
* On `broadcast` lag (`RecvError::Lagged`): call `FilterIncluded` with all pooled `tx_id`s, then drop those returned.
* `Stats` and `List { limit }` serve the RPC.

### 8.5 ConsensusEngine (`node::engine`)

`--mining-threads N` OS threads, plus a tokio task that computes `attempts_per_sec` (an EWMA over 10 s from an `AtomicU64` counter). `set_enabled(bool)` is used by tests.

```rust
// thread i of n
loop {
    if shutdown { return }
    let Some(tpl) = templates.borrow().clone() else { sleep(200ms); continue };   // watch::Receiver
    let mut nonce = i as u64;
    while enabled && templates.borrow().as_ref().map(|t| t.generation) == Some(tpl.generation) {
        let Ok(ctx) = tpl.context_with_nonce(nonce).to_core() else { error!("bad template"); break }; // §5.9
        let inst = ctx.instance();
        let z = inst.x.product(&inst.y);
        let ids = work::FreshWorkIds::derive(&ctx, &z);
        stats.attempts.fetch_add(1, Relaxed);
        let seal = top128(&ids.seal_ticket) < tpl.seal_target.0;
        let share = top128(&ids.share_ticket) < tpl.share_target.0;
        if (seal || share)
            && !work::verify_freivalds_rounds(&inst.x, &inst.y, &z, work::DEFAULT_FREIVALDS_ROUNDS)
        {
            error!("Freivalds self-check failed: engine or hardware fault"); nonce += n; continue;
        }
        if seal { chain_tx.blocking_send(ChainCmd::SubmitLocalBlock(tpl.seal(nonce, &ids))); break; }
        if share { chain_tx.blocking_send(ChainCmd::SubmitLocalShare(tpl.share(nonce, &ids))); }
        nonce += n as u64;
    }
    // after a seal, wait for the generation to change (poll every 50 ms)
}
```

* An attempt's seal is never also submitted as a `ShareProof`: the header share rule (§6.7) covers it.
* Results from a stale template are still submitted. A stale seal becomes a side block; a stale share stays valid while it is inside the window.
* Difficulty is configured only through genesis (`--seal-attempts`, `--share-ratio`, `--difficulty`, `--target-block-secs` on `plauthium-node genesis`), because it is consensus-critical.

### 8.6 Network (`node::net`)

* The PeerManager task owns `HashMap<PeerId, PeerHandle { addr, direction, tx: mpsc::Sender<WireMessage>, best_height, best_block_id, invalid_jobs }>`, the ban list, the dialers and the relay logic. It subscribes to `ChainEvent` (to relay `BlockAccepted` to all peers except the origin, and to ban on `BlockRejected` from a peer) and to `MempoolEvent` (to relay `Accepted` jobs to all peers except the origin).
* A PeerTask runs §4.5 and dispatches inbound messages:
  * `GossipBlock` goes to `ImportBlock` (acks drive §4.7).
  * `GossipJob` goes to `Mempool::Insert`.
  * `SyncRequest` goes to `GetCanonicalRange`.
  * `SyncResponse` goes to the sync driver.
* `Handshake` contents are built from `watch<StatusSnapshot>`.

### 8.7 Persistence and startup (`node::store`)

* `<data-dir>/blocks.log` is append-only. Each record is a `u32` BE length followed by `codec(Block)`. A block is appended once it is valid and inserted, on any branch, so parents always precede children.
* On startup the node creates the genesis state, then reads the log in order and imports each block with `Origin::Disk` (Stage A skipped, everything else executed). A truncated last record is cut off and logged.
* `--reset` deletes the data dir first.

### 8.8 Shutdown

On Ctrl-C (`tokio::signal`): stop the miners, stop accepting connections, flush and `fsync` `blocks.log`, and exit 0.

### 8.9 Concurrency rules (restating G8)

* Tile products run only on engine threads or in `spawn_blocking`.
* `std::sync::Mutex` may guard only short, non-async sections.
* `watch::Receiver::borrow()` guards are dropped before any `.await`.
* The ChainState actor is the only writer of chain state.

---

## 9. RPC and embedded web explorer (`node::rpc`)

### 9.1 Router (axum 0.8)

| Method and path | Handler |
|---|---|
| `GET /` | `include_str!("../static/index.html")`, `text/html; charset=utf-8`, `Cache-Control: no-cache` |
| `GET /api/status` | §9.3, read from the watch snapshots (no actor round trip) |
| `GET /api/blocks?limit=20&before=<height>` | §9.4, canonical blocks with `height < before` (default tip + 1), newest first, `limit ≤ 100` |
| `POST /api/submit_job` | §9.5 |

* Bind `127.0.0.1:8080` by default. No CORS headers: the dashboard is same-origin. Request bodies are limited to 64 KiB (`DefaultBodyLimit::max(65_536)`).
* Errors use the body `{"error": "<code>", "message": "<text>"}`.

### 9.2 JSON conventions

* **Strings:** all amounts (atoms), job ids, owners, nonces, weights and every `u128`/`i128`, as decimal. `job_id` comes from a hash and routinely exceeds 2^53, so a JSON number would corrupt it.
* **Numbers:** heights, epochs, counts, `time_bucket`, sizes and rates.
* **Encodings:** digests are 128 lowercase hex characters; targets are `"0x"` plus 32 hex characters.
* **Category keys:** `U`, `S_z`, `E_esc`, `B_bond`, `V_L2`, `R_a`, `R_l`, `P_pending` (`StateCategory::symbol()`), always in `StateCategory::ALL` order.

### 9.3 `GET /api/status`

```json
{
  "chain": { "name": "plauthium-testnet-0", "chain_id": "0x504c4154", "genesis_id": "<hex>",
             "wire_version": 1, "blocks_per_epoch": 30, "target_block_secs": 10 },
  "node":  { "owner_id": "1", "peers": 2, "uptime_secs": 312 },
  "tip":   { "height": 61, "block_id": "<hex>", "time_bucket": 1792281931, "epoch": 2,
             "cumulative_weight": "8383776161792", "seal_target": "0x07ff...", "share_target": "0x1fff...",
             "expected_attempts_per_seal": "32" },
  "mining": { "enabled": true, "threads": 1, "attempts_per_sec": 6.4, "shares_found": 14, "seals_found": 5 },
  "mempool": { "size": 3 },
  "ledger": {
    "categories": [ { "symbol": "U", "name": "Circulating", "atoms": "151350000000000" }, "... 8 entries in ALL order" ],
    "total": "1500000000000000", "supply_m": "1500000000000000", "conserved": true,
    "founder_locked": "150000000000000", "segregated_dust": "3", "fee_lots": 5, "locked_fees": 2,
    "pending_payout": "1348650000000", "open_jobs": 1
  },
  "commercial_consensus_weight": 0
}
```

`expected_attempts_per_seal` is `u128::MAX / seal_target`, and `conserved` is `total == TOTAL_SUPPLY`. `commercial_consensus_weight` is `scivm::CONSENSUS_WEIGHT`.

### 9.4 `GET /api/blocks`

```json
{ "tip_height": 61, "blocks": [ {
  "height": 61, "block_id": "<hex>", "parent_id": "<hex>", "anchor_id": "<hex>",
  "miner_owner": "2", "time_bucket": 1792281931, "attempt_nonce": "17", "epoch": 2,
  "seal_ticket": "<hex>", "seal_weight": "137438953472", "cumulative_weight": "8383776161792",
  "jobs":     [ { "job_id": "9876543210123456789", "tx_id": "<hex>", "executor_owner": "3",
                  "deposit": "10000000000", "queue_fee": "100000000", "base_fee": "1000000000" } ],
  "receipts": [ { "job_id": "...", "result": "-1234567890123456", "executor_owner": "3", "admitted_at": 59 } ],
  "shares":   [ { "work_id": "<hex>", "owner": "2", "height": 60, "weight": "34359738368" } ],
  "header_share": true,
  "fee_transitions": [ { "op": "EscrowFundAndQueue", "job_id": "...",
                         "delta": { "U": "-10100000000", "S_z": "0", "E_esc": "10000000000", "B_bond": "0",
                                    "V_L2": "0", "R_a": "0", "R_l": "100000000", "P_pending": "0" } } ],
  "balances_after": { "U": "...", "S_z": "0", "...": "..." },
  "total_after": "1500000000000000"
} ] }
```

ChainState keeps a `BlockSummary` (including `balances_after`) for every canonical block, built when the block is applied.

### 9.5 `POST /api/submit_job`

Request (every field except `payload` is optional; the defaults are the worked budget of WP 7.17, with 1 PLAU = 10^8 atoms):

```json
{ "client": "alice", "queue_fee": "100000000", "base_fee": "1000000000",
  "v_exec": "8000000000", "v_da": "500000000", "v_ref": "500000000",
  "expiry_blocks": 120, "nonce": "42",
  "payload": { "type": "exact_dot", "xs": [ 512 integers ], "ys": [ 512 integers ] } }
```

The handler builds the transaction as follows:

* `client = H(CLIENT, text)`, or the all-zero digest when `client` is absent.
* `expiry_height = tip + expiry_blocks` (default 120, at most 10 000).
* `nonce` is random when absent.
* Operands are converted with `as f64` after checking `|v| < 2^53`.

The handler then calls `Mempool::Insert { origin: Rpc }`.

| Status | Body |
|---|---|
| 202 | `{ "tx_id", "job_id", "deposit", "total_charge", "expected_result", "expiry_height", "status": "pending" }` |
| 400 | `invalid_payload`, `zero_fee`, `amount_overflow`, `too_large`, `bad_request` |
| 409 | `duplicate` |
| 503 | `mempool_full` |

```sh
curl -s localhost:8081/api/submit_job -H 'content-type: application/json' \
  -d "{\"payload\":{\"type\":\"exact_dot\",\"xs\":[$(seq -s, 1 512)],\"ys\":[$(seq -s, 512 -1 1)]}}"
```

### 9.6 Dashboard (`node/static/index.html`)

**Constraints.** One file of at most 60 KB with inline CSS and vanilla JS. No external requests (no CDN or fonts), so it works on an offline testnet. Light and dark themes via `prefers-color-scheme`. The layout works down to 360 px wide.

**Header strip:** chain name, `chain_id`, own owner id, peers, tip height, short tip id, epoch, attempts/s, expected attempts per seal, and mempool size. A red "node unreachable" banner appears when polling fails.

**Ledger panel**, conserving `M`:

1. One 100 %-stacked horizontal bar of the 8 categories, with linear widths (`atoms / M`) and a minimum of 2 px for any non-zero category so the small ones stay visible.
2. Eight rows in `ALL` order. Each row shows the symbol and name, exact atoms with digit grouping, PLAU (`atoms / 10^8` with 8 decimals, computed in `BigInt`), the share of `M` in ppm, and a bar on a log scale (`log10(1 + atoms) / log10(1 + M)`). `S_z`, `B_bond` and `V_L2` are labelled "inactive in v0".
3. An invariant badge. The client computes `Σ = sum(BigInt(atoms))` and shows a green "Σ = M = 1,500,000,000,000,000 ✓" when `Σ === 1500000000000000n` and the server's `conserved` is `true`. Otherwise it shows a red alert with the difference.
4. Founder locked, segregated dust, fee lots, locked fees, pending payout and open jobs.

**Live block feed:** the last 20 blocks with height, short id, miner, job count, receipt count, share count (and a ★ for the header share), ΔU and ΔR_l (summed from `fee_transitions`), and the time since the previous block. New rows flash. Clicking a row expands its fee transitions and receipts.

**Job submission:**

* Fee inputs in PLAU (decimal, at most 8 places, parsed to atoms with `BigInt`), prefilled with the WP 7.17 budget.
* An operand seed and a "Generate" button that fills `xs`/`ys` from xorshift64* with integers in `[−(2^23 − 1), 2^23 − 1]`.
* A locally computed `BigInt` reference dot product.
* A Submit button. The response's `expected_result` is shown next to the local `BigInt` value, and they MUST match.

**My jobs:** jobs submitted from this browser, kept in memory and mirrored to `localStorage` inside `try/catch`. Each shows its status: pending → admitted @h (lease owner) → settled @h (result, compared with the reference). The status comes from scanning `jobs` and `receipts` in the block feed.

**Polling:** `/api/status` every 2 s, and `/api/blocks?limit=20` only when `tip.height` changed. Polling pauses while `document.hidden`.

---

## 10. CLI and local testnet

```text
plauthium-node genesis [--out genesis.json] [--name S] [--seal-attempts 32] [--share-ratio 4]
                       [--difficulty windowed|fixed] [--retarget-window 32] [--target-block-secs 10]
                       [--blocks-per-epoch 30] [--settle-delay 2]
plauthium-node run --data-dir DIR --owner-id N [--genesis FILE] [--p2p-listen 0.0.0.0:30333]
                   [--rpc-listen 127.0.0.1:8080] [--peer HOST:PORT]... [--mining-threads 1 | --no-mine]
                   [--verify-threads <cpus/2>] [--reset]
```

Logging uses `tracing` with `RUST_LOG=info` by default. Every accepted block logs one line: height, short id, miner, jobs, shares, weight.

`scripts/local_testnet.sh` (generated):

1. `cargo build --release -p node`.
2. `plauthium-node genesis --out target/testnet/genesis.json --seal-attempts 128 --target-block-secs 5 --blocks-per-epoch 12`.
3. Start three nodes: owners 1–3, P2P ports 30331–30333, RPC ports 8081–8083, full mesh through `--peer`, and data dirs under `target/testnet/n{1,2,3}`.
4. Print the three dashboard URLs. Ctrl-C stops all three nodes.

---

## 11. Testing and acceptance criteria

**Unit tests (`wire`, `chain`):**

1. Golden vectors from Appendix B (codec bytes, retarget, `work_of`).
2. A codec round trip for every wire type. Trailing bytes are rejected, and oversized frames are rejected.
3. `apply_body` conserves `M` and passes `check_invariants()` after every block. This is a randomized test: 200 blocks with random job counts, random fees and epoch boundaries. Blocks are mined with `seal_target = u128::MAX`, so every attempt seals.
4. Tampering: changing any derived body field, any context field, the targets, `miner_owner`, or one byte of an operand → the block is rejected. A wrong `fee_transitions` order → rejected.
5. Job flow: funding before founder unlock or rewards → `FounderTrancheLocked`, and the job stays out of the template. After epoch-2 rewards the job is admitted, then settled `settle_delay` blocks later with E_esc back to 0. R_l holds `queue_fee + base_fee` until `reserve_mature` turns them into two lots.
6. Fork choice: the heavier branch wins, a tie keeps the tip, and a reorg yields correct `connected`/`disconnected` sets. A reorg deeper than `MAX_REORG_DEPTH` is rejected.
7. Share rules: outside the window, duplicate `work_id`, wrong owner digest, or wrong `share_target` → invalid.

**Integration test (`node`, `#[tokio::test(flavor = "multi_thread")]`, release profile):** three in-process nodes on 127.0.0.1 with ephemeral ports, with genesis `seal_attempts = 64`, `share_ratio = 2`, `Fixed`, `blocks_per_epoch = 5` and `settle_delay = 1`. With these values one block takes about 1.5 s with three miners, and a foreign block costs about three tile recomputes to verify, so validation keeps up with production (Appendix C, item 1).

| # | Acceptance criterion |
|---|---|
| A1 | `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace --release` are green, and the existing core tests are unchanged. |
| A2 | Every node reaches height ≥ 15. After `set_enabled(false)` on all engines, all tips become identical and the 8 balances are identical. With mining stopped, two equal-weight tips both survive (a tie keeps the current tip, §6.5), so the test breaks a tie by letting one node mine one more block. |
| A3 | A job submitted to node 1 after height 10 appears admitted and then settled on node 3. Its `result` equals a direct `i128` dot product. Every block's `total_after == M`. |
| A4 | Node 3 is stopped, 10 blocks are mined, and node 3 is restarted on the same data dir: it replays `blocks.log` and syncs to the tip. |
| A5 | Partition {1} and {2, 3}, mine on both sides, then reconnect: all nodes converge on the heavier branch, and the losing branch's jobs return to the mempool. |
| A6 | The dashboard served at `/` shows "Σ = M ✓" during the whole run. An HTTP test asserts `/api/status.ledger.conserved == true` at every poll. |

---

## 12. Implementation order for the generator

1. **wire:** constants, `Digest`/`Target` serde, all types, codec, golden tests.
2. **chain::core_adapter and verify:** conversions and Stage A, tested against a header produced by mining with `seal_target = u128::MAX`.
3. **chain genesis, stf and template:** `GenesisConfig` load and validation, `NodeState`, `apply_body`, `build_template`, unit tests 3–5.
4. **chain::tree:** `BlockTree`, Stage B, fork choice, pruning, unit tests 6–7.
5. **node, single node:** ChainState actor, Mempool, ConsensusEngine, store.
6. **node::rpc and static/index.html.**
7. **node::net:** handshake, gossip, sync, penalties.
8. The integration tests, `scripts/local_testnet.sh` and a README.

Each step MUST compile and pass its tests before the next one starts.

---

## 13. Adapter contract with the core crates

### 13.1 Items this spec uses

"Confirmed" items are visible in source in the current dump. "Observed" items are visible only at PoC call sites (see the dump appendix). "Required" items must be added if missing (§13.2).

| Item | Status | Used in |
|---|---|---|
| `ledger::Ledger::{genesis, advance_to, escrow_fund_and_queue, job_admit_and_lease, result_settle, reserve_mature, book_epoch, fw_reward, check_invariants, balance, total, epoch, height, fee_lots, locked_fees, segregated_dust, founder_vesting}` | confirmed | §7, §8.3 |
| `Ledger: Clone`; `LedgerParams` with pub fields; `EmissionRate::new`; `LedgerError: Display` | confirmed | §3.4, §7.1 |
| `StateCategory::{ALL, symbol}`, `BalanceDelta::get`, `JobId(pub u64)`, `LotId(pub u64)`, `EpochPayout.{budget, booked, dust, per_share, lots}` | confirmed | §5.7, §7.2 |
| `primitives::{Atoms(pub u64), Atoms::get, Atoms::from_plau, TOTAL_SUPPLY, ShareWeight = u64}` | observed | §7, §9 |
| `primitives::DomainSeparatedHash::new(&str, &[&[u8]])`, `Copy` | observed | §3.3 |
| `DomainSeparatedHash ⇄ [u8; DIGEST_LEN]` | **required (A1)** | §5.9 |
| `work::{FreshWorkContext (16 pub fields), instance(), x.product(&y), FreshWorkIds::derive, verify_freivalds_rounds, DEFAULT_FREIVALDS_ROUNDS, TILE_DIM}` | observed | §6.2, §8.5 |
| `scivm::{OzakiIIProfile, validate, exact_dot -> Result<i128, _>, RoundingMode::NearestTiesToEven, CONSENSUS_WEIGHT}` | observed | §5.4, §9.3 |
| `scheduler::{ShareID {id, weight, owner}, OwnerId(pub u64), dhondt_allocation(&[ShareID], usize), Allocation.{slots, assigned()}}` | observed | §7.2 |

### 13.2 Permitted changes to the core (only these)

**A1.** In `primitives`, only if an equivalent does not already exist (if it does, use it and skip this):

```rust
impl DomainSeparatedHash {
    /// Canonical digest bytes, most significant byte first.
    pub fn to_bytes(&self) -> [u8; DIGEST_LEN] { /* expose the internal digest */ }
    /// Inverse of `to_bytes`; every byte string is a valid digest value.
    pub fn from_bytes(bytes: [u8; DIGEST_LEN]) -> Self { /* wrap */ }
}
```

If the placeholder digest is not 64 bytes, `wire::DIGEST_LEN` MUST equal its real length, which MUST be at least 16 (§6.1 reads 16 bytes). Every "64" and "128 hex" in this document then scales accordingly.

**A2.** In the root `Cargo.toml`: the members, workspace dependencies and profile overrides of §2.2.

No other core change is allowed. If an item in §13.1 is missing or has a different shape, the generator MUST stop and report the mismatch instead of editing the core.

### 13.3 Checks against the complete dump, before generating code

1. **`FreshWorkContext` field types:** generate the `TryFrom` conversions of §5.9.
2. **`OzakiIIProfile` field types:** copy the `TESTNET_PROFILE` literal from `poc/src/main.rs` lines 133–140 verbatim.
3. **`DomainSeparatedHash`:** its digest length and any existing byte accessors (A1).
4. **`dhondt_allocation`:**
   * `slots[i]` is an index into the input slice, as the PoC prints it at line 382;
   * ties break by the least ShareID;
   * find the behavior with an empty input. §7.2 never calls it with an empty input or with zero slots.
5. **`Atoms::from_plau` multiplier:** 10^8 is expected (WP 7.18). It drives the dashboard's PLAU formatting and the RPC defaults.
6. **`instance()` and `product()` signatures,** by reference or by value, so that §6.2 and §8.5 can be written without extra clones.

---

## 14. Deferred to v1

* `GossipShare(ShareProof)`, so that any producer includes everyone's shares (FruitChains fairness).
* SHA3-512, SHAKE256 and KMAC256 in `primitives`, and ML-DSA-87 signatures with per-owner UTXO accounting (`TWL_TRANSFER`).
* Lessee-side execution with result gossip, `LEASE_TIMEOUT`/`LEASE_RETRY`/`ESCROW_REFUND` (needs new ledger operations), and bonds (`BOND_LOCK`/`BOND_SLASH`).
* Circle STARK `π_FW` and the P-DAL instead of recomputation.
* A persistent state database with checkpoints, peer discovery, and TLS or Noise transport.

---

## Appendix A: Whitepaper traceability

| Rule here | Whitepaper |
|---|---|
| 16-field context bound before evaluation (§5.1, §6.3) | WP (5.1)–(5.2), §8.2 item 1 |
| `block_id`, `work_id = ShareID`, tickets computed by `work` (§5.8) | WP (5.4)–(5.5) |
| Seal and share lotteries with integer targets (§6.1) | WP (5.6)–(5.7), (1.4) |
| Fork choice on seal weight only; tie keeps the tip (§6.5) | WP (5.10)–(5.11), §8.2 item 2 |
| ShareID-indexed sequential D'Hondt leases (§7.2 step 4) | WP (6.4), §8.2 item 4 |
| Base Fee to `R_l` only at `JOB_ADMIT_AND_LEASE`, never back (§7.2 step 4) | WP (7.21), §8.2 item 11 |
| Zero-sum deltas, all-or-nothing blocks, reorg reverts whole delta sets (§5.7, §7.2, §6.5) | WP (7.13)–(7.14), §8.2 item 9 |
| Σ of 8 categories = M in every reachable state (§7.2 step 6, §9.6) | WP (7.19), Theorem 7.1 |
| Maturation `R_l → R_a`, geometric epoch budget, dust segregated (§7.2 step 1) | WP (7.24)–(7.26) |
| No fee priority or auctions in the mempool or template (§7.4, §8.4) | WP (6.16), (7.20) |
| Commercial work carries zero weight (§9.3) | WP (1.1), Table 1 |

## Appendix B: Golden vectors

These vectors were produced with `bincode =1.3.3` using the options in §4.2.

```text
Handshake { version: 1, chain_id: 0x504C_4154, best_height: 7, best_block_id: Digest([0x00, 0x01, ..., 0x3f]) }
  -> 88 bytes:
  00000000 01000000 54414c5000000000 0700000000000000
  000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f
  202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f

SyncRequest { from_height: 0x0102 }
  -> 12 bytes: 03000000 0201000000000000

Frame on the wire = u32 BE length || bytes, e.g. the SyncRequest frame:
  0000000c 03000000 0201000000000000
```

```text
scale_target(2^100, actual=320, expected=320) = 2^100
scale_target(2^100, 640, 320)                 = 2^101
scale_target(2^100, 10_000, 320)              = 2^102        (actual clamped to 4x)
scale_target(2^100, 1, 320)                   = 2^98         (actual clamped to 1/4)
scale_target(u128::MAX, 640, 320)             = u128::MAX    (saturates)
scale_target(1000, 320, 320)                  = 2^64         (floored at MIN_TARGET)
work_of(Target(u128::MAX / 32))               = 137438953472 = 32 * 2^32   (default seal weight)
work_of(Target(u128::MAX / 64)) >> 32         = 64
share_weight(share_target_for(Target(u128::MAX / 32), 4)) = 34359738368 = 2^35
```

Encoded sizes, from the §4–§5 types compiled verbatim:

* `BlockHeader`: 992 bytes.
* `JobTransaction` with an `ExactDot` payload of 512 + 512 operands: 8 332 bytes (limit 16 384).
* `ShareProof`: 848 bytes.

A full block (32 jobs, 16 shares, plus their leases, receipts and fee transitions) is therefore about 300 KiB, well under `max_block_bytes`.

## Appendix C: Implementation notes (v0.1.1)

These are corrections and refinements found while implementing and testing v0.1 against the real core crates.

1. **Integration-test difficulty.** v0.1 specified `seal_attempts = 2`, `share_ratio = 2`. That makes almost every attempt a share. Three nodes would then produce about 10 blocks per second each, while every foreign block costs up to 17 tile recomputes to verify (one attempt is about 60 ms on one core). Validation could never catch up. The tested values are `seal_attempts = 64`, `share_ratio = 2` (§11).
2. **`GossipBlock(Box<Block>)` and boxed `ChainCmd::SubmitLocalBlock` / `SubmitLocalShare`.** These keep the enums small in memory (clippy `large_enum_variant`). serde encodes `Box<T>` exactly like `T`, so the wire bytes and the Appendix B vectors are unchanged.
3. **Ban policy (§4.8).** Connection resets, EOF and handshake timeouts are not misbehaviour and are not banned. Loopback peers are banned by socket address, not IP. A `SyncResponse` without an outstanding request is ignored rather than banned.
4. **Relay depth (§4.6).** Accepted blocks more than 16 below the tip are not relayed.
5. **Ties after mining stops (§6.5, §11 A2).** The tie rule is unchanged. Convergence tests break ties with one extra block.
6. **Core adapter (§13.2 A1).** `DomainSeparatedHash::from_bytes` and `as_bytes` already existed. `to_bytes` (by value) was added on request; it is the only core change.
7. **`node` is a library plus a binary.** Integration tests can only import a library, so `start_node`, `NodeHandle` and the actor modules live in `node/src/lib.rs`, and `src/main.rs` is the CLI. `NodeHandle::stop` signals shutdown and awaits the ChainState and mempool actors. The ChainState actor flushes and syncs `blocks.log` on exit.
8. **`build_template` returns `Result`.** If even an empty body cannot be derived, the ChainState actor keeps the previous template and logs the error instead of panicking. Identical templates are not republished, so miners never repeat nonces on unchanged work.
9. **Miner seal handoff (§8.5).** After sealing, a miner waits up to 2 s for the template built on its block, then keeps mining the old one. Its block may have lost a tie.
10. **Status reads (§9.3).** `/api/status` reads the mempool size from `watch<MempoolView>`, so it makes no actor round trip.
11. **§13.1 checked against the full core.** Every *observed* item exists with the expected shape. The §13.3 checks resolved as follows:
    * `FreshWorkContext.chain_id` is `u64`, while `protocol_version`, `crypto_suite_id` and `generator_version` are `u32` (so `TryFrom` is needed only for those three).
    * `DomainSeparatedHash` is exactly 64 bytes.
    * `dhondt_allocation` returns `Allocation { slots: Vec<usize>, per_share: Vec<u64> }`. `slots[i]` indexes the input slice; ties go to the least ShareID; an empty input returns zero slots; zero weights and duplicate ShareIDs are errors.
    * `Atoms::from_plau` multiplies by 10^8.
    * `instance()` and `product()` take `&self`.
    * `OzakiIIProfile` has `version: u32`, `moduli: Vec<u32>`, `inner_dim: u64` and `bound_x`/`bound_y: u64`. `CONSENSUS_WEIGHT` is a `u64`.
