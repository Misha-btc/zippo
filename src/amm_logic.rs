//! AMM math for the single-side zap on an oyl-amm pool.
//!
//! Fee model: `fee_per_1000` (e.g. `10` → 1%). The full input is added
//! to the reserve; the fee is retained inside the K invariant
//! (Uniswap V2-style).

use anyhow::{anyhow, Result};
use ruint::Uint;

pub type U256 = Uint<256, 4>;

/// Constant-product output with `fee_per_1000`.
///
/// `amount_out = amount_in · (1000 − fee) · reserve_out /
///               (reserve_in · 1000 + amount_in · (1000 − fee))`
pub fn calculate_swap_out(
    amount_in: u128,
    reserve_in: u128,
    reserve_out: u128,
    fee_per_1000: u128,
) -> Result<u128> {
    if amount_in == 0 {
        return Err(anyhow!("amount_in must be > 0"));
    }
    if reserve_in == 0 || reserve_out == 0 {
        return Err(anyhow!("pool has no liquidity on one side"));
    }
    let a = U256::from(1000u128.saturating_sub(fee_per_1000));
    let ai = U256::from(amount_in);
    let ri = U256::from(reserve_in);
    let ro = U256::from(reserve_out);

    let ai_with_fee = ai * a;
    let numerator = ai_with_fee * ro;
    let denominator = ri * U256::from(1000u128) + ai_with_fee;
    if denominator.is_zero() {
        return Err(anyhow!("swap denominator is zero (degenerate pool state)"));
    }
    Ok((numerator / denominator).try_into()?)
}

/// Optimal swap split for the single-side zap.
///
/// The user has `A` units of input_token. The pool's reserve on that
/// side is `Rx`. Find `s` such that after swapping `s` → output the
/// remaining `(A − s)` input and the received output match the new
/// pool ratio `(Rx', Ry')`.
///
/// Quadratic: `a·s² + Rx·(1000+a)·s − 1000·A·Rx = 0`,
/// where `a = 1000 − fee_per_1000`.
///
/// `s = (√(Rx·(Rx·(1000+a)² + 4·a·1000·A)) − Rx·(1000+a)) / (2·a)`
pub fn calculate_single_side_swap(
    amount_in: u128,
    reserve_in: u128,
    fee_per_1000: u128,
) -> u128 {
    if amount_in == 0 || reserve_in == 0 {
        return 0;
    }
    let a = U256::from(1000u128.saturating_sub(fee_per_1000));
    let big_a = U256::from(amount_in);
    let rx = U256::from(reserve_in);
    let thousand = U256::from(1000u128);
    let k = thousand + a;

    let inner = rx * k * k + U256::from(4u128) * a * thousand * big_a;
    let disc = rx * inner;
    let sqrt_disc = integer_sqrt(disc);
    let rx_k = rx * k;
    if sqrt_disc <= rx_k {
        return 0;
    }
    let num = sqrt_disc - rx_k;
    let den = U256::from(2u128) * a;
    if den.is_zero() {
        return 0;
    }
    let s: u128 = (num / den).try_into().unwrap_or(0);
    // At least 1 unit must remain for the deposit side.
    s.min(amount_in.saturating_sub(1))
}

