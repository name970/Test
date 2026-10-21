//! Plauthium proof of concept: one mock block transition across all five modules.
//!
//! a) Genesis ledger: Emission Reserve and Founder Allocation.
//! b) Fresh Work: expand a seed into two 512 x 512 INT8 tiles, compute the
//!    exact INT32 product and certify it with six Freivalds rounds.
//! c) Scheduler: sequential D'Hondt lease allocation over discrete ShareIDs.
//! d) Ledger: lock a Base Fee, mature it into an independent fee lot and pay
//!    Fresh Work rewards from it with segregated rounding dust.
//! e) ASCII summary.
//!
//! Hashing is SHA3-512/SHAKE256 (see the `primitives` crate); signatures are not implemented.

use std::error::Error;

use ledger::{
    BLOCKS_PER_DAY, FOUNDER_ALLOCATION, JobId, Ledger, LedgerParams, StateCategory,
    VESTING_TRANCHES,
};
use primitives::{Atoms, DomainSeparatedHash, Mersenne61, TOTAL_SUPPLY};
use scheduler::{Allocation, OwnerId, ShareID, dhondt_allocation};
use scivm::{CONSENSUS_WEIGHT, OzakiIIProfile, RoundingMode};
use work::{
    ACCUMULATION_BOUND, CLASSICAL_MACS, DEFAULT_FREIVALDS_ROUNDS, FREIVALDS_FIELD_MULS,
    FreshWorkContext, FreshWorkIds, TILE_DIM, verify_freivalds_rounds,
};

/// Height of the block this PoC seals.
const BLOCK_HEIGHT: u64 = 1;
/// Execution slots (ready tiles) offered to the scheduler in this batch.
const LEASE_SLOTS: usize = 8;

type PocResult<T> = Result<T, Box<dyn Error>>;

