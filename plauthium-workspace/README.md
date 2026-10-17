# Plauthium PoC workspace

Proof-of-concept Rust workspace for the Plauthium post-quantum layer-1. It follows the
*Plauthium Technical Specification*. It has no external dependencies and forbids `unsafe`.

```sh
cargo test --workspace   # 37 unit tests
cargo run -p poc         # mock block transition + ASCII summary
```

## Crates

| Crate        | Spec      | Contents |
|--------------|-----------|----------|
| `primitives` | §1.3–1.4, Table 3, §7.10 | `Atoms` (1 PLAU = 10^8 atoms), supply `M = 1.5·10^15`, `Mersenne61` field (`p = 2^61 − 1`), `DomainSeparatedHash` + `MockXof`, `BlockResourceVector` (5 components), PQ object sizes |
| `work`       | §3.1–3.2, §5.1–5.2 | 512×512 INT8 `WorkUnit` over `[-127, 127]`, INT32 `AccumulationTarget` with `\|z\| ≤ 8,258,048 < 2^23`, exact tile product, 16-field Fresh Work context, seed expansion by byte rejection, `verify_freivalds_rounds` (`r_F = 6`) |
| `scivm`      | §3.6      | `OzakiIIProfile` (pairwise-coprime moduli ≤ 255, `Q > 2·B_rec`), `split_residues`, centered CRT, exact residue dot product, zero consensus weight |
| `scheduler`  | §6.1–6.5  | `ShareID`, sequential D'Hondt (`w_s/(a_s+1)` by exact cross-multiplication, ties to least ShareID), owner capacity, persistent cohort counters |
| `ledger`     | §7.1–7.8  | 8 disjoint `StateCategory` balances, atomic zero-sum deltas, `FounderVesting` (1,460 daily tranches × 720 blocks), `FeeLot` with ceiling budgets, floor payouts and segregated dust |
| `poc`        | —         | Binary that runs Genesis → Fresh Work → D'Hondt → Base Fee lock/maturation/payout |

## PoC placeholders

- **Hashing:** `DomainSeparatedHash` and `MockXof` are deterministic but *not cryptographic*.
  They stand in for SHA3-512 and SHAKE256 and keep the normative interface: a domain label,
  an injective length-prefixed encoding, and 64-byte digests.
- **Not implemented:** ML-DSA-87, SLH-DSA, ML-KEM, the Circle STARK, P-DAL and zk-TWL notes.
- **Share weights:** `ShareWeight` is `u64`. The normative `w_s = ⌊2^640/Θ_S⌋` needs a bignum.
  With `u64` weights, every D'Hondt and payout cross-product is exact in `u128`.
- **Aggregate balances:** `U`, `S_z`, `B_bond` and `V_L2` hold totals only, with no per-UTXO
  ownership.
- **Deployment parameters:** epoch length, maturation delay `L`, `η` and fee tariffs are
  illustrative values in `LedgerParams::poc()`.

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