/// Inverse zap: given a target `lp_out`, find the exact `swap_amount`
/// and `amount_in` that make the pool mint exactly `lp_out` LP.
///
/// Closed-form (derived from the same balance conditions as the
/// forward zap):
///   `s = L · 1000 · R_in / (a' · ts)`
///   `A_in = s + L · (R_in + s) / ts`,
/// where `a' = 1000 − fee_per_1000`.
///
/// Rounding: both results are rounded UP so the pool mints ≥ `lp_out`
/// (the surplus comes back as a tiny dust refund). This is safe for the
/// caller — `amount_in_max` catches any overshoot.
pub fn calculate_amount_in_for_exact_lp(
    lp_out: u128,
    reserve_in: u128,
    total_supply: u128,
    fee_per_1000: u128,
) -> Result<(u128, u128)> {
    if lp_out == 0 {
        return Err(anyhow!("lp_out must be > 0"));
    }
    if reserve_in == 0 {
        return Err(anyhow!("pool input-side reserve is zero"));
    }
    if total_supply == 0 {
        return Err(anyhow!("pool total_supply is zero"));
    }
    let a_prime = U256::from(1000u128.saturating_sub(fee_per_1000));
    if a_prime.is_zero() {
        return Err(anyhow!("fee >= 1000"));
    }
    let l = U256::from(lp_out);
    let ri = U256::from(reserve_in);
    let ts = U256::from(total_supply);
    let thousand = U256::from(1000u128);
    let one = U256::from(1u128);

    // s = ceil(L · 1000 · R_in / (a' · ts))
    let s_num = l * thousand * ri;
    let s_den = a_prime * ts;
    let s = (s_num + s_den - one) / s_den;
    let s_u128: u128 = s.try_into().map_err(|_| anyhow!("s overflow u128"))?;

    // A_in = s + ceil(L · (R_in + s) / ts)
    let ri_plus_s = ri + s;
    let a_in_part = l * ri_plus_s;
    let a_in_part_div = (a_in_part + ts - one) / ts;
    let a_in_u256 = s + a_in_part_div;
    let amount_in: u128 = a_in_u256.try_into().map_err(|_| anyhow!("amount_in overflow u128"))?;

    Ok((amount_in, s_u128))
}

/// Inverse ZapOut: given a target `output_amount`, find the minimum
/// `L` (LP to burn) such that a single-side unzap (WithdrawAndBurn +
/// swap of the other side → output) yields ≥ `output_amount`.
///
/// Closed-form from the quadratic:
///   `1000·R_a·f² − (R_a·(2000−fee) + T·fee)·f + 1000·T = 0`,
/// where `f = L / ts` and `R_a` is the output-side reserve.
/// Smaller root: `f = (B − √(B² − 4·10⁶·R_a·T)) / (2000·R_a)`,
/// where `B = R_a·(2000−fee) + T·fee`.
///
/// L is rounded **up** so that burning this L makes the pool deliver
/// ≥ `output_amount` (overshoot is microscopic).
///
/// Errors:
/// * `output_amount = 0`, `reserve_out_side = 0`, or `total_supply = 0`
/// * `output_amount ≥ reserve_out_side` — impossible to extract more
///   than the pool's own side (asymptotically f→1 gives T → R_a)
pub fn calculate_lp_for_exact_out(
    output_amount: u128,
    reserve_out_side: u128,
    total_supply: u128,
    fee_per_1000: u128,
) -> Result<u128> {
    if output_amount == 0 {
        return Err(anyhow!("output_amount must be > 0"));
    }
    if reserve_out_side == 0 {
        return Err(anyhow!("pool output-side reserve is zero"));
    }
    if total_supply == 0 {
        return Err(anyhow!("pool total_supply is zero"));
    }
    if output_amount >= reserve_out_side {
        return Err(anyhow!(
            "output_amount {} >= reserve {} — impossible to extract",
            output_amount,
            reserve_out_side
        ));
    }
    if fee_per_1000 >= 1000 {
        return Err(anyhow!("pool fee >= 100% (misconfigured)"));
    }

    let r_a = U256::from(reserve_out_side);
    let t = U256::from(output_amount);
    let ts = U256::from(total_supply);
    let fee_u = U256::from(fee_per_1000);
    let thousand = U256::from(1000u128);
    let two_thousand = U256::from(2000u128);
    let one = U256::from(1u128);

    // B = R_a · (2000 − fee) + T · fee
    let b_pos = r_a * (two_thousand - fee_u) + t * fee_u;

    // disc = B² − 4·1000·R_a · 1000·T
    let four_ac = U256::from(4u128) * thousand * r_a * thousand * t;
    if b_pos * b_pos < four_ac {
        return Err(anyhow!("discriminant negative — output_amount unreachable"));
    }
    let disc = b_pos * b_pos - four_ac;
    let sqrt_disc = integer_sqrt(disc);

    // f_num = B − √disc (smaller root)
    if sqrt_disc > b_pos {
        return Err(anyhow!("sqrt > B — math sanity violated"));
    }
    let f_num = b_pos - sqrt_disc;

    // f_den = 2000 · R_a
    let f_den = two_thousand * r_a;
    if f_den.is_zero() {
        return Err(anyhow!("denominator is zero"));
    }

    // L = ceil(f · ts) = ceil(f_num · ts / f_den)
    let l_num = f_num * ts;
    let l_ceil = (l_num + f_den - one) / f_den;
    let l: u128 = l_ceil.try_into().map_err(|_| anyhow!("L overflow u128"))?;
    if l == 0 {
        return Err(anyhow!("L computed as 0 — output_amount too small"));
    }
    // Safety margin: pool's `WithdrawAndBurn` does `floor(L · R / ts)`
    // on both sides. Each floor() strips up to 1 wei from output. The
    // swap rounds separately too. Bumping by ~3 wei guarantees we end
    // up ≥ output_amount after every floor.
    let l_safe = l.saturating_add(3);
    Ok(l_safe)
}