fn main() -> PocResult<()> {
    println!("Plauthium PoC - mock block transition");
    println!("(hash/XOF are SHA3-512/SHAKE256; signatures are not implemented)\n");

    // ---------------------------------------------------------------- (a)
    section("a", "Genesis ledger");
    let mut ledger = Ledger::genesis(LedgerParams::poc())?;
    print_categories(&ledger);
    let vesting = *ledger.founder_vesting();
    let first = vesting.tranche(1).ok_or("missing tranche 1")?;
    let last = vesting
        .tranche(VESTING_TRANCHES)
        .ok_or("missing last tranche")?;
    println!(
        "  FounderAllocation: {} tranches inside U; tranche 1 = {} atoms @ h={}, tranche {} @ h={}",
        VESTING_TRANCHES,
        group(first.value.get()),
        first.unlock_height,
        last.day,
        group(last.unlock_height),
    );
    println!(
        "  EmissionReserve:   Genesis lot c_G with A = {} atoms\n",
        group(ledger.fee_lots()[0].principal().get())
    );

    // ---------------------------------------------------------------- (b)
    section(
        "b",
        "Fresh Work: seed -> 512x512 INT8 tiles -> INT32 product",
    );
    let context = mock_context(BLOCK_HEIGHT, 0);
    let instance = context.instance();
    println!(
        "  seed = H512(FW/INPUT/v2, x)       {}..",
        instance.seed.short_hex(16)
    );
    println!(
        "  X, Y = Map(XOF(FW/A|FW/B, seed))  {TILE_DIM}x{TILE_DIM}, entries in [-127, 127], max|X| = {}",
        instance.x.max_abs()
    );
    let product = instance.x.product(&instance.y);
    println!(
        "  Z = XY ({} MACs)         max|Z| = {} <= {}",
        group(CLASSICAL_MACS),
        group(product.max_abs() as u64),
        group(ACCUMULATION_BOUND as u64)
    );
    let accepted =
        verify_freivalds_rounds(&instance.x, &instance.y, &product, DEFAULT_FREIVALDS_ROUNDS);
    let mut forged = product.clone();
    let entry = forged.get(17, 42);
    forged.set(17, 42, if entry > 0 { entry - 1 } else { entry + 1 })?;
    let forged_rejected =
        !verify_freivalds_rounds(&instance.x, &instance.y, &forged, DEFAULT_FREIVALDS_ROUNDS);
    ensure(accepted, "Freivalds rejected the honest product")?;
    ensure(forged_rejected, "Freivalds accepted a forged product")?;
    println!(
        "  Freivalds r_F = {DEFAULT_FREIVALDS_ROUNDS} over F_p (p = {}): honest ACCEPT, forged REJECT",
        Mersenne61::P
    );
    let ids = FreshWorkIds::derive(&context, &product);
    println!("  outputRoot {}..", ids.output_root.short_hex(16));
    println!("  WorkID     {}..  (= ShareID)", ids.work_id.short_hex(16));
    println!("  BlockID    {}..", ids.block_id.short_hex(16));
    println!(
        "  theta_S    {}..  theta_B {}..\n",
        ids.share_ticket.short_hex(8),
        ids.seal_ticket.short_hex(8)
    );

    // ---------------------------------------------------------------- (c)
    section("c", "Scheduler: sequential D'Hondt over discrete ShareIDs");
    let shares = mock_cohort(ids.work_id);
    let allocation = dhondt_allocation(&shares, LEASE_SLOTS)?;
    ensure(
        allocation.assigned() == LEASE_SLOTS,
        "D'Hondt left slots unassigned",
    )?;
    print_allocation(&shares, &allocation);

    // ---------------------------------------------------------------- (d)
    section("d", "Ledger: Base Fee lock -> maturation -> fee-lot payout");
    let weights: Vec<u64> = shares.iter().map(|s| s.weight).collect();

    // Epoch 0 rewards from the Genesis lot give miners spendable outputs in U.
    let (_, genesis_epoch) = ledger.book_epoch(&weights)?;
    ledger.fw_reward(genesis_epoch.booked)?;
    step(
        &ledger,
        "FW_REWARD (epoch 0, Genesis lot)",
        format!(
            "{} paid, {} atoms dust",
            genesis_epoch.booked,
            genesis_epoch.dust.get()
        ),
    )?;

    // The commercial job runs on the SciVM under a bound Ozaki-II profile.
    let profile = OzakiIIProfile {
        version: 1,
        moduli: vec![255, 254, 253, 251, 247, 241, 239, 233],
        inner_dim: TILE_DIM as u64,
        bound_x: 1 << 23,
        bound_y: 1 << 23,
        rounding: RoundingMode::NearestTiesToEven,
    };
    profile.validate()?;
    let (xs, ys) = mock_scientific_operands();
    let dot = profile.exact_dot(&xs, &ys)?;
    let direct: i128 = xs
        .iter()
        .zip(&ys)
        .map(|(&a, &b)| a as i128 * b as i128)
        .sum();
    ensure(dot == direct, "Ozaki-II reconstruction mismatch")?;
    println!(
        "  SciVM Ozaki-II: {} moduli, Q ~ 2^{:.1} > 2*B_rec; exact dot = {} (consensus weight {CONSENSUS_WEIGHT})",
        profile.moduli.len(),
        (profile.reconstruction_modulus()? as f64).log2(),
        dot
    );

    // Worked budget of eq. (7.17): D = F_base + v_exec + v_DA + v_ref = 100 PLAU.
    let job = JobId(1);
    let (deposit, queue_fee, base_fee) = (plau(100)?, plau(1)?, plau(10)?);
    ledger.escrow_fund_and_queue(job, deposit, queue_fee)?;
    step(
        &ledger,
        "ESCROW_FUND_AND_QUEUE",
        format!("D = {deposit}, f_queue = {queue_fee}"),
    )?;
    ledger.job_admit_and_lease(job, base_fee)?;
    step(
        &ledger,
        "JOB_ADMIT_AND_LEASE",
        format!("Base Fee {base_fee} -> R_l (irretrievable)"),
    )?;
    ledger.result_settle(job, plau(80)?, plau(5)?, plau(5)?)?;
    step(
        &ledger,
        "RESULT_SETTLE",
        "v_exec 80 + v_DA 5 + v_ref 5 PLAU -> U".to_string(),
    )?;

    let maturity_epoch = ledger
        .locked_fees()
        .iter()
        .map(|f| f.maturity_epoch)
        .max()
        .ok_or("no locked fees")?;
    let maturity_height =
        ledger.params().genesis_height + maturity_epoch * ledger.params().blocks_per_epoch;
    ledger.advance_to(maturity_height)?;
    let (_, matured) = ledger.reserve_mature()?;
    step(
        &ledger,
        "RESERVE_MATURE",
        format!(
            "h={maturity_height} (epoch {maturity_epoch}): {} lots R_l -> R_a",
            matured.len()
        ),
    )?;

    let (_, epoch) = ledger.book_epoch(&weights)?;
    ledger.fw_reward(epoch.booked)?;
    step(
        &ledger,
        "FW_REWARD (epoch with fee lots)",
        format!("{} paid from {} lots", epoch.booked, epoch.lots.len()),
    )?;
    for lot in epoch.lots.iter().filter(|l| matured.contains(&l.lot)) {
        let source = ledger
            .fee_lots()
            .iter()
            .find(|l| l.id() == lot.lot)
            .map_or(Atoms::ZERO, |l| l.source_fee());
        println!(
            "    fee lot #{} (F_c = {source}): budget {} atoms = {} booked + {} dust (segregated)",
            lot.lot.0,
            group(lot.budget.get()),
            group(lot.booked.get()),
            lot.dust.get()
        );
    }
    ledger.check_invariants()?;
    println!();
    print_categories(&ledger);

    // ---------------------------------------------------------------- (e)
    print_summary(&ledger, &ids, &allocation, matured.len(), epoch.dust)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Mock inputs
// ---------------------------------------------------------------------------

fn tag(name: &str) -> DomainSeparatedHash {
    DomainSeparatedHash::new("POC/MOCK/v2", &[name.as_bytes()])
}

fn mock_context(height: u64, attempt_nonce: u64) -> FreshWorkContext {
    FreshWorkContext {
        chain_id: 0x504c_4155, // "PLAU"
        genesis_id: tag("genesis"),
        protocol_version: 2,
        crypto_suite_id: 1,
        program_id: tag("fresh-work-program"),
        air_id: tag("int8-tile-air"),
        generator_version: 2,
        parent_canonical_id: tag("block-0"),
        stable_anchor_id: tag("anchor-0"),
        height,
        miner_policy_digest: tag("miner-policy"),
        payout_policy_digest: tag("payout-policy"),
        attempt_nonce,
        body_core_root: tag("body-core-1"),
        parent_derived_difficulty_context: tag("difficulty-0"),
        time_bucket: height * ledger::BLOCK_INTERVAL_SECONDS,
    }
}

/// The fresh share plus five mature shares from other miners. Owner 2 holds
/// two ShareIDs: its slots are simply the sum over them.
fn mock_cohort(fresh: DomainSeparatedHash) -> Vec<ShareID> {
    let mut shares = vec![ShareID {
        id: fresh,
        weight: 1_000_003,
        owner: OwnerId(1),
    }];
    for (i, (weight, owner)) in [
        (2_400_017u64, 2u64),
        (700_001, 2),
        (1_600_009, 3),
        (300_007, 4),
        (999_983, 5),
    ]
    .into_iter()
    .enumerate()
    {
        shares.push(ShareID {
            id: tag(&format!("mature-share-{i}")),
            weight,
            owner: OwnerId(owner),
        });
    }
    shares
}

/// Deterministic scaled integer operands with |x|, |y| < 2^23.
fn mock_scientific_operands() -> (Vec<f64>, Vec<f64>) {
    let operands = |mul: u64| -> Vec<f64> {
        (0..TILE_DIM as u64)
            .map(|i| ((i.wrapping_mul(mul) >> 7) % (1 << 24)) as f64 - (1 << 23) as f64)
            .collect()
    };
    (operands(2_654_435_761), operands(40_503))
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

fn plau(n: u64) -> PocResult<Atoms> {
    Atoms::from_plau(n).ok_or_else(|| "PLAU amount overflows".into())
}

fn ensure(condition: bool, message: &str) -> PocResult<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn section(letter: &str, title: &str) {
    println!("({letter}) {title}");
}

/// Reports one operation and re-checks the supply after it.
fn step(ledger: &Ledger, op: &str, detail: String) -> PocResult<()> {
    ensure(
        ledger.total() == u128::from(TOTAL_SUPPLY.get()),
        "supply changed",
    )?;
    println!("  {op:<32} {detail}");
    Ok(())
}

fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn print_categories(ledger: &Ledger) {
    let rule = "  +-----------------+-----------+---------------------------+";
    println!("{rule}");
    println!(
        "  | {:<15} | {:<9} | {:>25} |",
        "Category", "Symbol", "Balance (atoms)"
    );
    println!("{rule}");
    for category in StateCategory::ALL {
        println!(
            "  | {:<15} | {:<9} | {:>25} |",
            format!("{category:?}"),
            category.symbol(),
            group(ledger.balance(category).get())
        );
    }
    println!("{rule}");
    let total = ledger.total() as u64;
    let verdict = if u128::from(total) == u128::from(TOTAL_SUPPLY.get()) {
        "== M"
    } else {
        "!= M"
    };
    println!(
        "  | {:<15} | {:<9} | {:>25} |",
        "SUM",
        verdict,
        group(total)
    );
    println!("{rule}\n");
}

fn print_allocation(shares: &[ShareID], allocation: &Allocation) {
    let rule = "  +----+--------------------+-------+-----------+-------+";
    println!("{rule}");
    println!("  | #  | ShareID            | Owner | Weight    | Slots |");
    println!("{rule}");
    for (i, (share, slots)) in shares.iter().zip(&allocation.per_share).enumerate() {
        println!(
            "  | {i:<2} | {}.. | {:<5} | {:>9} | {slots:>5} |",
            share.id.short_hex(8),
            share.owner.0,
            group(share.weight)
        );
    }
    println!("{rule}");
    let order: Vec<String> = allocation.slots.iter().map(|s| s.to_string()).collect();
    println!("  lease order (slot -> share #): {}\n", order.join(" "));
}

fn print_summary(
    ledger: &Ledger,
    ids: &FreshWorkIds,
    allocation: &Allocation,
    matured_lots: usize,
    epoch_dust: Atoms,
) -> PocResult<()> {
    const WIDTH: usize = 72;
    let border = format!("+{}+", "=".repeat(WIDTH));
    let rule = format!("+{}+", "-".repeat(WIDTH));
    let line = |key: &str, value: String| println!("| {key:<20}: {value:<w$}|", w = WIDTH - 23);
    let distinct = allocation.per_share.iter().filter(|&&a| a > 0).count();
    let vested = ledger
        .founder_vesting()
        .completed_intervals(ledger.height());

    println!("{border}");
    println!("|{:^WIDTH$}|", "PLAUTHIUM BLOCK EXECUTED SUCCESSFULLY");
    println!("{border}");
    line(
        "Block height",
        format!("{BLOCK_HEIGHT} (state advanced to h={})", ledger.height()),
    );
    line("BlockID", format!("{}..", ids.block_id.short_hex(16)));
    line(
        "WorkID = ShareID",
        format!("{}..", ids.work_id.short_hex(16)),
    );
    line(
        "Work unit",
        format!(
            "{TILE_DIM}x{TILE_DIM} INT8 -> INT32, |Z| <= {}",
            group(ACCUMULATION_BOUND as u64)
        ),
    );
    line(
        "Freivalds",
        format!(
            "ACCEPT, r_F = {DEFAULT_FREIVALDS_ROUNDS}, {} F_p mults (9/256)",
            group(FREIVALDS_FIELD_MULS)
        ),
    );
    line(
        "D'Hondt leases",
        format!(
            "{}/{LEASE_SLOTS} slots -> {distinct} discrete ShareIDs",
            allocation.assigned()
        ),
    );
    line(
        "Fee lots matured",
        format!("{matured_lots} (queue fee + Base Fee), R_l -> R_a"),
    );
    line(
        "Dust this epoch",
        format!(
            "{} atoms (total segregated {})",
            epoch_dust.get(),
            ledger.segregated_dust().get()
        ),
    );
    line(
        "Founder vesting",
        format!(
            "{vested}/{VESTING_TRANCHES} tranches unlocked ({} days x {BLOCKS_PER_DAY} blocks)",
            vested
        ),
    );
    line(
        "Founder locked",
        format!(
            "{} of {} atoms",
            group(ledger.founder_vesting().locked_at(ledger.height()).get()),
            group(FOUNDER_ALLOCATION.get())
        ),
    );
    println!("{rule}");
    for category in StateCategory::ALL {
        line(
            category.symbol(),
            format!(
                "{:<16}{:>25}",
                format!("{category:?}"),
                group(ledger.balance(category).get())
            ),
        );
    }
    println!("{rule}");
    let total = ledger.total();
    ensure(
        total == u128::from(TOTAL_SUPPLY.get()),
        "supply invariant violated",
    )?;
    line(
        "SUM (== M)",
        format!("{:<16}{:>25}  OK", "", group(total as u64)),
    );
    println!("{border}");
    Ok(())
}
