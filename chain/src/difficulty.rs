//! Consensus difficulty retargeting and seal/share weights (§6.4, §6.5).

use wire::{BlockHeader, DifficultyMode, Digest, GenesisConfig, MIN_TARGET, Target};

/// Read-only view of one branch's ancestor metadata, used by Stage B and retargeting.
pub trait ChainView {
    /// Returns the block identifier at height `h` (`genesis_id` at `h == 0`).
    fn block_id_at(&self, h: u64) -> Digest;
    /// Returns the `time_bucket` at height `h` (`genesis_time_bucket` at `h == 0`).
    fn time_bucket_at(&self, h: u64) -> u64;
    /// Returns `(seal_target, share_target)` at height `h`.
    fn targets_at(&self, h: u64) -> (Target, Target);
}

/// Computes `Θ_S = saturating_mul(Θ_B, ratio)` (§6.4).
pub fn share_target_for(seal: Target, ratio: u64) -> Target {
    Target(seal.0.saturating_mul(u128::from(ratio)))
}

/// Computes `(seal_target, share_target)` for the block at height `h >= 1` from its
/// ancestors only (§6.4).
pub fn expected_targets(h: u64, chain: &impl ChainView, g: &GenesisConfig) -> (Target, Target) {
    let prev_h = h.saturating_sub(1);
    let prev = if prev_h == 0 {
        g.initial_seal_target
    } else {
        chain.targets_at(prev_h).0
    };
    let seal = match g.difficulty {
        DifficultyMode::Windowed { window: n }
            if n > 0 && prev_h.is_multiple_of(n) && prev_h >= 2 * n =>
        {
            let actual = chain
                .time_bucket_at(prev_h)
                .saturating_sub(chain.time_bucket_at(prev_h - n));
            let expected = n.saturating_mul(g.target_block_secs / g.time_bucket_secs);
            Target(scale_target(prev.0, actual, expected))
        }
        _ => prev,
    };
    (seal, share_target_for(seal, g.share_ratio))
}

/// `floor(T * actual / expected)`, with `actual` clamped to `[ceil(expected/4), 4*expected]`,
/// exact in `u128`, saturating at `u128::MAX`, floored at `MIN_TARGET` (§6.4).
pub fn scale_target(t: u128, actual: u64, expected: u64) -> u128 {
    let e = expected.max(1);
    let a = u128::from(actual.clamp(e.div_ceil(4), e.saturating_mul(4)));
    let e = u128::from(e);
    (t / e)
        .checked_mul(a)
        .and_then(|x| x.checked_add((t % e) * a / e))
        .unwrap_or(u128::MAX)
        .max(MIN_TARGET)
}

/// Work of a target in units of `2^-32` attempts: `floor((2^128 - 1) / floor(T / 2^32))` (§6.5).
///
/// Targets are at least `MIN_TARGET = 2^64`, so the divisor is never zero for valid
/// headers; the `max(1)` only guards malformed input.
pub fn work_of(t: Target) -> u128 {
    u128::MAX / (t.0 >> 32).max(1)
}

/// Seal weight `d_B` of a block header, WP (5.10) (§6.5).
pub fn seal_weight(h: &BlockHeader) -> u128 {
    work_of(h.seal_target)
}

/// Share weight `w_s` of a share target, WP (6.1) (§6.5).
pub fn share_weight(t: Target) -> u64 {
    u64::try_from(work_of(t)).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appendix_b_difficulty_and_weight_vectors() {
        assert_eq!(scale_target(1u128 << 100, 320, 320), 1u128 << 100);
        assert_eq!(scale_target(1u128 << 100, 640, 320), 1u128 << 101);
        assert_eq!(scale_target(1u128 << 100, 10_000, 320), 1u128 << 102);
        assert_eq!(scale_target(1u128 << 100, 1, 320), 1u128 << 98);
        assert_eq!(scale_target(u128::MAX, 640, 320), u128::MAX);
        assert_eq!(scale_target(1000, 320, 320), 1u128 << 64);

        let default_seal = Target(u128::MAX / 32);
        assert_eq!(work_of(default_seal), 137_438_953_472);
        assert_eq!(work_of(default_seal), 32 * (1u128 << 32));
        assert_eq!(work_of(Target(u128::MAX / 64)) >> 32, 64);
        assert_eq!(
            share_weight(share_target_for(default_seal, 4)),
            34_359_738_368
        );
        assert_eq!(share_weight(share_target_for(default_seal, 4)), 1u64 << 35);
    }

    struct Flat {
        tb: Vec<u64>,
        seal: Target,
    }

    impl ChainView for Flat {
        fn block_id_at(&self, _h: u64) -> Digest {
            Digest::ZERO
        }
        fn time_bucket_at(&self, h: u64) -> u64 {
            self.tb[h as usize]
        }
        fn targets_at(&self, _h: u64) -> (Target, Target) {
            (self.seal, share_target_for(self.seal, 4))
        }
    }

    #[test]
    fn windowed_retarget_fires_only_at_window_boundaries() {
        let mut g = GenesisConfig::default_testnet(0);
        g.difficulty = DifficultyMode::Windowed { window: 4 };
        g.target_block_secs = 10;
        let seal = Target(1u128 << 100);
        // Blocks arrive twice as slowly as targeted: 20 s apart.
        let chain = Flat {
            tb: (0..20).map(|h| h * 20).collect(),
            seal,
        };
        for h in 1..=8 {
            assert_eq!(
                expected_targets(h, &chain, &g).0,
                if h == 1 { g.initial_seal_target } else { seal }
            );
        }
        // h - 1 = 8 = 2n: the first retarget doubles the target.
        assert_eq!(expected_targets(9, &chain, &g).0, Target(1u128 << 101));
        assert_eq!(expected_targets(10, &chain, &g).0, seal);
        assert_eq!(expected_targets(13, &chain, &g).0, Target(1u128 << 101));
    }
}
