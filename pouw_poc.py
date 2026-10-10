#!/usr/bin/env python3
"""PoUW AppChain — executable Proof of Concept of Technical Whitepaper v2.0.

Modelled exactly (integer arithmetic only, floats appear solely in log formatting):
  M3  Fresh Work context (5.1), seed (5.2), instance (5.3), identifiers (5.4), dual lottery (5.5)-(5.7),
      rewards proportional to mature share weight (5.16).
  M4  ShareID weights (6.1), batch size (6.2), sequential D'Hondt with cross-multiplied quotients
      (6.4)-(6.6), completeness and proportionality checks (6.7)-(6.8), leases and phase machine (6.13)-(6.15).
  M5  Balance categories (7.1), atomic delta-sum-zero commits (7.13)-(7.14), lifecycle deltas (7.16),
      supply invariant (7.19), fee schedule and irretrievable Base Fee (7.20)-(7.22), unique identifiers
      (7.23), recycling and geometric reserve payout (7.24)-(7.25).

MOCK (heavy cryptography replaced, interfaces kept):
  * Circle STARK proofs     -> SHA3-512 commitments; soundness comes from real Freivalds, r_F = 6 over F_p.
  * 512 x 512 tile          -> TILE_N x TILE_N tile (INT8 inputs, exact integer product).
  * ML-DSA-87 authorization -> owner == lockingPolicy.
  * P-DAL reconstruction    -> recomputed SHA3-512 data root.
  * Block production        -> fixed clock of BLOCKS_PER_EPOCH heights; the seal lottery is still evaluated.

Standard library only.  Run:  python3 pouw_poc.py
"""

from __future__ import annotations

import hashlib
import os
import sys
from collections import defaultdict
from contextlib import contextmanager
from dataclasses import dataclass, fields as dc_fields
from enum import Enum
from fractions import Fraction
from functools import cmp_to_key
from typing import Callable, Iterator

# ─── Normative parameter core (Table 1, 7.18) ────────────────────────────────────────────────────
ATOMS_PER_TKN = 10**8
TKN = ATOMS_PER_TKN
SUPPLY_TKN = 15_000_000
M_SUPPLY = SUPPLY_TKN * ATOMS_PER_TKN            # 1.5e15 atoms
GENESIS_TEAM = 1_500_000 * TKN                   # U_team^(0): 10 %
GENESIS_RESERVE = 13_500_000 * TKN               # R_a^(0):    90 % Network Emission Reserve
P_FIELD = 2**61 - 1                              # base field F_p (3.1)
FREIVALDS_ROUNDS = 6                             # r_F (3.5)
HASH_SPACE = 2**512                              # lottery values in [0, 2^512)
SHARE_TO_SEAL = 120                              # Θ_S / Θ_B (5.7)
WEIGHT_NUMERATOR = 2**(512 + 128)                # w_s = ⌊2^640 / Θ_S(s)⌋ (6.1)

# ─── PoC deployment parameters (release-manifest values, not normative) ──────────────────────────
TILE_N = 8                                       # MOCK: n = 512 in the spec
EPOCHS = 10
BLOCKS_PER_EPOCH = 3
SHARE_MATURITY_EPOCHS = 1                        # shares found in epoch e form the snapshot S_{e+1}
ANCHOR_DEPTH = 2                                 # stableAnchorID = tip at height h - 1 - ANCHOR_DEPTH
LOCK_EPOCHS = 2                                  # L: R_l[e(h) + L] matures into R_a (7.21)
ETA = Fraction(1, 100)                           # η, rational ≈ 1 - 2^(-1/H_R), H_R ≈ 69 epochs (7.26)
THETA_S_GENESIS = HASH_SPACE // 4                # q_S = 1/4 at genesis
TARGET_SHARES_PER_BLOCK = 2
SERVICE_THETA = Fraction(1, 1)                   # θ in N_svc ≤ θ W_e / c_ref (6.2)
C_REF = 10 << 128                                # reference weight per service unit

# Job budget (7.17) and fee schedule (7.20)
F_BASE = 10 * TKN
V_EXEC = 80 * TKN
V_DA = 5 * TKN
V_REF = 5 * TKN
F_ENTRY = 1 * TKN
TAU_BYTE = 100                                   # atoms per byte-block
B_QUEUE = 2_048                                  # accounted job-state bytes
H_TTL = 6                                        # paid queue retention in blocks
F_QUEUE = F_ENTRY + TAU_BYTE * B_QUEUE * H_TTL   # f_queue > 0

# Leases and deadlines (6.13)-(6.15)
B_LEASE = 25 * TKN                               # bond reserved per concurrent lease
V_SLASH = 10 * TKN
D_AVAIL = 2
TTL_COMP = 2

# Fresh Work program identity (5.1)
PROTOCOL_VERSION = 2
GENERATOR_VERSION = 2
CRYPTO_SUITE_ID = "SuiteGen/ML-DSA-87+SLH-DSA-256s+ML-KEM-1024+SHA3-512/v2"

TEAM = "team"
DA_COMMITTEE = "da-committee"

Matrix = tuple[tuple[int, ...], ...]


# ─── Presentation helpers (display only, never consensus) ────────────────────────────────────────
_COLOR = sys.stdout.isatty() and "NO_COLOR" not in os.environ


def _paint(code: str) -> Callable[[object], str]:
    return (lambda s: f"\033[{code}m{s}\033[0m") if _COLOR else (lambda s: str(s))


BOLD, DIM, RED, GREEN, YELLOW, BLUE, MAGENTA, CYAN = (
    _paint(c) for c in ("1", "2", "31", "32", "33", "34", "35", "36"))


def fmt(atoms: int, signed: bool = False) -> str:
    """Exact atoms -> TKN rendering."""
    sign = "-" if atoms < 0 else ("+" if signed else "")
    whole, frac = divmod(abs(atoms), ATOMS_PER_TKN)
    return f"{sign}{whole:,}.{f'{frac:08d}'.rstrip('0').ljust(2, '0')}"


def hx(digest: bytes) -> str:
    return digest.hex()[:8]


def big(value: int) -> str:
    return f"0x{value:0128x}"[:14] + "…"


def units(weight: int) -> str:
    """An integer weight w_s = ⌊2^128 d_S⌋ shown in units of 2^128, i.e. ≈ d_S."""
    return f"{weight / 2**128:.3f}"


class Log:
    WIDTH = 108

    def banner(self, title: str) -> None:
        print("\n" + BOLD("═" * self.WIDTH))
        print(BOLD(f"  {title}"))
        print(BOLD("═" * self.WIDTH))

    def section(self, title: str) -> None:
        print(f"\n  {CYAN('──')} {BOLD(title)} {CYAN('─' * max(0, self.WIDTH - len(title) - 6))}")

    def info(self, text: str, depth: int = 1) -> None:
        print("  " * (depth + 1) + text)

    def op(self, name: str, ctx: str, delta: dict[str, int]) -> None:
        print(f"    {GREEN('✔')} {BOLD(name.ljust(22))} {ctx}")
        moves = "   ".join(f"Δ{k} {fmt(v, signed=True)}" for k, v in delta.items() if v)
        if moves:
            print(f"        {DIM(moves + '   ΣΔ = 0')}")

    def reject(self, name: str, reason: str) -> None:
        print(f"    {RED('✘')} {BOLD(RED(name.ljust(22)))} {reason}")

    def warn(self, text: str) -> None:
        print(f"    {YELLOW('!')} {YELLOW(text)}")