/// `a · b / c` via U256 — avoids overflow of intermediate u128 values.
pub fn mul_div(a: u128, b: u128, c: u128) -> u128 {
    if c == 0 {
        return 0;
    }
    let r = U256::from(a) * U256::from(b) / U256::from(c);
    r.try_into().unwrap_or(0)
}

/// Babylonian integer sqrt over U256.
pub fn integer_sqrt(n: U256) -> U256 {
    if n.is_zero() {
        return U256::ZERO;
    }
    let mut x = n;
    let mut y = (x + U256::from(1u128)) / U256::from(2u128);
    while y < x {
        x = y;
        y = (x + n / x) / U256::from(2u128);
    }
    x
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── integer_sqrt ──

    #[test]
    fn sqrt_zero_one() {
        assert_eq!(integer_sqrt(U256::from(0u128)), U256::ZERO);
        assert_eq!(integer_sqrt(U256::from(1u128)), U256::from(1u128));
    }

    #[test]
    fn sqrt_perfect_squares() {
        assert_eq!(integer_sqrt(U256::from(4u128)), U256::from(2u128));
        assert_eq!(integer_sqrt(U256::from(100u128)), U256::from(10u128));
        assert_eq!(integer_sqrt(U256::from(10_000u128)), U256::from(100u128));
    }

    #[test]
    fn sqrt_floor_for_non_squares() {
        assert_eq!(integer_sqrt(U256::from(99u128)), U256::from(9u128));
        assert_eq!(integer_sqrt(U256::from(2u128)), U256::from(1u128));
        assert_eq!(integer_sqrt(U256::from(10u128)), U256::from(3u128));
    }

    #[test]
    fn sqrt_large_power_of_two() {
        // sqrt(2^200) == 2^100
        let n: U256 = U256::from(1u128) << 200;
        let expected: U256 = U256::from(1u128) << 100;
        assert_eq!(integer_sqrt(n), expected);
    }

    // ── mul_div ──

    #[test]
    fn mul_div_basic() {
        assert_eq!(mul_div(10, 20, 5), 40);
        assert_eq!(mul_div(7, 11, 13), 7 * 11 / 13);
    }

    #[test]
    fn mul_div_zeros() {
        assert_eq!(mul_div(0, 100, 5), 0);
        assert_eq!(mul_div(100, 0, 5), 0);
        assert_eq!(mul_div(100, 200, 0), 0); // protected division by zero
    }

    #[test]
    fn mul_div_no_u128_overflow() {
        // u128::MAX * 3 / 3 must not overflow because we go through U256.
        assert_eq!(mul_div(u128::MAX, 3, 3), u128::MAX);
        assert_eq!(mul_div(u128::MAX, 2, 4), u128::MAX / 2);
    }

    // ── calculate_swap_out ──

    #[test]
    fn swap_out_equal_reserves_1pct_fee() {
        // a=990, in=100, R_in=R_out=1000
        // in_with_fee = 99000
        // num = 99000 * 1000 = 99_000_000
        // den = 1000*1000 + 99000 = 1_099_000
        // out = 99_000_000 / 1_099_000 = 90 (floor)
        let out = calculate_swap_out(100, 1000, 1000, 10).unwrap();
        assert_eq!(out, 90);
    }

    #[test]
    fn swap_out_zero_fee() {
        // No fee: out = in * R_out / (R_in + in)
        // 100 * 1000 / 1100 = 90
        let out = calculate_swap_out(100, 1000, 1000, 0).unwrap();
        assert_eq!(out, 90);
    }

    #[test]
    fn swap_out_invariant_grows_under_fee() {
        // With fee, the invariant K = R_in*R_out grows after the swap.
        let r_in: u128 = 1_000_000;
        let r_out: u128 = 1_000_000;
        let amount_in: u128 = 10_000;
        let fee = 10;

        let out = calculate_swap_out(amount_in, r_in, r_out, fee).unwrap();
        let k_before = U256::from(r_in) * U256::from(r_out);
        let k_after = U256::from(r_in + amount_in) * U256::from(r_out - out);
        assert!(k_after > k_before, "K must grow due to fee");
    }

    #[test]
    fn swap_out_errors_on_zero() {
        assert!(calculate_swap_out(0, 1000, 1000, 10).is_err());
        assert!(calculate_swap_out(100, 0, 1000, 10).is_err());
        assert!(calculate_swap_out(100, 1000, 0, 10).is_err());
    }

    // ── calculate_single_side_swap ──

    /// Check via a **practical criterion**: simulate what execute_zap
    /// does after the swap (proportional cap), and count how many units
    /// of input are lost as dust. That dust must be a small fraction of
    /// `amount`, otherwise the formula is incorrect.
    ///
    /// `max_dust_bps` — allowed dust in bps (`100 = 1%`). For tiny pools
    /// (where `amount/reserve > 1%`) we allow more slack because of
    /// integer rounding; for normal pools we expect single-digit wei.
    fn assert_dust_below(
        amount: u128,
        r_in: u128,
        r_out: u128,
        fee: u128,
        max_dust_bps: u128,
    ) {
        let s = calculate_single_side_swap(amount, r_in, fee);
        assert!(s > 0, "s must be positive; amount={}, r_in={}", amount, r_in);
        assert!(s < amount, "s must leave depositable remainder");

        let out = calculate_swap_out(s, r_in, r_out, fee).unwrap();
        assert!(out > 0, "swap output must be positive");

        let keep = amount - s;
        let new_r_in = r_in + s;
        let new_r_out = r_out - out;

        // Same proportional cap as in execute_zap.
        let out_for_in = mul_div(keep, new_r_out, new_r_in);
        let (deposit_in, deposit_out) = if out_for_in <= out {
            (keep, out_for_in)
        } else {
            let in_for_out = mul_div(out, new_r_in, new_r_out);
            (in_for_out, out)
        };

        let dust_in = keep - deposit_in;
        let dust_out = out - deposit_out;

        // Convert dust_out into "input units" at the new pool price.
        let dust_out_as_in = mul_div(dust_out, new_r_in, new_r_out);
        let total_dust = dust_in + dust_out_as_in;

        let limit = amount * max_dust_bps / 10_000;
        assert!(
            total_dust <= limit,
            "dust > {}bps: amount={}, r_in={}, r_out={}, fee={}, s={}, out={}, dust_in={}, dust_out={} (as in: {}), limit={}",
            max_dust_bps, amount, r_in, r_out, fee, s, out, dust_in, dust_out, dust_out_as_in, limit
        );
    }

    #[test]
    fn single_side_equal_reserves_small() {
        // Small pools — rounding produces up to 1% dust. Tolerated:
        // dust is bounded by ~1 unit per ~50 wei of deposit.
        assert_dust_below(100, 1_000, 1_000, 10, 200); // ≤ 2%
    }

    #[test]
    fn single_side_equal_reserves_medium() {
        // Medium pools — dust drops to a fraction of a percent.
        assert_dust_below(1_000, 10_000, 10_000, 10, 50);     // ≤ 0.5%
        assert_dust_below(50_000, 1_000_000, 1_000_000, 10, 10); // ≤ 0.1%
    }

    #[test]
    fn single_side_equal_reserves_large() {
        // Large pools — dust is negligible.
        assert_dust_below(1_000_000_000, 1_000_000_000_000, 1_000_000_000_000, 10, 1); // ≤ 0.01%
    }

    #[test]
    fn single_side_asymmetric_reserves() {
        // input-side << output-side: lots of output for s
        assert_dust_below(1_000, 10_000, 1_000_000, 10, 100);
        // input-side >> output-side: little output for s
        assert_dust_below(1_000, 1_000_000, 10_000, 10, 100);
    }

    #[test]
    fn single_side_high_fee() {
        // 5% (50/1000) — optimal s shifts but the formula still holds
        assert_dust_below(10_000, 1_000_000, 1_000_000, 50, 10);
    }

    #[test]
    fn single_side_zero_fee() {
        // No fee: s → A/2 for symmetric reserves
        let s = calculate_single_side_swap(1_000, 1_000_000, 0);
        assert!(s > 0);
        // ~A/2 when the pool is large relative to amount
        let s_i = s as i128;
        assert!((s_i - 500).abs() < 5, "s should be ~A/2, got {}", s);
        // With fee=0, floor(s*) sheds up to ~1 swap unit, which for
        // tiny amounts yields a few wei of dust. Allow 50 bps.
        assert_dust_below(1_000, 1_000_000, 1_000_000, 0, 50);
    }

    #[test]
    fn single_side_zero_fee_large_amount() {
        // At realistic volumes rounding becomes invisible.
        assert_dust_below(1_000_000, 1_000_000_000, 1_000_000_000, 0, 1);
    }

    #[test]
    fn single_side_zero_inputs() {
        assert_eq!(calculate_single_side_swap(0, 1000, 10), 0);
        assert_eq!(calculate_single_side_swap(100, 0, 10), 0);
    }

    #[test]
    fn single_side_keeps_at_least_one_unit() {
        // s must never equal amount (no remainder to deposit)
        let s = calculate_single_side_swap(100, 10_000, 10);
        assert!(s < 100);
    }

    // ── calculate_amount_in_for_exact_lp ──

    /// Forward/inverse consistency: given amount_in, compute LP via
    /// forward zap. Then run inverse on that LP — recovered amount_in
    /// should be ≈ original (rounding differences ≤ 0.1%).
    fn assert_round_trip(amount_in: u128, r_in: u128, r_out: u128, ts: u128, fee: u128) {
        // FORWARD: solve for L given A_in
        let s = calculate_single_side_swap(amount_in, r_in, fee);
        assert!(s > 0 && s < amount_in);
        let out = calculate_swap_out(s, r_in, r_out, fee).unwrap();
        let keep = amount_in - s;
        let new_r_in = r_in + s;
        let new_r_out = r_out - out;
        let one_for_zero = mul_div(keep, new_r_out, new_r_in);
        let (dep_in, dep_out) = if one_for_zero <= out {
            (keep, one_for_zero)
        } else {
            (mul_div(out, new_r_in, new_r_out), out)
        };
        let lp_a = U256::from(dep_in) * U256::from(ts) / U256::from(new_r_in);
        let lp_b = U256::from(dep_out) * U256::from(ts) / U256::from(new_r_out);
        let lp_forward: u128 = (if lp_a < lp_b { lp_a } else { lp_b }).try_into().unwrap();
        assert!(lp_forward > 0);

        // INVERSE: given L, find required A_in. Should be ≈ original.
        let (amount_in_inv, s_inv) =
            calculate_amount_in_for_exact_lp(lp_forward, r_in, ts, fee).unwrap();

        // Required input must be ≤ original (we round UP slightly, so could be
        // a hair higher; let it within 0.1% slack).
        let diff = if amount_in_inv > amount_in {
            amount_in_inv - amount_in
        } else {
            amount_in - amount_in_inv
        };
        let tol = (amount_in / 1000).max(10);
        assert!(
            diff <= tol,
            "inverse A_in {} differs from forward {} by {} (>{:.2}%)",
            amount_in_inv, amount_in, diff, (tol as f64 / amount_in as f64) * 100.0
        );
        // Also check swap s consistency
        let s_diff = if s_inv > s { s_inv - s } else { s - s_inv };
        let s_tol = (s / 1000).max(10);
        assert!(s_diff <= s_tol, "inverse s {} differs from forward {} by {}", s_inv, s, s_diff);
    }

    #[test]
    fn inverse_round_trip_medium() {
        assert_round_trip(50_000_000, 1_000_000, 1_000_000, 1_000_000, 10);
        assert_round_trip(10_000_000, 100_000_000, 100_000_000, 100_000_000, 10);
    }

    #[test]
    fn inverse_round_trip_asymmetric() {
        assert_round_trip(10_000_000, 100_000_000, 10_000_000, 31_622_776, 10);
        assert_round_trip(10_000_000, 10_000_000, 100_000_000, 31_622_776, 10);
    }

    #[test]
    fn inverse_round_trip_large() {
        assert_round_trip(
            1_000_000_000_000,
            10_000_000_000_000,
            10_000_000_000_000,
            10_000_000_000_000,
            10,
        );
    }

    #[test]
    fn inverse_errors_on_zero() {
        assert!(calculate_amount_in_for_exact_lp(0, 1000, 1000, 10).is_err());
        assert!(calculate_amount_in_for_exact_lp(100, 0, 1000, 10).is_err());
        assert!(calculate_amount_in_for_exact_lp(100, 1000, 0, 10).is_err());
        assert!(calculate_amount_in_for_exact_lp(100, 1000, 1000, 1000).is_err()); // fee=1000 → a'=0
    }

    // ── calculate_lp_for_exact_out (ZapOut inverse) ──

    /// Forward/inverse round-trip for ZapOut: given LP `L`, compute T
    /// using ZapOut math (withdraw + swap), then ask inverse to find L'
    /// from T. Should recover L within rounding.
    fn assert_zap_out_round_trip(
        liquidity: u128,
        r_a: u128, // output side reserve
        r_b: u128, // other side
        ts: u128,
        fee: u128,
    ) {
        // FORWARD: simulate ZapOut to compute T
        let amt_a: u128 = (U256::from(liquidity) * U256::from(r_a) / U256::from(ts))
            .try_into()
            .unwrap();
        let amt_b: u128 = (U256::from(liquidity) * U256::from(r_b) / U256::from(ts))
            .try_into()
            .unwrap();
        let new_r_a = r_a - amt_a;
        let new_r_b = r_b - amt_b;
        let swap_out = calculate_swap_out(amt_b, new_r_b, new_r_a, fee).unwrap();
        let t_forward: u128 = amt_a + swap_out;
        assert!(t_forward > 0);

        // INVERSE: given T, find L'. Should be ≈ L (slightly higher due to ceil).
        let l_inverse = calculate_lp_for_exact_out(t_forward, r_a, ts, fee).unwrap();

        let diff = if l_inverse > liquidity {
            l_inverse - liquidity
        } else {
            liquidity - l_inverse
        };
        let tol = (liquidity / 1000).max(10);
        assert!(
            diff <= tol,
            "inverse L {} differs from forward {} by {} (>{:.2}%)",
            l_inverse,
            liquidity,
            diff,
            (tol as f64 / liquidity as f64) * 100.0
        );
    }

    #[test]
    fn zap_out_inverse_symmetric_pool() {
        assert_zap_out_round_trip(10_000_000, 100_000_000, 100_000_000, 100_000_000, 10);
        assert_zap_out_round_trip(50_000_000, 100_000_000, 100_000_000, 100_000_000, 10);
    }

    #[test]
    fn zap_out_inverse_asymmetric_pool() {
        // Pool 2:1 ratio
        assert_zap_out_round_trip(10_000_000, 200_000_000, 100_000_000, 100_000_000, 10);
        assert_zap_out_round_trip(10_000_000, 50_000_000, 200_000_000, 100_000_000, 10);
    }

    #[test]
    fn zap_out_inverse_high_fee() {
        // 5% fee
        assert_zap_out_round_trip(10_000_000, 100_000_000, 100_000_000, 100_000_000, 50);
    }

    #[test]
    fn zap_out_inverse_errors() {
        assert!(calculate_lp_for_exact_out(0, 1000, 1000, 10).is_err());
        assert!(calculate_lp_for_exact_out(100, 0, 1000, 10).is_err());
        assert!(calculate_lp_for_exact_out(100, 1000, 0, 10).is_err());
        // T >= R_a (requesting more output than the side holds)
        assert!(calculate_lp_for_exact_out(2000, 1000, 1000, 10).is_err());
        assert!(calculate_lp_for_exact_out(1000, 1000, 1000, 10).is_err());
        assert!(calculate_lp_for_exact_out(100, 1000, 1000, 1000).is_err()); // fee=1000
    }

    #[test]
    fn single_side_huge_values() {
        // ~u128 / 1e6 — must not overflow thanks to U256
        let amount = u128::MAX / 1_000_000;
        let r_in = u128::MAX / 2_000_000;
        let r_out = u128::MAX / 2_000_000;
        assert_dust_below(amount, r_in, r_out, 10, 100);
    }
}