# ─── Encoding and hashing (1.3)-(1.4), (2.4) ─────────────────────────────────────────────────────
def _enc_field(value: object) -> bytes:
    if isinstance(value, bool):
        raise TypeError("bool has no canonical encoding")
    if isinstance(value, bytes):
        tag, body = b"B", value
    elif isinstance(value, int):
        tag, body = b"I", value.to_bytes(value.bit_length() // 8 + 1, "big", signed=True)
    elif isinstance(value, str):
        tag, body = b"S", value.encode()
    elif isinstance(value, (tuple, list)):
        tag, body = b"T", b"".join(_enc_field(v) for v in value)
    else:
        raise TypeError(f"no canonical encoding for {type(value).__name__}")
    return tag + len(body).to_bytes(4, "big") + body


def enc(label: str, *values: object) -> bytes:
    """Enc(LABEL/v2, f1, …, fk): typed, length-prefixed, domain-separated, injective."""
    raw = label.encode()
    return b"L" + len(raw).to_bytes(4, "big") + raw + b"".join(_enc_field(v) for v in values)


def H512(label: str, *values: object) -> bytes:
    return hashlib.sha3_512(enc(label, *values)).digest()


def shake256(label: str, length: int, *values: object) -> bytes:
    return hashlib.shake_256(enc(label, *values)).digest(length)


def as_int(digest: bytes) -> int:
    return int.from_bytes(digest, "big")


PROGRAM_ID = H512("PROGRAM/v2", "MATMUL-INT8", TILE_N)
AIR_ID = H512("AIR/v2", "circle-stark-matmul", TILE_N)


# ─── Compute primitive: tile, Freivalds, mock STARK (3.1)-(3.5), (5.3) ───────────────────────────
def expand_int8_matrix(label: str, seed: bytes, n: int = TILE_N) -> Matrix:
    """Map(b) = b - 127 over a SHAKE256 stream; b = 255 is rejected, so entries are uniform on [-127, 127]."""
    length = n * n + 32
    while True:
        values = [b - 127 for b in shake256(label, length, seed) if b != 255]
        if len(values) >= n * n:
            return tuple(tuple(values[i * n:(i + 1) * n]) for i in range(n))
        length *= 2


def matmul(a: Matrix, b: Matrix) -> Matrix:
    cols = tuple(zip(*b))
    return tuple(tuple(sum(x * y for x, y in zip(row, col)) for col in cols) for row in a)


def matvec_mod_p(m: Matrix, v: list[int]) -> list[int]:
    return [sum(x * y for x, y in zip(row, v)) % P_FIELD for row in m]


def fiat_shamir_vector(transcript: bytes, rnd: int, n: int) -> list[int]:
    """Uniform v ∈ F_p^n by 61-bit rejection sampling, one domain per round."""
    out: list[int] = []
    counter = 0
    while len(out) < n:
        block = shake256("FS/FREIVALDS/v2", 8 * n, transcript, rnd, counter)
        for i in range(0, len(block), 8):
            candidate = int.from_bytes(block[i:i + 8], "big") & ((1 << 61) - 1)
            if candidate < P_FIELD and len(out) < n:
                out.append(candidate)
        counter += 1
    return out


def freivalds(x: Matrix, y: Matrix, z: Matrix, context: bytes) -> int:
    """(3.5): X(Y v) = Z v over F_p for r_F rounds; vectors drawn after X, Y and Z are bound.
    Returns the number of rounds passed; the claim is accepted iff it equals FREIVALDS_ROUNDS."""
    transcript = H512("FS/TRANSCRIPT/v2", context, x, y, z)
    for rnd in range(FREIVALDS_ROUNDS):
        v = fiat_shamir_vector(transcript, rnd, TILE_N)
        if matvec_mod_p(x, matvec_mod_p(y, v)) != matvec_mod_p(z, v):
            return rnd
    return FREIVALDS_ROUNDS


def mock_stark(statement: str, *public: object) -> bytes:
    """MOCK Circle STARK: a binding commitment to the public statement."""
    return H512("MOCK/STARK/v2", statement, *public)


# ─── Module 3: Fresh Work (5.1)-(5.7) ────────────────────────────────────────────────────────────
@dataclass(frozen=True)
class FreshWorkContext:
    """x: the sixteen fields bound before any matrix computation (5.1)."""
    chainID: bytes
    genesisID: bytes
    protocolVersion: int
    cryptoSuiteID: str
    programID: bytes
    AIR_ID: bytes
    generatorVersion: int
    parentCanonicalID: bytes
    stableAnchorID: bytes
    height: int
    minerPolicyDigest: bytes
    payoutPolicyDigest: bytes
    attemptNonce: int
    bodyCoreRoot: bytes
    parentDerivedDifficultyContext: int
    timeBucket: int

    def encode(self) -> bytes:
        values = [getattr(self, f.name) for f in dc_fields(self)]
        assert len(values) == 16, "(5.1) x binds exactly sixteen fields"
        return enc("FW/CONTEXT/v2", *values)


@dataclass(frozen=True)
class FreshWorkSubmission:
    x: FreshWorkContext
    output: Matrix          # MOCK: Z travels in clear; the spec ships outputRoot and π_FW only
    output_root: bytes
    proof: bytes            # π_FW (MOCK)


@dataclass(frozen=True)
class Share:
    """A mature-able unit of execution entitlement, identified by its ShareID = WorkID (5.4), (6.1)."""
    share_id: bytes
    owner: str
    height: int
    theta_s: int
    weight: int             # w_s = ⌊2^640 / Θ_S(parent)⌋


def seal_target(theta_s: int) -> int:
    return theta_s // SHARE_TO_SEAL


class FreshWork:
    """Eval_FW and VerifyFW: deterministic instance, canonical product, identifiers and lotteries."""

    @staticmethod
    def seed(x: FreshWorkContext) -> bytes:
        return H512("FW/INPUT/v2", x.encode())                                       # (5.2)

    @staticmethod
    def instance(seed: bytes) -> tuple[Matrix, Matrix]:
        return expand_int8_matrix("FW/A", seed), expand_int8_matrix("FW/B", seed)    # (5.3)

    @staticmethod
    def identifiers(x: FreshWorkContext, output_root: bytes) -> tuple[bytes, bytes]:
        x_enc = x.encode()                                                           # (5.4)
        return H512("FW/ID/v2", x_enc, output_root), H512("BLOCK/v2", x_enc, output_root)

    @staticmethod
    def lotteries(work_id: bytes) -> tuple[int, int]:
        return as_int(H512("FW/SHARE/v2", work_id)), as_int(H512("FW/SEAL/v2", work_id))  # (5.5)

    @staticmethod
    def evaluate(x: FreshWorkContext) -> FreshWorkSubmission:
        a, b = FreshWork.instance(FreshWork.seed(x))
        z = matmul(a, b)
        root = H512("FW/OUTPUT/v2", z)
        return FreshWorkSubmission(x, z, root, mock_stark("FW", x.encode(), root))

    @staticmethod
    def verify(sub: FreshWorkSubmission) -> bool:
        a, b = FreshWork.instance(FreshWork.seed(sub.x))
        return (H512("FW/OUTPUT/v2", sub.output) == sub.output_root
                and sub.proof == mock_stark("FW", sub.x.encode(), sub.output_root)
                and freivalds(a, b, sub.output, sub.x.encode()) == FREIVALDS_ROUNDS)


@dataclass(frozen=True)
class ChainView:
    """What a miner reads from the canonical tip before it binds x."""
    chain_id: bytes
    genesis_id: bytes
    parent_id: bytes
    anchor_id: bytes
    height: int
    theta_s: int


class Miner:
    """A Fresh Work producer that also serves commercial tiles as a bonded executor."""

    def __init__(self, name: str, hashpower: list[tuple[int, int]], bond_leases: int,
                 offline_epochs: frozenset[int] = frozenset(), corrupt: bool = False):
        self.name = name
        self.hashpower = sorted(hashpower)             # [(from_epoch, attempts per block)]
        self.bond_leases = bond_leases
        self.offline_epochs = offline_epochs
        self.corrupt = corrupt
        self.miner_policy = H512("MINER/POLICY/v2", name)
        self.payout_policy = H512("PAYOUT/POLICY/v2", name)

    def attempts(self, epoch: int) -> int:
        return next((n for start, n in reversed(self.hashpower) if epoch >= start), 0)

    def mine(self, view: ChainView) -> list[FreshWorkSubmission]:
        """Bind x, evaluate, and submit every attempt that wins the share or the seal lottery."""
        body_core_root = H512("BODY/CORE/v2", self.name, view.height, view.parent_id)
        winners = []
        for nonce in range(self.attempts(view.height // BLOCKS_PER_EPOCH)):
            x = FreshWorkContext(
                chainID=view.chain_id, genesisID=view.genesis_id, protocolVersion=PROTOCOL_VERSION,
                cryptoSuiteID=CRYPTO_SUITE_ID, programID=PROGRAM_ID, AIR_ID=AIR_ID,
                generatorVersion=GENERATOR_VERSION, parentCanonicalID=view.parent_id,
                stableAnchorID=view.anchor_id, height=view.height, minerPolicyDigest=self.miner_policy,
                payoutPolicyDigest=self.payout_policy, attemptNonce=nonce, bodyCoreRoot=body_core_root,
                parentDerivedDifficultyContext=view.theta_s, timeBucket=view.height)
            sub = FreshWork.evaluate(x)
            share_value, seal_value = FreshWork.lotteries(FreshWork.identifiers(x, sub.output_root)[0])
            if share_value < view.theta_s or seal_value < seal_target(view.theta_s):
                winners.append(sub)
        return winners

    def execute(self, x: Matrix, y: Matrix, epoch: int) -> Matrix | None:
        """Serve a leased commercial tile; returns None when offline."""
        if epoch in self.offline_epochs:
            return None
        z = matmul(x, y)
        if self.corrupt:
            z = ((z[0][0] + 1,) + z[0][1:],) + z[1:]
        return z


# ─── Module 5: hybrid state and tokenomics (7.1)-(7.25) ──────────────────────────────────────────
class InvariantViolation(AssertionError):
    pass


class TransitionRejected(Exception):
    pass


@dataclass(frozen=True)
class Utxo:
    owner: str              # MOCK lockingPolicy: the owner's ML-DSA-87 key
    amount: int


class TokenomicsState:
    """Value-bearing components of Σ (7.1) under the supply invariant (7.19)."""

    CATEGORIES = ("U", "S_z", "E_esc", "B_bond", "V_L2", "R_a", "R_l", "P_pending")

    def __init__(self, log: Log):
        self.log = log
        self.utxo: dict[bytes, Utxo] = {}                 # U          outpoint -> output
        self.shielded_pool = 0                            # S_z        zk-TWL notes (unused by the PoC)
        self.escrow: dict[bytes, int] = {}                # E_esc      escrowID -> balance
        self.bond: dict[str, int] = {}                    # B_bond     owner -> B_i
        self.l2_vaults: dict[bytes, int] = {}             # V_L2       rollup vaults (unused by the PoC)
        self.reserve_available = 0                        # R_a
        self.reserve_locked: dict[int, int] = {}          # R_l        maturity epoch -> amount
        self.pending: dict[str, int] = {}                 # P_pending  owner -> segregated payable
        self.used_ids: set[bytes] = set()                 # (7.23)
        self.locked_inflow: dict[tuple[int, str], int] = {}
        self._nonce = 0

    # ── views ──
    def totals(self) -> dict[str, int]:
        return {
            "U": sum(u.amount for u in self.utxo.values()),
            "S_z": self.shielded_pool,
            "E_esc": sum(self.escrow.values()),
            "B_bond": sum(self.bond.values()),
            "V_L2": sum(self.l2_vaults.values()),
            "R_a": self.reserve_available,
            "R_l": sum(self.reserve_locked.values()),
            "P_pending": sum(self.pending.values()),
        }

    def total_supply_atoms(self) -> int:
        return sum(self.totals().values())

    def balance_of(self, owner: str) -> int:
        return sum(u.amount for u in self.utxo.values() if u.owner == owner)

    def check_invariants(self) -> None:
        totals = self.totals()
        if sum(totals.values()) != M_SUPPLY:
            raise InvariantViolation(f"(7.19) Σ = {sum(totals.values())} ≠ M = {M_SUPPLY}")
        balances = [*totals.values(), *self.escrow.values(), *self.bond.values(),
                    *self.reserve_locked.values(), *self.pending.values()]
        if any(v < 0 for v in balances) or any(u.amount <= 0 for u in self.utxo.values()):
            raise InvariantViolation("(7.19) categories must be non-negative")

    # ── commit rule ──
    def _snapshot(self) -> tuple:
        return (dict(self.utxo), self.shielded_pool, dict(self.escrow), dict(self.bond), dict(self.l2_vaults),
                self.reserve_available, dict(self.reserve_locked), dict(self.pending), set(self.used_ids),
                dict(self.locked_inflow), self._nonce)

    def _restore(self, s: tuple) -> None:
        (self.utxo, self.shielded_pool, self.escrow, self.bond, self.l2_vaults, self.reserve_available,
         self.reserve_locked, self.pending, self.used_ids, self.locked_inflow, self._nonce) = s

    @contextmanager
    def atomic(self, op: str, ctx: str = "") -> Iterator[None]:
        """(7.13)-(7.14): Σ' = Σ ⊕ Δ iff Pre and Inv hold and the deltas sum to zero; otherwise Σ' = Σ."""
        saved, before = self._snapshot(), self.totals()
        try:
            yield
            delta = {k: v - before[k] for k, v in self.totals().items()}
            if sum(delta.values()) != 0:
                raise InvariantViolation(f"(7.13) {op}: ΣΔ = {sum(delta.values())}")
            if delta["R_l"] < 0 and op != "RESERVE_MATURE":
                raise InvariantViolation(f"(7.21) {op} moves value out of the locked reserve")
            self.check_invariants()
        except BaseException:
            self._restore(saved)
            raise
        self.log.op(op, ctx, delta)

    def claim_id(self, kind: str, ident: bytes) -> None:
        """(7.23): every ShareID, admissionID, receiptID and rewardID is used at most once."""
        if ident in self.used_ids:
            raise TransitionRejected(f"duplicate {kind} {hx(ident)}")
        self.used_ids.add(ident)

    # ── primitive moves ──
    def _txid(self, op: str, *values: object) -> bytes:
        self._nonce += 1
        return H512("TX/EFFECTS/v2", op, self._nonce, *values)      # MOCK TxCore (7.4)-(7.5)

    def _credit(self, owner: str, amount: int, txid: bytes, index: int) -> None:
        if amount > 0:
            self.utxo[H512("UTXO/OUTPOINT/v2", txid, index)] = Utxo(owner, amount)

    def _debit(self, owner: str, amount: int, txid: bytes) -> None:
        """Consume the owner's outputs in canonical outpoint order and return change (7.3)."""
        gathered = 0
        for outpoint in sorted(op for op, u in self.utxo.items() if u.owner == owner):
            if gathered >= amount:
                break
            gathered += self.utxo.pop(outpoint).amount
        if gathered < amount:
            raise TransitionRejected(f"{owner}: insufficient funds ({fmt(gathered)} < {fmt(amount)})")
        self._credit(owner, gathered - amount, txid, 0)

    def _lock(self, epoch: int, amount: int, kind: str) -> int:
        """R_l[e(h) + L] += amount; returns the maturity epoch."""
        maturity = epoch + LOCK_EPOCHS
        self.reserve_locked[maturity] = self.reserve_locked.get(maturity, 0) + amount
        self.locked_inflow[(epoch, kind)] = self.locked_inflow.get((epoch, kind), 0) + amount
        return maturity

    # ── operations (7.2), deltas (7.16) ──
    def genesis(self) -> None:
        self._credit(TEAM, GENESIS_TEAM, H512("GENESIS/v2", "pouw-appchain-poc"), 0)
        self.reserve_available = GENESIS_RESERVE
        self.check_invariants()
        self.log.op("GENESIS", f"U_team = {fmt(GENESIS_TEAM)}, R_a = {fmt(GENESIS_RESERVE)} TKN "
                               f"(M = {M_SUPPLY:,} atoms)", {})

    def twl_transfer(self, sender: str, recipient: str, amount: int) -> None:
        with self.atomic("TWL_TRANSFER", f"{sender} → {recipient}  {fmt(amount)} TKN"):
            txid = self._txid("TWL_TRANSFER", sender, recipient, amount)
            self._debit(sender, amount, txid)
            self._credit(recipient, amount, txid, 1)

    def bond_lock(self, owner: str, amount: int) -> None:
        with self.atomic("BOND_LOCK", f"{owner} pledges {fmt(amount)} TKN = {amount // B_LEASE} × b_lease"):
            self._debit(owner, amount, self._txid("BOND_LOCK", owner, amount))
            self.bond[owner] = self.bond.get(owner, 0) + amount

    def bond_slash(self, owner: str, amount: int, epoch: int, ctx: str) -> None:
        with self.atomic("BOND_SLASH", ctx):
            if self.bond.get(owner, 0) < amount:
                raise TransitionRejected(f"{owner}: bond below slash amount")
            self.bond[owner] -= amount
            self._lock(epoch, amount, "slash")

    def escrow_fund_and_queue(self, client: str, escrow_id: bytes, deposit: int, f_queue: int,
                              epoch: int, ctx: str) -> None:
        """ΔU = -(D + f_queue), ΔE_esc = D, ΔR_l = f_queue."""
        with self.atomic("ESCROW_FUND_AND_QUEUE", ctx):
            if escrow_id in self.escrow:
                raise TransitionRejected("escrowID already funded")
            self._debit(client, deposit + f_queue, self._txid("ESCROW_FUND_AND_QUEUE", escrow_id))
            self.escrow[escrow_id] = deposit
            self._lock(epoch, f_queue, "queue fee")

    def collect_base_fee(self, op: str, escrow_id: bytes, admission_id: bytes, epoch: int, ctx: str) -> None:
        """JOB_ADMIT_AND_LEASE / LEASE_RETRY: E_J ← E_J - F_base, R_l[e(h_admit) + L] += F_base (7.21)."""
        with self.atomic(op, ctx):
            self.claim_id("admissionID", admission_id)
            if self.escrow.get(escrow_id, 0) < F_BASE:
                raise TransitionRejected("escrow cannot cover the Base Fee")
            self.escrow[escrow_id] -= F_BASE
            self._lock(epoch, F_BASE, "Base Fee")

    def escrow_release(self, op: str, escrow_id: bytes, payouts: list[tuple[str, int]], ctx: str,
                       receipt_id: bytes | None = None) -> None:
        """RESULT_SETTLE: ΔE_esc = -(v_exec + v_DA + v_ref), ΔU = +(…).  ESCROW_REFUND: ΔE_esc = -v_ref, ΔU = v_ref."""
        with self.atomic(op, ctx):
            if receipt_id is not None:
                self.claim_id("receiptID", receipt_id)              # R' = R ∪ {receiptID}
            total = sum(amount for _, amount in payouts)
            if self.escrow.get(escrow_id, 0) < total:
                raise TransitionRejected("(7.15) E_esc below the payout")
            txid = self._txid(op, escrow_id)
            self.escrow[escrow_id] -= total
            for index, (owner, amount) in enumerate(payouts, start=1):
                self._credit(owner, amount, txid, index)
            if self.escrow[escrow_id] == 0:
                del self.escrow[escrow_id]

    def reserve_mature(self, epoch: int) -> int:
        """RESERVE_MATURE: ΔR_l = -v_mat, ΔR_a = v_mat."""
        amount = self.reserve_locked.get(epoch, 0)
        if amount:
            with self.atomic("RESERVE_MATURE", f"R_l[{epoch}] → R_a  {fmt(amount)} TKN recycled (never burned)"):
                del self.reserve_locked[epoch]
                self.reserve_available += amount
        return amount

    def epoch_reward_budget(self) -> int:
        """Asymptotic payout E_e = η R_e (7.25): a fixed rational fraction of the available reserve."""
        return self.reserve_available * ETA.numerator // ETA.denominator

    def distribute_epoch_rewards(self, epoch: int, weights: dict[str, int]) -> dict[str, int]:
        """Reward_{i,e} = ⌊E_e w_{i,e} / Σ_j w_{j,e}⌋ (5.16); remainders stay in R_a."""
        budget = self.epoch_reward_budget()
        self.log.info(f"▸ reward budget E_{epoch} = η·R_a = {ETA} × {fmt(self.reserve_available)} "
                      f"= {BOLD(fmt(budget))} TKN   (7.25)")
        total = sum(weights.values())
        if total == 0:
            self.log.info(DIM("no mature shares in this window: E_e stays in R_a"), 2)
            return {}
        rewards = {owner: budget * w // total for owner, w in sorted(weights.items())}
        for owner, reward in rewards.items():
            self.log.info(f"{owner:<9} w_i/Σw = {100 * weights[owner] / total:6.2f} %   "
                          f"Reward = ⌊E·w_i/Σw⌋ = {fmt(reward)}", 2)
        v_seg = sum(rewards.values())
        with self.atomic("EPOCH_BUDGET", f"R_a → P_pending  v_seg = {fmt(v_seg)} ≤ E_e  "
                                         f"(remainder {fmt(budget - v_seg)} stays in R_a)"):
            self.reserve_available -= v_seg
            for owner, reward in rewards.items():
                self.pending[owner] = self.pending.get(owner, 0) + reward
        for owner in rewards:
            with self.atomic("FW_REWARD", f"P_pending → {owner}  {fmt(rewards[owner])} TKN"):
                self.claim_id("rewardID", H512("REWARD/v2", epoch, owner))
                amount = self.pending.pop(owner)
                self._credit(owner, amount, self._txid("FW_REWARD", epoch, owner), 1)
        return rewards


# ─── Module 4: ShareID-indexed sequential D'Hondt (6.1)-(6.8) ────────────────────────────────────
@dataclass
class Cohort:
    epoch: int
    sources: list[Share]                 # S, canonical ShareID order
    total_weight: int                    # W
    allocated: dict[bytes, int]          # a_s, persists across the batches of the cohort
    service_budget: int                  # remaining N_svc
    slots: int = 0                       # N_c
    capacity_bound: bool = False


class DHondtScheduler:
    def __init__(self, log: Log):
        self.log = log
        self.cohort: Cohort | None = None
        self._traced = False

    def open_cohort(self, epoch: int, snapshot: list[Share]) -> None:
        sources = sorted(snapshot, key=lambda s: s.share_id)
        total = sum(s.weight for s in sources)
        n_svc = total * SERVICE_THETA.numerator // (SERVICE_THETA.denominator * C_REF)
        self.cohort = Cohort(epoch, sources, total, {s.share_id: 0 for s in sources}, n_svc)
        if not sources:
            self.log.info(f"▸ D'Hondt cohort S_{epoch} = ∅ (no mature ShareIDs yet) → no execution slots")
            return
        self.log.info(f"▸ D'Hondt cohort S_{epoch}: |S| = {len(sources)} mature ShareIDs, "
                      f"W = Σw_s = {units(total)}·2^128, N_svc = ⌊θW/c_ref⌋ = {n_svc}   (6.1)-(6.2)")
        by_owner: dict[str, list[Share]] = defaultdict(list)
        for s in sources:
            by_owner[s.owner].append(s)
        for owner, owned in sorted(by_owner.items()):
            w = sum(s.weight for s in owned)
            self.log.info(f"{owner:<9} {len(owned):>2} ShareID(s)   W_E/W = {100 * w / total:6.2f} %", 2)

    @staticmethod
    def prefers(s: Share, r: Share, a: dict[bytes, int]) -> bool:
        """(6.5): w_s/(a_s+1) > w_r/(a_r+1) ⟺ w_s(a_r+1) > w_r(a_s+1); ties go to the least ShareID."""
        lhs, rhs = s.weight * (a[r.share_id] + 1), r.weight * (a[s.share_id] + 1)
        return lhs > rhs or (lhs == rhs and s.share_id < r.share_id)

    def _rank(self, candidates: list[Share], a: dict[bytes, int]) -> list[Share]:
        return sorted(candidates, key=cmp_to_key(lambda s, r: -1 if self.prefers(s, r, a) else 1))

    def _recursion(self, a: dict[bytes, int], capacity: dict[str, int] | None, steps: int,
                   observe: Callable[[int, list[Share], dict[bytes, int]], None] | None = None) -> list[Share]:
        """(6.4): s*_t = argmax_{s ∈ E_t} w_s/(a_s+1); a_s += 1; K_owner -= 1; while t < N and E_t ≠ ∅."""
        chosen = []
        for t in range(steps):
            eligible = [s for s in self.cohort.sources if capacity is None or capacity.get(s.owner, 0) > 0]
            if not eligible:
                break
            ranked = self._rank(eligible, a)
            if observe:
                observe(t, ranked, a)
            s_star = ranked[0]
            a[s_star.share_id] += 1
            if capacity is not None:
                capacity[s_star.owner] -= 1
            chosen.append(s_star)
        return chosen

    def allocate(self, n_ready: int, capacity: dict[str, int]) -> list[Share]:
        c = self.cohort
        owners = sorted({s.owner for s in c.sources})
        k_free = sum(capacity.get(o, 0) for o in owners)
        n = min(c.service_budget, n_ready, k_free)
        self.log.info(f"N = min(N_svc {c.service_budget}, N_ready {n_ready}, ΣK_free {k_free}) = {BOLD(n)}   (6.2)", 2)
        blocked = [o for o in owners if capacity.get(o, 0) <= 0]
        if blocked and n:
            self.log.info(DIM(f"K_free = 0 → ShareIDs of {', '.join(blocked)} are outside E_t"), 2)
        if n == 0:
            return []
        show_exact = not self._traced
        if show_exact:
            self._traced = True
            self._trace_quotients(c.allocated)

        def quotient(s: Share, a: dict[bytes, int]) -> str:
            return f"{units(s.weight)}/{a[s.share_id] + 1} = {s.weight / 2**128 / (a[s.share_id] + 1):.3f}"

        def observe(t: int, ranked: list[Share], a: dict[bytes, int]) -> None:
            nonlocal show_exact
            s_star = ranked[0]
            line = (f"t={t}  s* = {MAGENTA(hx(s_star.share_id))} {s_star.owner:<8}"
                    f"q = {quotient(s_star, a):<15}  a_s {a[s_star.share_id]}→{a[s_star.share_id] + 1}")
            if len(ranked) > 1:
                r = ranked[1]
                lhs, rhs = s_star.weight * (a[r.share_id] + 1), r.weight * (a[s_star.share_id] + 1)
                line += DIM(f"   ≻ {hx(r.share_id)} {r.owner:<8}q = {quotient(r, a)}"
                            f"{'' if lhs > rhs else '  (tie → least ShareID)'}")
                if show_exact:
                    show_exact = False
                    self.log.info(line, 2)
                    self.log.info(DIM(f"     exact (6.5): w_s·(a_r+1) = {lhs} {'>' if lhs > rhs else '='} "
                                      f"w_r·(a_s+1) = {rhs}"), 2)
                    line = ""
            unconstrained = self._rank(c.sources, a)[0]
            if unconstrained is not s_star:
                c.capacity_bound = True
                line += YELLOW(f"   [K=0 binds: {unconstrained.owner} {hx(unconstrained.share_id)}]")
            if line:
                self.log.info(line, 2)

        winners = self._recursion(c.allocated, dict(capacity), n, observe)
        if len(winners) != n:
            raise InvariantViolation("(6.7) Theorem 6.1: a batch allocates exactly its N feasible slots")
        c.slots += n
        c.service_budget -= n
        return winners

    def _trace_quotients(self, a: dict[bytes, int]) -> None:
        self.log.info(DIM("┌ D'Hondt trace — quotient table of the first non-empty batch"), 2)
        self.log.info(DIM(f"│ {'ShareID':<10}{'owner':<9}{'found at':<10}{'w_s/2^128 ≈ d_S':<18}{'a_s':<5}w_s/(a_s+1)"), 2)
        for s in self._rank(self.cohort.sources, a):
            self.log.info(DIM(f"│ {hx(s.share_id):<10}{s.owner:<9}{'h=' + str(s.height):<10}{units(s.weight):<18}"
                              f"{a[s.share_id]:<5}{s.weight / 2**128 / (a[s.share_id] + 1):.3f}"), 2)
        self.log.info(DIM("└ heavier ShareIDs (harder parent target Θ_S) come first; equal quotients go to the "
                          "least ShareID"), 2)

    def close_cohort(self) -> None:
        """Checks the batch sequence against the capacity-free recursion, (6.6) and Theorem 6.2."""
        c = self.cohort
        if c is None or c.slots == 0:
            return
        reference = {s.share_id: 0 for s in c.sources}
        self._recursion(reference, None, c.slots)
        shortcut = {s.share_id: c.slots * s.weight // c.total_weight for s in c.sources}
        n_rem = c.slots - sum(shortcut.values())
        assert 0 <= n_rem < len(c.sources), "(6.6) N_rem < |S|"
        self._recursion(shortcut, None, n_rem)
        assert shortcut == reference, "(6.6) floor-plus-remainder shortcut diverges from (6.4)"
        for s in c.sources:
            a_s = reference[s.share_id]
            assert c.slots * s.weight // c.total_weight <= a_s, "(6.8) lower bound"
            assert a_s * c.total_weight <= (c.slots + len(c.sources)) * s.weight, "(6.8) upper bound"
        if not c.capacity_bound:
            assert c.allocated == reference, "(6.4) batches of one cohort continue a single recursion"

        self.log.info(f"▸ cohort S_{c.epoch} closed: N_c = {c.slots} slots over |S| = {len(c.sources)} ShareIDs")
        totals: dict[str, list[int]] = defaultdict(lambda: [0, 0, 0])
        for s in c.sources:
            row = totals[s.owner]
            row[0] += c.allocated[s.share_id]
            row[1] += reference[s.share_id]
            row[2] += s.weight
        for owner, (live, free, w) in sorted(totals.items()):
            self.log.info(f"{owner:<9} a_E = {live}   capacity-free a_E = {free}   "
                          f"N_c·W_E/W = {c.slots * w / c.total_weight:5.2f}", 2)
        self.log.info(DIM(f"(6.6) shortcut Σb_s = {c.slots - n_rem}, N_rem = {n_rem} ≡ recursion ✓   "
                          f"(6.8) ⌊N_c w_s/W⌋ ≤ a_s ≤ (N_c+|S|) w_s/W for every ShareID ✓"), 2)
        if c.capacity_bound:
            self.log.info(DIM("owner capacity bound in this cohort: the live allocation follows (6.4) with K, "
                              "which is normative"), 2)


# ─── Commercial job: phase machine and escrow cycle (6.13)-(6.15), (7.15)-(7.22) ─────────────────
class Phase(Enum):
    FUNDED = "Funded"
    ADMITTED = "Admitted"
    RUNNING = "Running"
    RETRY = "Retry"
    SETTLED = "Settled"
    REFUNDED = "Refunded"


TRANSITIONS = {                                           # (6.14)
    (None, Phase.FUNDED): "ESCROW_FUND_AND_QUEUE",
    (Phase.FUNDED, Phase.ADMITTED): "JOB_ADMIT_AND_LEASE",
    (Phase.FUNDED, Phase.REFUNDED): "ESCROW_REFUND",
    (Phase.ADMITTED, Phase.RUNNING): "INPUT_READY",
    (Phase.ADMITTED, Phase.RETRY): "LEASE_TIMEOUT",       # data window missed: client fault (6.15)
    (Phase.RUNNING, Phase.SETTLED): "RESULT_SETTLE",
    (Phase.RUNNING, Phase.RETRY): "LEASE_TIMEOUT",        # delivery deadline missed: executor fault
    (Phase.RETRY, Phase.ADMITTED): "LEASE_RETRY",
    (Phase.RETRY, Phase.REFUNDED): "ESCROW_REFUND",
}


@dataclass
class Lease:
    """Lease = (jobID, tileID, leaseVersion, executor, escrowID, price, B_res, h_deadline) (6.13)."""
    job_id: bytes
    tile_id: bytes
    lease_version: int
    executor: str
    escrow_id: bytes
    price: int
    bond_reserved: int
    share_id: bytes
    h_admit: int
    h_ready: int | None = None
    h_deadline: int | None = None
    executor_acted: bool = False


def epoch_of(h: int) -> int:
    return h // BLOCKS_PER_EPOCH


class CommercialJob:
    """A single-tile commercial job; it carries zero consensus weight (1.1)."""

    def __init__(self, client: str, nonce: int, retry_budget: int, h: int):
        self.client = client
        data_seed = H512("CLIENT/DATA/v2", client, nonce)
        self.x = expand_int8_matrix("CLIENT/A", data_seed)
        self.y = expand_int8_matrix("CLIENT/B", data_seed)
        self.data_root = H512("PDAL/ROOT/v2", self.x, self.y)            # MOCK RS-1/8 Merkle root (4.2)
        self.retry_budget = retry_budget                                   # β_retry
        self.deposit = F_BASE + V_EXEC + V_DA + V_REF + retry_budget * F_BASE
        manifest = H512("JOB/MANIFEST/v2", PROGRAM_ID, TILE_N, self.data_root, self.deposit, retry_budget)
        self.job_id = H512("JOB/ID/v2", client, nonce, manifest)
        self.tile_id = H512("TILE/ID/v2", self.job_id, 0)
        self.escrow_id = H512("ESCROW/ID/v2", self.job_id)
        self.phase: Phase | None = None
        self.lease: Lease | None = None
        self.lease_version = 0
        self.h_fund = h
        self.ready_since = h
        self.fees_paid = 0

    @property
    def tag(self) -> str:
        return f"job {BLUE(hx(self.job_id))} ({self.client})"

    def _op_for(self, target: Phase) -> str:
        op = TRANSITIONS.get((self.phase, target))
        if op is None:
            raise InvariantViolation(f"(6.14) illegal transition {self.phase} → {target}")
        return op

    def fund(self, state: TokenomicsState, h: int) -> None:
        """⊥ → Funded: the client's deposit D enters escrow; f_queue is locked at once."""
        self._op_for(Phase.FUNDED)
        retry = f" + β·F_base {fmt(self.retry_budget * F_BASE)}" if self.retry_budget else ""
        state.escrow_fund_and_queue(
            self.client, self.escrow_id, self.deposit, F_QUEUE, epoch_of(h),
            f"{self.tag} → escrow {hx(self.escrow_id)}: D = {fmt(self.deposit)}, "
            f"f_queue {fmt(F_QUEUE)} → R_l[{epoch_of(h) + LOCK_EPOCHS}]")
        state.log.info(DIM(f"D = F_base {fmt(F_BASE)} + v_exec {fmt(V_EXEC)} + v_DA {fmt(V_DA)} "
                           f"+ v_ref {fmt(V_REF)}{retry}   (7.17), β_retry = {self.retry_budget}"), 3)
        self.fees_paid += F_QUEUE
        self.phase = Phase.FUNDED

    def admit(self, state: TokenomicsState, lease: Lease, h: int) -> None:
        """Funded → Admitted or Retry → Admitted: the Base Fee leaves the escrow irretrievably (7.21)."""
        op = self._op_for(Phase.ADMITTED)
        if op == "LEASE_RETRY" and self.retry_budget == 0:
            raise TransitionRejected("β_retry = 0")
        maturity = epoch_of(h) + LOCK_EPOCHS
        state.collect_base_fee(
            op, self.escrow_id, H512("ADMISSION/v2", self.job_id, lease.lease_version), epoch_of(h),
            f"{self.tag} lease v{lease.lease_version} → {BOLD(lease.executor)} via ShareID {hx(lease.share_id)}: "
            f"Base Fee {fmt(F_BASE)} escrow → R_l[{maturity}] (irretrievable)")
        if op == "LEASE_RETRY":
            self.retry_budget -= 1
        self.fees_paid += F_BASE
        self.lease, self.lease_version, self.phase = lease, lease.lease_version, Phase.ADMITTED

    def input_ready(self, state: TokenomicsState, h: int) -> None:
        """Admitted → Running iff h ≤ h_admit + D_avail; then h_deadline = h_ready + TTL_comp (6.15)."""
        self._op_for(Phase.RUNNING)
        lease = self.lease
        if h > lease.h_admit + D_AVAIL:
            raise TransitionRejected("data window closed")
        with state.atomic("INPUT_READY", f"{self.tag} matrices published to P-DAL, root {hx(self.data_root)}; "
                                         f"h_deadline = {h} + TTL_comp = {h + TTL_COMP}"):
            lease.h_ready, lease.h_deadline = h, h + TTL_COMP
        self.phase = Phase.RUNNING

    def settle(self, state: TokenomicsState, executor: str, lease_version: int, output: Matrix,
               published: tuple[Matrix, Matrix], h: int) -> None:
        """Running → Settled under Pre_settle (7.15): payment to the executor, DA and refund to the client."""
        self._op_for(Phase.SETTLED)
        lease = self.lease
        if (lease.executor, lease.lease_version) != (executor, lease_version):
            raise TransitionRejected("LeaseMatch = 0")
        if h > lease.h_deadline:
            raise TransitionRejected("h > h_deadline")
        if H512("PDAL/ROOT/v2", *published) != self.data_root:
            raise TransitionRejected("Admit_DA = 0")
        output_root = H512("JOB/OUTPUT/v2", output)
        passed = freivalds(published[0], published[1], output, self.job_id + output_root)
        if passed != FREIVALDS_ROUNDS:
            raise TransitionRejected(f"ComputeValid = 0: Freivalds round {passed + 1} finds X(Yv) ≠ Zv")
        refund = state.escrow.get(self.escrow_id, 0) - V_EXEC - V_DA          # v_ref + unused retry budget
        if refund < V_REF:
            raise TransitionRejected("(7.15) E_esc < v_exec + v_DA + v_ref")
        receipt_id = H512("RECEIPT/v2", self.job_id, self.tile_id, lease_version, executor, output_root)
        state.escrow_release(
            "RESULT_SETTLE", self.escrow_id, [(executor, V_EXEC), (DA_COMMITTEE, V_DA), (self.client, refund)],
            f"{self.tag} receipt {hx(receipt_id)}: Freivalds {FREIVALDS_ROUNDS}/{FREIVALDS_ROUNDS} ✓  "
            f"v_exec {fmt(V_EXEC)} → {BOLD(executor)}, v_DA {fmt(V_DA)} → {DA_COMMITTEE}, "
            f"v_ref {fmt(refund)} → {self.client}", receipt_id=receipt_id)
        self.lease, self.phase = None, Phase.SETTLED

    def expire(self, state: TokenomicsState, h: int) -> Lease:
        """Admitted → Retry (missed data window) or Running → Retry (missed delivery deadline)."""
        op = self._op_for(Phase.RETRY)
        lease = self.lease
        reason = (f"no INPUT_READY by h_admit + D_avail = {lease.h_admit + D_AVAIL}" if self.phase is Phase.ADMITTED
                  else f"{lease.executor} missed h_deadline = {lease.h_deadline}")
        with state.atomic(op, f"{self.tag} lease v{lease.lease_version}: {reason} → Retry "
                              f"(β_retry = {self.retry_budget})"):
            pass
        self.lease, self.phase, self.ready_since = None, Phase.RETRY, h
        return lease

    def refund(self, state: TokenomicsState, reason: str) -> None:
        """Funded/Retry → Refunded: everything left in E_J returns to the client; locked fees do not."""
        self._op_for(Phase.REFUNDED)
        amount = state.escrow.get(self.escrow_id, 0)
        state.escrow_release(
            "ESCROW_REFUND", self.escrow_id, [(self.client, amount)],
            f"{self.tag} {reason}: v_ref {fmt(amount)} → {self.client};  "
            f"fees forfeited = {RED(fmt(self.fees_paid))} (locked in R_l, never returned)")
        self.phase = Phase.REFUNDED


# ─── The node: one canonical state shared by every module ────────────────────────────────────────
@dataclass
class Client:
    name: str
    publishes_data: bool = True
    nonce: int = 0


class PoUWNode:
    def __init__(self, log: Log, miners: list[Miner], clients: list[Client]):
        self.log = log
        self.state = TokenomicsState(log)
        self.scheduler = DHondtScheduler(log)
        self.miners = {m.name: m for m in miners}
        self.clients = {c.name: c for c in clients}
        self.payout_owner = {m.payout_policy: m.name for m in miners}
        self.jobs: dict[bytes, CommercialJob] = {}
        self.active_leases: dict[bytes, Lease] = {}
        self.bond_reserved: dict[str, int] = defaultdict(int)
        self.shares_by_epoch: dict[int, list[Share]] = defaultdict(list)
        self.da_store: dict[bytes, tuple[Matrix, Matrix]] = {}
        self.theta_s = THETA_S_GENESIS
        self.chain_id = H512("CHAIN/ID/v2", "pouw-appchain-poc")
        self.genesis_id = H512("GENESIS/ID/v2", self.chain_id)
        self.tips = [self.genesis_id]
        self.chain_weight = 0
        self.stats: dict[str, dict[str, int]] = defaultdict(lambda: defaultdict(int))
        self.reserve_trace: list[tuple[int, int, int]] = []
        self._epoch_open_totals: dict[str, int] = {}
        self._traced_share = False

    # ── helpers ──
    def capacity_free(self, owner: str) -> int:
        """K_i^free = ⌊(B_i - Σ B_res) / b_lease⌋ (6.2), (6.13)."""
        return max(0, (self.state.bond.get(owner, 0) - self.bond_reserved[owner]) // B_LEASE)

    def jobs_in(self, *phases: Phase) -> list[CommercialJob]:
        return sorted((j for j in self.jobs.values() if j.phase in phases), key=lambda j: (j.h_fund, j.job_id))

    def _release_lease(self, lease: Lease) -> None:
        del self.active_leases[lease.tile_id]
        self.bond_reserved[lease.executor] -= lease.bond_reserved
        self.log.info(DIM(f"B_res {fmt(lease.bond_reserved)} of {lease.executor} released "
                          f"→ K_free({lease.executor}) = {self.capacity_free(lease.executor)}"), 3)

    # ── genesis ──
    def genesis(self, grants: dict[str, int]) -> None:
        self.log.banner("GENESIS  ·  supply M = 15,000,000 TKN = 1.5·10^15 atoms   (7.18)")
        self.state.genesis()
        for who, amount in grants.items():
            self.state.twl_transfer(TEAM, who, amount)
        for miner in self.miners.values():
            self.state.bond_lock(miner.name, miner.bond_leases * B_LEASE)

    # ── epoch boundaries ──
    def open_epoch(self, e: int) -> None:
        self.log.banner(f"EPOCH {e}  ·  heights {e * BLOCKS_PER_EPOCH}–{(e + 1) * BLOCKS_PER_EPOCH - 1}")
        self._epoch_open_totals = self.state.totals()
        self.state.reserve_mature(e)
        snapshot = self.shares_by_epoch.get(e - SHARE_MATURITY_EPOCHS, [])
        weights: dict[str, int] = defaultdict(int)
        for share in snapshot:
            weights[share.owner] += share.weight
        budget = self.state.epoch_reward_budget()
        rewards = self.state.distribute_epoch_rewards(e, dict(weights))
        for owner, reward in rewards.items():
            self.stats[owner]["rewards"] += reward
        self.reserve_trace.append((e, self.state.reserve_available + sum(rewards.values()), budget))
        self.scheduler.open_cohort(e, snapshot)

    def close_epoch(self, e: int) -> None:
        self.scheduler.close_cohort()
        totals, opened = self.state.totals(), self._epoch_open_totals
        self.log.section(f"epoch {e} ledger (7.19)")
        labels = {"U": "public UTXOs (circulating)", "S_z": "shielded pool", "E_esc": "escrow",
                  "B_bond": "executor bonds", "V_L2": "L2 vaults", "R_a": "reserve, available",
                  "R_l": "reserve, locked", "P_pending": "reward payables"}
        for key, label in labels.items():
            change = totals[key] - opened[key]
            self.log.info(f"{key:<10}{label:<28}{fmt(totals[key]):>22}   "
                          f"{DIM(f'epoch Δ {fmt(change, signed=True)}') if change else ''}")
        self.log.info("─" * 64)
        self.log.info(f"{'Σ':<38}{fmt(sum(totals.values())):>22}   = M")
        buckets = ", ".join(f"R_l[{k}] = {fmt(v)}" for k, v in sorted(self.state.reserve_locked.items()))
        inflow = {k: self.state.locked_inflow.get((e, k), 0) for k in ("Base Fee", "queue fee", "slash")}
        if sum(inflow.values()):
            self.log.info(f"fee sink → R_l this epoch: Base Fees {fmt(inflow['Base Fee'])} + queue fees "
                          f"{fmt(inflow['queue fee'])} + slashes {fmt(inflow['slash'])} = "
                          f"{BOLD(fmt(sum(inflow.values())))} TKN withdrawn from circulation until epoch "
                          f"{e + LOCK_EPOCHS}")
        if buckets:
            self.log.info(DIM(f"locked buckets: {buckets}"))

    # ── block pipeline ──
    def run_block(self, h: int, actions: list[tuple]) -> None:
        self.log.section(f"h={h} · epoch {epoch_of(h)} · block {h % BLOCKS_PER_EPOCH + 1}/{BLOCKS_PER_EPOCH} "
                         f"· Θ_S ⇒ d_S ≈ {HASH_SPACE / self.theta_s:.3f}, Θ_B = Θ_S/{SHARE_TO_SEAL}")
        self.client_round(h, actions)
        self.fresh_work_round(h)
        self.executor_round(h)
        self.data_round(h)
        self.deadline_round(h)
        self.scheduler_round(h)
        self.state.check_invariants()
        for owner in self.miners:
            if self.bond_reserved[owner] > self.state.bond.get(owner, 0):
                raise InvariantViolation(f"(6.13) Σ B_res > B_i for {owner}")

    def client_round(self, h: int, actions: list[tuple]) -> None:
        for action, who, *args in actions:
            client = self.clients[who]
            if action == "fund":
                job = CommercialJob(who, client.nonce, retry_budget=args[0], h=h)
                client.nonce += 1
                try:
                    job.fund(self.state, h)
                except TransitionRejected as err:
                    self.log.reject("ESCROW_FUND_AND_QUEUE", str(err))
                    continue
                self.jobs[job.job_id] = job
            elif action == "cancel":
                queued = [j for j in self.jobs_in(Phase.FUNDED) if j.client == who]
                if queued:
                    queued[-1].refund(self.state, "cancelled while Funded (F_base never charged)")

    def fresh_work_round(self, h: int) -> None:
        view = ChainView(self.chain_id, self.genesis_id, self.tips[-1],
                         self.tips[max(0, len(self.tips) - 1 - ANCHOR_DEPTH)], h, self.theta_s)
        found = 0
        for miner in self.miners.values():
            attempts = miner.attempts(epoch_of(h))
            if attempts == 0:
                continue
            seals: list[str] = []
            accepted = [s for s in (self.submit_fresh_work(sub, view, seals) for sub in miner.mine(view)) if s]
            self.stats[miner.name]["attempts"] += attempts
            self.stats[miner.name]["shares"] += len(accepted)
            found += len(accepted)
            ids = " ".join(hx(s.share_id) for s in accepted)
            self.log.info(f"⛏  {miner.name:<8} {attempts:>2} fresh x → {len(accepted)} share(s)  {DIM(ids)}")
            for note in seals:
                self.log.info(MAGENTA(note), 2)
        old = self.theta_s
        self.theta_s = min(HASH_SPACE, self.theta_s * 2 * TARGET_SHARES_PER_BLOCK // (found + TARGET_SHARES_PER_BLOCK))
        self.tips.append(H512("CHAIN/TIP/v2", self.tips[-1], h))           # MOCK block production
        self.log.info(DIM(f"{found} shares at h={h}; retarget d_S {HASH_SPACE / old:.3f} → "
                          f"{HASH_SPACE / self.theta_s:.3f} (target {TARGET_SHARES_PER_BLOCK}/block)"))

    def submit_fresh_work(self, sub: FreshWorkSubmission, view: ChainView, seals: list[str]) -> Share | None:
        """VerifyFW, then ShareValid ⟺ ϑ_S < Θ_S and SealValid ⟺ ϑ_B < Θ_B (5.6)."""
        x = sub.x
        owner = self.payout_owner.get(x.payoutPolicyDigest)
        context_ok = (owner is not None and x.chainID == self.chain_id and x.genesisID == self.genesis_id
                      and x.parentCanonicalID == view.parent_id and x.height == view.height
                      and x.parentDerivedDifficultyContext == self.theta_s)
        if not context_ok or not FreshWork.verify(sub):
            self.log.reject("FRESH_WORK", f"{owner}: stale context or VerifyFW = 0")
            return None
        work_id, block_id = FreshWork.identifiers(x, sub.output_root)
        share_value, seal_value = FreshWork.lotteries(work_id)
        theta_b = seal_target(self.theta_s)
        if seal_value < theta_b:
            d_b = HASH_SPACE // theta_b
            self.chain_weight += d_b
            self.stats[owner]["seals"] += 1
            seals.append(f"★ SEAL: ϑ_B < Θ_B, BlockID {hx(block_id)}, d_B = 2^512/Θ_B ≈ {d_b} "
                         f"→ chain weight W(C) = {self.chain_weight}   (5.10)")
        if share_value >= self.theta_s:
            return None
        self.state.claim_id("ShareID", work_id)
        share = Share(work_id, owner, view.height, self.theta_s, WEIGHT_NUMERATOR // self.theta_s)
        self.shares_by_epoch[epoch_of(view.height)].append(share)
        if not self._traced_share:
            self._trace_fresh_work(sub, share, share_value, seal_value)
        return share

    def _trace_fresh_work(self, sub: FreshWorkSubmission, share: Share, share_value: int, seal_value: int) -> None:
        self._traced_share = True
        x, info = sub.x, lambda text: self.log.info(DIM("│ ") + text, 2)
        self.log.info(DIM("┌ Fresh Work trace — first accepted share"), 2)
        info("x = Enc(FW/CONTEXT/v2, 16 fields)   (5.1)")
        for f in dc_fields(x):
            value = getattr(x, f.name)
            shown = (hx(value) + "…" if isinstance(value, bytes) else
                     big(value) if f.name == "parentDerivedDifficultyContext" else str(value))
            info(f"   {f.name:<31}{shown}")
        seed = FreshWork.seed(x)
        a, b = FreshWork.instance(seed)
        info(f"seed = SHA3-512(Enc(FW/INPUT/v2, x)) = {hx(seed)}…   (5.2)")
        info(f"X = Map(SHAKE256(FW/A, seed)): {TILE_N}×{TILE_N} INT8, row 0 = {list(a[0])}   (5.3)")
        info(f"Y = Map(SHAKE256(FW/B, seed)): row 0 = {list(b[0])}")
        info(f"Z = XY exact, |Z_ij| ≤ {TILE_N}·127² = {TILE_N * 127 ** 2:,}; Z[0] = {list(sub.output[0])}")
        info(f"outputRoot = {hx(sub.output_root)}…, π_FW (MOCK STARK) ✓, Freivalds {FREIVALDS_ROUNDS}/{FREIVALDS_ROUNDS} "
             f"rounds over F_p, p = 2^61-1 ✓")
        info(f"ShareID = WorkID = SHA3-512(Enc(FW/ID/v2, x, outputRoot)) = {MAGENTA(hx(share.share_id))}…   (5.4)")
        info(f"ϑ_S = {big(share_value)} < Θ_S = {big(share.theta_s)}  ⇒ ShareValid   (5.6)")
        info(f"ϑ_B = {big(seal_value)} {'<' if seal_value < seal_target(share.theta_s) else '≥'} "
             f"Θ_B = {big(seal_target(share.theta_s))}")
        info(f"w_s = ⌊2^640/Θ_S⌋ = {units(share.weight)}·2^128, owner(s) bound by payoutPolicyDigest = {share.owner}  (6.1)")
        self.log.info(DIM("└"), 2)

    def executor_round(self, h: int) -> None:
        for job in self.jobs_in(Phase.RUNNING):
            lease = job.lease
            if lease.h_ready >= h or lease.executor_acted:
                continue
            lease.executor_acted = True
            published = self.da_store[job.data_root]                       # P-DAL reconstruction (MOCK)
            output = self.miners[lease.executor].execute(*published, epoch_of(h))
            if output is None:
                self.log.warn(f"{lease.executor} is offline: no result for {job.tag} (h_deadline = {lease.h_deadline})")
                continue
            try:
                job.settle(self.state, lease.executor, lease.lease_version, output, published, h)
            except TransitionRejected as err:
                self.log.reject("RESULT_SETTLE", f"{job.tag} from {lease.executor}: {err} — Σ unchanged (7.14)")
                continue
            self._release_lease(lease)
            self.stats[lease.executor]["settled"] += 1
            self.stats[lease.executor]["exec income"] += V_EXEC

    def data_round(self, h: int) -> None:
        for job in self.jobs_in(Phase.ADMITTED):
            if job.lease.h_admit < h and self.clients[job.client].publishes_data:
                self.da_store[job.data_root] = (job.x, job.y)
                job.input_ready(self.state, h)

    def deadline_round(self, h: int) -> None:
        for job in self.jobs_in(Phase.FUNDED, Phase.ADMITTED, Phase.RUNNING, Phase.RETRY):
            if job.phase is Phase.ADMITTED and h > job.lease.h_admit + D_AVAIL:
                executor = job.lease.executor
                bond_before = self.state.bond[executor]
                self._release_lease(job.expire(self.state, h))
                if self.state.bond[executor] != bond_before:
                    raise InvariantViolation("(6.15) a missed data window never penalizes the executor")
                self.log.info(DIM(f"penalty_executor = 0 (6.15); client forfeits f_queue + F_base (7.22)"), 3)
            elif job.phase is Phase.RUNNING and h > job.lease.h_deadline:
                lease = job.expire(self.state, h)
                self._release_lease(lease)
                self.slash(lease.executor, h)
            if job.phase is Phase.RETRY and job.retry_budget == 0:
                job.refund(self.state, "β_retry = 0")
            elif job.phase is Phase.FUNDED and h > job.h_fund + H_TTL:
                job.refund(self.state, "queue TTL expired")

    def slash(self, executor: str, h: int) -> None:
        amount = min(V_SLASH, self.state.bond[executor])
        remaining = self.state.bond[executor] - amount
        self.state.bond_slash(executor, amount, epoch_of(h),
                              f"{RED(executor)} -{fmt(amount)} TKN bond → R_l[{epoch_of(h) + LOCK_EPOCHS}]; "
                              f"B_i = {fmt(remaining)} ⇒ K = {remaining // B_LEASE}")
        self.stats[executor]["slashed"] += amount

    def scheduler_round(self, h: int) -> None:
        ready = sorted((j for j in self.jobs.values()
                        if j.phase is Phase.FUNDED or (j.phase is Phase.RETRY and j.retry_budget > 0)),
                       key=lambda j: (j.ready_since, j.job_id))
        if not ready:
            return
        self.log.info(BOLD(f"⚖  D'Hondt batch over S_{self.scheduler.cohort.epoch}: "
                           f"{len(ready)} ready tile(s) in canonical order"))
        capacity = {owner: self.capacity_free(owner) for owner in self.miners}
        winners = self.scheduler.allocate(len(ready), capacity)
        for share, job in zip(winners, ready):
            if job.tile_id in self.active_leases:
                raise InvariantViolation("(6.13) |active(tileID)| ≤ 1")
            lease = Lease(job.job_id, job.tile_id, job.lease_version + 1, share.owner, job.escrow_id,
                          V_EXEC, B_LEASE, share.share_id, h)
            self.bond_reserved[share.owner] += B_LEASE
            try:
                job.admit(self.state, lease, h)
            except TransitionRejected as err:
                self.bond_reserved[share.owner] -= B_LEASE
                self.log.reject("JOB_ADMIT_AND_LEASE", f"{job.tag}: {err}")
                continue
            self.active_leases[job.tile_id] = lease
            self.stats[share.owner]["leases"] += 1
        if len(ready) > len(winners):
            self.log.info(DIM(f"{len(ready) - len(winners)} tile(s) remain queued"), 2)

    # ── report ──
    def final_report(self) -> None:
        self.log.banner("FINAL REPORT")
        self.log.section("miners / executors")
        self.log.info(f"{'name':<9}{'U balance':>20}{'bond':>8}{'attempts':>10}{'shares':>8}{'seals':>7}"
                      f"{'FW rewards':>20}{'leases':>8}{'settled':>9}{'slashed':>9}")
        for name in self.miners:
            s = self.stats[name]
            self.log.info(f"{name:<9}{fmt(self.state.balance_of(name)):>20}{fmt(self.state.bond.get(name, 0)):>8}"
                          f"{s['attempts']:>10}{s['shares']:>8}{s['seals']:>7}{fmt(s['rewards']):>20}"
                          f"{s['leases']:>8}{s['settled']:>9}{fmt(s['slashed']):>9}")
        self.log.section("clients")
        for name in self.clients:
            jobs = [j for j in self.jobs.values() if j.client == name]
            phases = ", ".join(f"{p.value} {n}" for p in Phase if (n := sum(j.phase is p for j in jobs)))
            self.log.info(f"{name:<9} U = {fmt(self.state.balance_of(name)):>12}   jobs: {phases or '—'}   "
                          f"fees paid (locked, recycled) = {fmt(sum(j.fees_paid for j in jobs))}")
        self.log.info(f"{DA_COMMITTEE:<9} U = {fmt(self.state.balance_of(DA_COMMITTEE)):>12}")
        self.log.section("reserve dynamics (7.24)-(7.26)")
        for e, r_a, budget in self.reserve_trace:
            self.log.info(f"epoch {e:>2}   R_a = {fmt(r_a):>20}   E_e = η·R_a = {fmt(budget):>16}")
        locked_total = sum(self.state.locked_inflow.values())
        f_bar = Fraction(locked_total, EPOCHS)
        r_star = f_bar / ETA
        self.log.info(f"fees recycled into R_l: {fmt(locked_total)} TKN over {EPOCHS} epochs "
                      f"→ F̄ ≈ {fmt(int(f_bar))} TKN/epoch, steady state R* = F̄/η ≈ {fmt(int(r_star))} TKN")
        self.log.info(DIM("R_e - R* = (1-η)^e (R_0 - R*): emission decays geometrically and is perpetually "
                          "refilled by recycled fees"))
        self.log.section("consensus")
        self.log.info(f"seals = {sum(s['seals'] for s in self.stats.values())}, W(C) = {self.chain_weight}; "
                      f"shares, commercial results, fees and bonds contributed ω = 0   (5.11)")
        self.log.section("conformance invariants asserted at every commit (8.2)")
        for line in ("3   ShareID, admissionID, receiptID, rewardID used at most once (7.23)",
                     "4   allocation by (6.4) over ShareIDs, exactly N slots per batch (6.7), owner capacity enforced",
                     "5   |active(tileID)| ≤ 1 and Σ B_res ≤ B_i (6.13)",
                     "6   a missed data window never penalizes the executor (6.15)",
                     "9   every transition has ΣΔ = 0 and commits atomically or not at all (7.13)-(7.14)",
                     "10  U + S_z + E_esc + B_bond + V_L2 + R_a + R_l + P_pending = M after every operation (7.19)",
                     "11  the Base Fee reaches R_l only at admission and never returns to the escrow (7.21)"):
            self.log.info(f"{GREEN('✓')} {line}")


# ─── Simulation ──────────────────────────────────────────────────────────────────────────────────
def build_workload() -> dict[int, list[tuple]]:
    """Client actions per height: ("fund", client, β_retry) or ("cancel", client)."""
    workload: dict[int, list[tuple]] = defaultdict(list)
    for e in range(EPOCHS - 1):
        h0 = e * BLOCKS_PER_EPOCH
        workload[h0] += [("fund", "lab-A", 1), ("fund", "lab-A", 0)]
        workload[h0 + 1].append(("fund", "lab-B", e % 2))
    workload[2].append(("cancel", "lab-B"))                  # cancelled while Funded: loses f_queue only
    workload[6].append(("fund", "spammer", 0))               # never publishes data: loses f_queue + F_base
    workload[9] += [("fund", "lab-A", 1)] * 4                # burst: service budget and capacity bind
    workload[18] += [("fund", "lab-B", 1)] * 4
    return workload


def main() -> None:
    log = Log()
    miners = [
        Miner("alice", [(0, 10), (7, 5)], bond_leases=3),
        Miner("bob", [(0, 7)], bond_leases=2, offline_epochs=frozenset({3})),
        Miner("carol", [(0, 4)], bond_leases=1),
        Miner("mallory", [(0, 5)], bond_leases=1, corrupt=True),
        Miner("dave", [(4, 8)], bond_leases=2),
    ]
    clients = [Client("lab-A"), Client("lab-B"), Client("spammer", publishes_data=False)]
    node = PoUWNode(log, miners, clients)
    node.genesis({"lab-A": 4_000 * TKN, "lab-B": 2_500 * TKN, "spammer": 200 * TKN,
                  **{m.name: 100 * TKN for m in miners}})

    workload = build_workload()
    for epoch in range(EPOCHS):
        node.open_epoch(epoch)
        for h in range(epoch * BLOCKS_PER_EPOCH, (epoch + 1) * BLOCKS_PER_EPOCH):
            node.run_block(h, workload.get(h, []))
        node.close_epoch(epoch)

        total_supply = Fraction(node.state.total_supply_atoms(), ATOMS_PER_TKN)
        assert total_supply == 15_000_000, f"epoch {epoch}: supply drifted to {total_supply} TKN"
        log.info(GREEN(BOLD(f"assert total_supply == 15_000_000  ✓   "
                            f"({node.state.total_supply_atoms():,} atoms, not one created or destroyed)")))

    node.final_report()


if __name__ == "__main__":
    main()
