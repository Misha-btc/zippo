//! zippo — single-side zap + stake/bond router for oyl-amm pools.
//!
//! Opcodes:
//!   1   ZapIn              — single-asset → LP (exact-in)
//!   2   ZapInForExactLp    — single-asset → exact LP target (exact-out)
//!   3   ZapOut             — LP → single-asset (exact-in)
//!   6   ZapOutForExactOut  — LP → exact token target (exact-out)
//!   4   ZapInAndStake      — zap + forward LP into FIRE staking
//!   5   ZapInAndBond       — zap + forward LP into FIRE bonding
//!   9   ZapInAndStakeForExactLp  — exact-LP variant of ZapInAndStake
//!   10  ZapInAndBondForExactLp   — exact-LP variant of ZapInAndBond
//!   7   Stake              — standalone forward LP into staking
//!   8   Bond               — standalone forward LP into bonding
//!   50  Forward            — no-op deploy marker
//!   99  GetName            — view: returns "Zippo"
//!   100 MadeIn             — view: returns "Winnipeg"
//!
//! Baseline ZapIn flow:
//!   1. Sort the pair into the pool's canonical order (smaller → `/alkane/0`).
//!   2. Read reserves (97) and fee (20).
//!   3. Compute the optimal swap `s` via the quadratic formula.
//!   4. Swap `s` input → output directly on the pool (opcode 3).
//!   5. Call AddLiquidity (opcode 1) with the proportionally-capped deposit.
//!   6. Return the LP + dust + any other incoming tokens to the caller.

use alkanes_runtime::{
    declare_alkane, message::MessageDispatch, runtime::AlkaneResponder,
};
use alkanes_support::{
    cellpack::Cellpack,
    id::AlkaneId,
    parcel::{AlkaneTransfer, AlkaneTransferParcel},
    response::CallResponse,
};
use anyhow::{anyhow, Result};
use metashrew_support::compat::to_arraybuffer_layout;

pub mod amm_logic;

const DEFAULT_FEE_PER_1000: u128 = 10;

// ─── Message Dispatch ────────────────────────────────────────────────

#[derive(MessageDispatch)]
pub enum ZapMessage {
    /// Single-side zap → LP.
    /// `input_token` is what the caller sends in incoming.
    /// `output_token` is the other side of the pool (used for swap output).
    /// `amount_in` is the exact amount of `input_token` to zap. Must be
    /// `> 0` and `≤ total_incoming` (matching the `oyl-amm factory`
    /// convention). Excess `total_incoming − amount_in` is returned to
    /// the caller as change via the leftovers list (the same pattern as
    /// `_return_leftovers`).
    /// Any other tokens in incoming (including a stray `output_token`)
    /// are returned untouched.
    #[opcode(1)]
    ZapIn {
        pool: AlkaneId,
        input_token: AlkaneId,
        output_token: AlkaneId,
        amount_in: u128,
        min_lp_tokens: u128,
        deadline: u128,
    },

    /// Zap for an **exact LP target** — analog of `swap_tokens_for_exact_tokens`
    /// (opcode 14) on the oyl-amm factory. The caller specifies the
    /// desired `lp_out` and the maximum `amount_in_max` they're willing
    /// to spend. The contract derives the minimum `amount_in` via the
    /// closed-form formula, reverts if `amount_in > amount_in_max`, and
    /// otherwise executes the zap. The unused
    /// `amount_in_max − amount_in` is returned as change through the
    /// same leftover mechanism.
    #[opcode(2)]
    ZapInForExactLp {
        pool: AlkaneId,
        input_token: AlkaneId,
        output_token: AlkaneId,
        lp_out: u128,
        amount_in_max: u128,
        deadline: u128,
    },

    /// Zap + Stake: performs a single-side zap → LP, then forwards the
    /// LP into a FIRE staking contract with the given `lock_duration`.
    ///
    /// `lock_duration` is whitelist-validated — must be exactly one of:
    ///   `0` (no lock), `1050` (WEEK), `4375` (MONTH),
    ///   `13125` (3 MO), `26250` (6 MO), `52500` (YEAR).
    /// Anything else (e.g. `4374` or `52501`) reverts before any zap
    /// work is done, so the user cannot accidentally land in an
    /// unexpected multiplier tier.
    ///
    /// `staking` is the staking-clone for the current epoch (the
    /// UI/caller resolves it via
    /// `staking_factory.GetCurrentEpochIndex + GetEpochContract`).
    #[opcode(4)]
    ZapInAndStake {
        pool: AlkaneId,
        input_token: AlkaneId,
        output_token: AlkaneId,
        amount_in: u128,
        min_lp_tokens: u128,
        staking: AlkaneId,
        lock_duration: u128,
        deadline: u128,
    },

    /// Zap + Bond: performs a zap → LP, then forwards the LP into a FIRE
    /// bonding contract with `min_fire_out` (slippage guard on the bond
    /// side). The caller receives a bond NFT (plus immediate FIRE, if
    /// any), residuals, and leftovers.
    #[opcode(5)]
    ZapInAndBond {
        pool: AlkaneId,
        input_token: AlkaneId,
        output_token: AlkaneId,
        amount_in: u128,
        min_lp_tokens: u128,
        bonding: AlkaneId,
        min_fire_out: u128,
        deadline: u128,
    },

    /// Exact-out variant of `ZapInAndStake`: the caller asks to stake
    /// **exactly** `lp_out` LP in the FIRE staking contract. The contract
    /// derives the minimum `amount_in` via the closed-form inverse
    /// formula, reverts if it would exceed `amount_in_max`, otherwise
    /// runs the zap and forwards exactly `lp_out` LP into staking.
    /// Unused `amount_in_max − amount_in` is refunded as change.
    /// `lock_duration` is whitelist-validated (same set as opcode 4).
    #[opcode(9)]
    ZapInAndStakeForExactLp {
        pool: AlkaneId,
        input_token: AlkaneId,
        output_token: AlkaneId,
        lp_out: u128,
        amount_in_max: u128,
        staking: AlkaneId,
        lock_duration: u128,
        deadline: u128,
    },

    /// Exact-out variant of `ZapInAndBond`: stake **exactly** `lp_out`
    /// LP into the FIRE bonding contract. Closed-form inverse derives
    /// the minimum `amount_in`; reverts if > `amount_in_max`. Caller
    /// receives bond NFT + immediate FIRE (if any) + residuals + change.
    #[opcode(10)]
    ZapInAndBondForExactLp {
        pool: AlkaneId,
        input_token: AlkaneId,
        output_token: AlkaneId,
        lp_out: u128,
        amount_in_max: u128,
        bonding: AlkaneId,
        min_fire_out: u128,
        deadline: u128,
    },

    /// Single-asset unzap (LP → one token). Analog of the factory `burn`
    /// (opcode 12), but instead of returning the pair `(token_a, token_b)`
    /// it returns only `output_token`. The caller sends exactly
    /// `liquidity` LP via incoming (excess is refunded as change), picks
    /// `output_token` (one of the pool sides), and sets `min_out`.
    ///
    /// Algorithm:
    ///   1. Extract `liquidity` LP from incoming (excess → leftover).
    ///   2. `pool.WithdrawAndBurn(LP)` → `(amt_a, amt_b)` proportionally.
    ///   3. Swap the "unwanted" side → `output_token` on the same pool.
    ///   4. Final: `output_amount = own_side + swap_result`; revert if
    ///      `< min_out`.
    #[opcode(3)]
    ZapOut {
        pool: AlkaneId,
        output_token: AlkaneId,
        liquidity: u128,
        min_out: u128,
        deadline: u128,
    },

    /// Standalone stake: the caller sends LP directly, the contract
    /// validates `lock_duration` against the FIRE whitelist and forwards
    /// exactly `liquidity` LP into `staking`. Excess LP and any other
    /// incoming tokens are returned as change. Useful when LP is already
    /// in hand and only the validation + atomicity is needed.
    #[opcode(7)]
    Stake {
        pool: AlkaneId,
        staking: AlkaneId,
        liquidity: u128,
        lock_duration: u128,
        deadline: u128,
    },

    /// Standalone bond: the caller sends LP, the contract forwards
    /// exactly `liquidity` LP into `bonding` with `min_fire_out`. Excess
    /// LP and any other incoming tokens are returned as change.
    #[opcode(8)]
    Bond {
        pool: AlkaneId,
        bonding: AlkaneId,
        liquidity: u128,
        min_fire_out: u128,
        deadline: u128,
    },

    /// Zap-out for an **exact output target** — mirror of
    /// `ZapInForExactLp`. The caller specifies the desired
    /// `output_amount` (exact `output_token` amount) and `max_lp` (the
    /// maximum LP they're willing to burn). The contract derives the
    /// minimum L via the inverse formula, reverts if `L > max_lp`, and
    /// otherwise executes ZapOut. Unused LP is refunded as change.
    #[opcode(6)]
    ZapOutForExactOut {
        pool: AlkaneId,
        output_token: AlkaneId,
        output_amount: u128,
        max_lp: u128,
        deadline: u128,
    },

    /// Forward incoming alkanes unchanged. Safe no-op used as a deploy
    /// marker.
    #[opcode(50)]
    Forward {},

    /// Returns the contract name `"Zippo"`. Matches the oyl-amm pool
    /// convention (opcode 99 = `GetName`).
    #[opcode(99)]
    #[returns(String)]
    GetName {},

    /// Returns the string `"Winnipeg"`. Lightweight ident getter — no
    /// incoming alkanes consumed, no cross-contract calls.
    #[opcode(100)]
    #[returns(String)]
    MadeIn {},
}

// ─── Contract ────────────────────────────────────────────────────────

#[derive(Default)]
pub struct Zap();

impl AlkaneResponder for Zap {}

/// Decomposition of a zap-to-LP result: LP minted, residuals after the
/// proportional cap, and leftovers from incoming. Wrappers decide how to
/// package this — `zap_in` returns LP to the caller, `zap_in_and_stake`
/// forwards it into staking, `zap_in_and_bond` into bonding, etc.
struct ZapInResult {
    lp_received: u128,
    pool: AlkaneId,
    input_token: AlkaneId,
    output_token: AlkaneId,
    residual_input: u128,
    residual_output: u128,
    leftovers: Vec<AlkaneTransfer>,
}

/// Collapse repeated AlkaneId entries in a transfer list into a single
/// transfer per id (matches `oyl-amm factory::_return_leftovers`
/// formatting — one transfer per token, deterministic order).
///
/// The alkanes runtime sums duplicates during routing anyway; this is
/// purely cosmetic for trace inspection.
fn aggregate_transfers(transfers: Vec<AlkaneTransfer>) -> Vec<AlkaneTransfer> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<AlkaneId, u128> = BTreeMap::new();
    for tr in transfers {
        let entry = map.entry(tr.id).or_insert(0);
        *entry = entry.saturating_add(tr.value);
    }
    map.into_iter()
        .filter(|(_, v)| *v > 0)
        .map(|(id, value)| AlkaneTransfer { id, value })
        .collect()
}

/// Whitelist for FIRE staking `lock_duration`. Only the exact values from
/// `fire-constants` are accepted — any other duration reverts (even one
/// that's "almost" right). This ensures the user lands in the intended
/// multiplier tier and doesn't accidentally end up with, say, 4374 blocks
/// of lock that technically gives 1.25× instead of 1.5×.
///
/// Accepted values (from `crates/fire-constants/src/lib.rs`):
///   * `0`      — no lock (multiplier 1.0×)
///   * `1_050`  — WEEK (1.25×)
///   * `4_375`  — MONTH (1.5×)
///   * `13_125` — THREE_MONTHS (2.0×)
///   * `26_250` — SIX_MONTHS (2.5×)
///   * `52_500` — YEAR (3.0×)
fn validate_lock_duration(lock_duration: u128) -> Result<()> {
    match lock_duration {
        0 | 1_050 | 4_375 | 13_125 | 26_250 | 52_500 => Ok(()),
        _ => Err(anyhow!(
            "invalid lock_duration {} (expected exact: 0, 1050, 4375, 13125, 26250, 52500)",
            lock_duration
        )),
    }
}

/// Canonical pool token ordering — same convention as the oyl-amm factory
/// (the smaller AlkaneId maps to `/alkane/0`).
fn sort_alkanes(a: AlkaneId, b: AlkaneId) -> (AlkaneId, AlkaneId) {
    if a < b { (a, b) } else { (b, a) }
}

impl Zap {
    // ── Pool queries ─────────────────────────────────────────────────

    /// GetReserves (opcode 97) → `(reserve_0, reserve_1)`.
    fn query_reserves(&self, pool: AlkaneId) -> Result<(u128, u128)> {
        let resp = self.staticcall(
            &Cellpack { target: pool, inputs: vec![97] },
            &AlkaneTransferParcel::default(),
            self.fuel(),
        )?;
        if resp.data.len() < 32 {
            return Err(anyhow!("GetReserves too short: {}", resp.data.len()));
        }
        let r0 = u128::from_le_bytes(resp.data[0..16].try_into().unwrap());
        let r1 = u128::from_le_bytes(resp.data[16..32].try_into().unwrap());
        Ok((r0, r1))
    }

    /// PoolDetails (opcode 999) → parsed `total_supply`. Needed only by
    /// `ZapInForExactLp` for the exact-`amount_in` inverse formula —
    /// the oyl-amm pool has no dedicated getter for `total_supply`.
    fn query_total_supply(&self, pool: AlkaneId) -> Result<u128> {
        let resp = self.staticcall(
            &Cellpack { target: pool, inputs: vec![999] },
            &AlkaneTransferParcel::default(),
            self.fuel(),
        )?;
        if resp.data.len() < 112 {
            return Err(anyhow!("PoolDetails too short for total_supply"));
        }
        Ok(u128::from_le_bytes(resp.data[96..112].try_into().unwrap()))
    }

    /// GetTotalFee (opcode 20) → `fee_per_1000`. Falls back to default.
    fn query_fee_per_1000(&self, pool: AlkaneId) -> u128 {
        let resp = self.staticcall(
            &Cellpack { target: pool, inputs: vec![20] },
            &AlkaneTransferParcel::default(),
            self.fuel(),
        );
        match resp {
            Ok(r) if r.data.len() >= 16 => {
                u128::from_le_bytes(r.data[0..16].try_into().unwrap())
            }
            _ => DEFAULT_FEE_PER_1000,
        }
    }

    // ── Pool calls ───────────────────────────────────────────────────

    /// Swap (opcode 3). `to = 0:0, data = []` → output is returned to zap.
    fn call_swap(
        &self,
        pool: AlkaneId,
        input_token: AlkaneId,
        amount_in: u128,
        amount_0_out: u128,
        amount_1_out: u128,
    ) -> Result<CallResponse> {
        let cellpack = Cellpack {
            target: pool,
            inputs: vec![3, amount_0_out, amount_1_out, 0, 0, 0],
        };
        let parcel = AlkaneTransferParcel(vec![AlkaneTransfer {
            id: input_token,
            value: amount_in,
        }]);
        self.call(&cellpack, &parcel, self.fuel())
    }

    /// AddLiquidity (opcode 1). The pool requires exactly 2 transfers.
    fn call_add_liquidity(
        &self,
        pool: AlkaneId,
        token_0: AlkaneId,
        amount_0: u128,
        token_1: AlkaneId,
        amount_1: u128,
    ) -> Result<CallResponse> {
        let cellpack = Cellpack {
            target: pool,
            inputs: vec![1],
        };
        let parcel = AlkaneTransferParcel(vec![
            AlkaneTransfer { id: token_0, value: amount_0 },
            AlkaneTransfer { id: token_1, value: amount_1 },
        ]);
        self.call(&cellpack, &parcel, self.fuel())
    }

    /// WithdrawAndBurn (opcode 2) — send `lp_amount` LP, get back
    /// proportional `(token_a, token_b)`. The pool checks that incoming
    /// contains exactly one LP transfer (`pool == self.myself`).
    fn call_withdraw_and_burn(
        &self,
        pool: AlkaneId,
        lp_amount: u128,
    ) -> Result<CallResponse> {
        let cellpack = Cellpack {
            target: pool,
            inputs: vec![2],
        };
        let parcel = AlkaneTransferParcel(vec![AlkaneTransfer {
            id: pool, // LP token IS the pool's own AlkaneId
            value: lp_amount,
        }]);
        self.call(&cellpack, &parcel, self.fuel())
    }

    // ── Helpers ──────────────────────────────────────────────────────

    fn check_deadline(&self, deadline: u128) -> Result<()> {
        if deadline != 0 && (self.height() as u128) > deadline {
            return Err(anyhow!(
                "EXPIRED: block {} > deadline {}",
                self.height(),
                deadline
            ));
        }
        Ok(())
    }

    /// Extract exactly `amount_in` of `input_token` from incoming.
    /// Matches the `oyl-amm factory` behaviour: the caller always
    /// specifies an exact amount; `amount_in == 0` or
    /// `amount_in > total_incoming` reverts. The excess
    /// (`total − amount_in`) plus any other tokens go into leftovers and
    /// are returned to the caller.
    fn extract_input(
        &self,
        input_token: AlkaneId,
        amount_in: u128,
    ) -> Result<(u128, Vec<AlkaneTransfer>)> {
        if amount_in == 0 {
            return Err(anyhow!("amount_in must be > 0"));
        }
        let context = self.context()?;
        let mut total_in = 0u128;
        let mut leftovers = Vec::new();
        for tr in &context.incoming_alkanes.0 {
            if tr.id == input_token {
                total_in = total_in.saturating_add(tr.value);
            } else {
                leftovers.push(tr.clone());
            }
        }
        if total_in == 0 {
            return Err(anyhow!("input_token not present in incoming"));
        }
        if amount_in > total_in {
            return Err(anyhow!(
                "amount_in {} > available input {}",
                amount_in, total_in
            ));
        }
        let excess = total_in - amount_in;
        if excess > 0 {
            leftovers.push(AlkaneTransfer {
                id: input_token,
                value: excess,
            });
        }
        Ok((amount_in, leftovers))
    }

    // ── Opcode handlers ──────────────────────────────────────────────

    fn zap_in(
        &self,
        pool: AlkaneId,
        input_token: AlkaneId,
        output_token: AlkaneId,
        amount_in: u128,
        min_lp_tokens: u128,
        deadline: u128,
    ) -> Result<CallResponse> {
        self.check_deadline(deadline)?;
        if input_token == output_token {
            return Err(anyhow!("input_token == output_token"));
        }

        // Sort into the pool's canonical order and remember which side is input.
        let (token_0, token_1) = sort_alkanes(input_token, output_token);
        let input_is_0 = input_token == token_0;

        let (reserve_0, reserve_1) = self.query_reserves(pool)?;
        if reserve_0 == 0 || reserve_1 == 0 {
            return Err(anyhow!("pool not seeded — single-side zap impossible"));
        }
        let fee = self.query_fee_per_1000(pool);

        let (amount_in, leftovers) = self.extract_input(input_token, amount_in)?;

        // Reserves on the input / output side.
        let (reserve_in, reserve_out) = if input_is_0 {
            (reserve_0, reserve_1)
        } else {
            (reserve_1, reserve_0)
        };

        // 1) Optimal swap.
        let swap_amount =
            amm_logic::calculate_single_side_swap(amount_in, reserve_in, fee);
        if swap_amount == 0 {
            return Err(anyhow!("amount_in too small to compute optimal swap split"));
        }

        // 2) Swap input → output. amount_0_out / amount_1_out follow pool ordering.
        let expected_out = amm_logic::calculate_swap_out(
            swap_amount, reserve_in, reserve_out, fee,
        )?;
        let (amt_0_out, amt_1_out) = if input_is_0 {
            (0u128, expected_out)
        } else {
            (expected_out, 0u128)
        };
        let swap_resp = self.call_swap(
            pool, input_token, swap_amount, amt_0_out, amt_1_out,
        )?;

        let mut got_output = 0u128;
        for tr in &swap_resp.alkanes.0 {
            if tr.id == output_token {
                got_output = got_output.saturating_add(tr.value);
            }
        }
        if got_output == 0 {
            return Err(anyhow!("pool swap returned 0 output (input too small or pool degenerate)"));
        }

        let have_input = amount_in - swap_amount;
        let have_output = got_output;
        let new_reserve_in = reserve_in.saturating_add(swap_amount);
        let new_reserve_out = reserve_out.saturating_sub(got_output);

        // 3) Cap to the exact ratio — otherwise AddLiquidity would eat dust.
        let (deposit_input, deposit_output) = {
            let out_for_in =
                amm_logic::mul_div(have_input, new_reserve_out, new_reserve_in);
            if out_for_in <= have_output {
                (have_input, out_for_in)
            } else {
                let in_for_out =
                    amm_logic::mul_div(have_output, new_reserve_in, new_reserve_out);
                (in_for_out, have_output)
            }
        };
        if deposit_input == 0 || deposit_output == 0 {
            return Err(anyhow!("computed deposit is zero on one side (amount_in too small for this pool)"));
        }

        // 4) AddLiquidity — pass amounts in pool order.
        let (amount_0_dep, amount_1_dep) = if input_is_0 {
            (deposit_input, deposit_output)
        } else {
            (deposit_output, deposit_input)
        };
        let add_resp = self.call_add_liquidity(
            pool, token_0, amount_0_dep, token_1, amount_1_dep,
        )?;

        // 5) Assemble the result: LP + dust + leftovers.
        let mut response = CallResponse::default();
        let mut lp_received = 0u128;
        for tr in &add_resp.alkanes.0 {
            if tr.id == pool {
                lp_received = lp_received.saturating_add(tr.value);
            }
            response.alkanes.0.push(tr.clone());
        }
        if lp_received < min_lp_tokens {
            return Err(anyhow!(
                "insufficient LP: got {}, want >= {}",
                lp_received,
                min_lp_tokens
            ));
        }

        // Change goes back to the caller.
        let residual_input = have_input.saturating_sub(deposit_input);
        let residual_output = have_output.saturating_sub(deposit_output);
        if residual_input > 0 {
            response.alkanes.0.push(AlkaneTransfer {
                id: input_token,
                value: residual_input,
            });
        }
        if residual_output > 0 {
            response.alkanes.0.push(AlkaneTransfer {
                id: output_token,
                value: residual_output,
            });
        }

        // Other incoming tokens — return them untouched.
        for tr in leftovers {
            response.alkanes.0.push(tr);
        }

        response.alkanes.0 = aggregate_transfers(response.alkanes.0);

        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&lp_received.to_le_bytes());
        response.data = data;
        Ok(response)
    }

    fn zap_in_for_exact_lp(
        &self,
        pool: AlkaneId,
        input_token: AlkaneId,
        output_token: AlkaneId,
        lp_out: u128,
        amount_in_max: u128,
        deadline: u128,
    ) -> Result<CallResponse> {
        self.check_deadline(deadline)?;
        if input_token == output_token {
            return Err(anyhow!("input_token == output_token"));
        }
        if lp_out == 0 {
            return Err(anyhow!("lp_out must be > 0"));
        }
        if amount_in_max == 0 {
            return Err(anyhow!("amount_in_max must be > 0"));
        }

        let (token_0, _token_1) = sort_alkanes(input_token, output_token);
        let input_is_0 = input_token == token_0;

        let (reserve_0, reserve_1) = self.query_reserves(pool)?;
        if reserve_0 == 0 || reserve_1 == 0 {
            return Err(anyhow!("pool not seeded — single-side zap impossible"));
        }
        let fee = self.query_fee_per_1000(pool);
        let total_supply = self.query_total_supply(pool)?;
        if total_supply == 0 {
            return Err(anyhow!("pool total_supply is zero"));
        }

        let r_in = if input_is_0 { reserve_0 } else { reserve_1 };

        // Closed-form: derive the exact amount_in needed for the target LP.
        let (amount_in, _) = amm_logic::calculate_amount_in_for_exact_lp(
            lp_out, r_in, total_supply, fee,
        )?;

        if amount_in > amount_in_max {
            return Err(anyhow!(
                "required amount_in {} > amount_in_max {}",
                amount_in, amount_in_max
            ));
        }

        // Reuse the already-read reserves+fee (state is immutable until
        // the swap fires) — call `_with_state` instead of reading them a
        // second time (saves 2 staticcalls ≈ 175K fuel).
        let r = self.execute_zap_to_lp_with_state(
            pool, input_token, output_token, amount_in, lp_out,
            reserve_0, reserve_1, fee,
        )?;

        // Build the response: LP + residuals + leftovers (same shape as opcode 1).
        let mut response = CallResponse::default();
        response.alkanes.0.push(AlkaneTransfer {
            id: r.pool,
            value: r.lp_received,
        });
        if r.residual_input > 0 {
            response.alkanes.0.push(AlkaneTransfer {
                id: r.input_token,
                value: r.residual_input,
            });
        }
        if r.residual_output > 0 {
            response.alkanes.0.push(AlkaneTransfer {
                id: r.output_token,
                value: r.residual_output,
            });
        }
        for tr in r.leftovers {
            response.alkanes.0.push(tr);
        }
        response.alkanes.0 = aggregate_transfers(response.alkanes.0);

        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&r.lp_received.to_le_bytes());
        response.data = data;
        Ok(response)
    }

    /// Core zap-to-LP logic that does *not* build a `CallResponse`. It
    /// returns the components so callers can decide where the LP goes —
    /// hand it back to the user (as in `zap_in`) or forward it into a
    /// staking/bonding contract. Thin wrapper: reads pool state
    /// (reserves, fee) and delegates to `_with_state`.
    fn execute_zap_to_lp(
        &self,
        pool: AlkaneId,
        input_token: AlkaneId,
        output_token: AlkaneId,
        amount_in: u128,
        min_lp_tokens: u128,
    ) -> Result<ZapInResult> {
        let (reserve_0, reserve_1) = self.query_reserves(pool)?;
        if reserve_0 == 0 || reserve_1 == 0 {
            return Err(anyhow!("pool not seeded — single-side zap impossible"));
        }
        let fee = self.query_fee_per_1000(pool);
        self.execute_zap_to_lp_with_state(
            pool, input_token, output_token, amount_in, min_lp_tokens,
            reserve_0, reserve_1, fee,
        )
    }

    /// "Hot path" without duplicate pool reads — used when the caller
    /// (`zap_in_for_exact_lp` and derivatives) has already read
    /// reserves+fee for its own math. Within a single runtime invocation
    /// pool state is immutable between our mutate ops, so reusing the
    /// snapshot is safe.
    fn execute_zap_to_lp_with_state(
        &self,
        pool: AlkaneId,
        input_token: AlkaneId,
        output_token: AlkaneId,
        amount_in: u128,
        min_lp_tokens: u128,
        reserve_0: u128,
        reserve_1: u128,
        fee: u128,
    ) -> Result<ZapInResult> {
        let (token_0, token_1) = sort_alkanes(input_token, output_token);
        let input_is_0 = input_token == token_0;

        let (amount_in, leftovers) = self.extract_input(input_token, amount_in)?;

        let (reserve_in, reserve_out) = if input_is_0 {
            (reserve_0, reserve_1)
        } else {
            (reserve_1, reserve_0)
        };

        let swap_amount =
            amm_logic::calculate_single_side_swap(amount_in, reserve_in, fee);
        if swap_amount == 0 {
            return Err(anyhow!("amount_in too small to compute optimal swap split"));
        }

        let expected_out = amm_logic::calculate_swap_out(
            swap_amount, reserve_in, reserve_out, fee,
        )?;
        let (amt_0_out, amt_1_out) = if input_is_0 {
            (0u128, expected_out)
        } else {
            (expected_out, 0u128)
        };
        let swap_resp = self.call_swap(
            pool, input_token, swap_amount, amt_0_out, amt_1_out,
        )?;

        let mut got_output = 0u128;
        for tr in &swap_resp.alkanes.0 {
            if tr.id == output_token {
                got_output = got_output.saturating_add(tr.value);
            }
        }
        if got_output == 0 {
            return Err(anyhow!("pool swap returned 0 output (input too small or pool degenerate)"));
        }

        let have_input = amount_in - swap_amount;
        let have_output = got_output;
        let new_reserve_in = reserve_in.saturating_add(swap_amount);
        let new_reserve_out = reserve_out.saturating_sub(got_output);

        let (deposit_input, deposit_output) = {
            let out_for_in =
                amm_logic::mul_div(have_input, new_reserve_out, new_reserve_in);
            if out_for_in <= have_output {
                (have_input, out_for_in)
            } else {
                let in_for_out =
                    amm_logic::mul_div(have_output, new_reserve_in, new_reserve_out);
                (in_for_out, have_output)
            }
        };
        if deposit_input == 0 || deposit_output == 0 {
            return Err(anyhow!("computed deposit is zero on one side (amount_in too small for this pool)"));
        }

        let (amount_0_dep, amount_1_dep) = if input_is_0 {
            (deposit_input, deposit_output)
        } else {
            (deposit_output, deposit_input)
        };
        let add_resp = self.call_add_liquidity(
            pool, token_0, amount_0_dep, token_1, amount_1_dep,
        )?;

        let mut lp_received = 0u128;
        for tr in &add_resp.alkanes.0 {
            if tr.id == pool {
                lp_received = lp_received.saturating_add(tr.value);
            }
        }
        if lp_received < min_lp_tokens {
            return Err(anyhow!(
                "insufficient LP: got {}, want >= {}",
                lp_received,
                min_lp_tokens
            ));
        }

        let residual_input = have_input.saturating_sub(deposit_input);
        let residual_output = have_output.saturating_sub(deposit_output);

        Ok(ZapInResult {
            lp_received,
            pool,
            input_token,
            output_token,
            residual_input,
            residual_output,
            leftovers,
        })
    }

    /// Forwards `lp_amount` LP into `staking` opcode 1
    /// (`Stake { lock_duration, amount }`). Returns the staking response
    /// (typically the position NFT). Used by opcodes 4 (ZapInAndStake)
    /// and 7 (Stake).
    fn call_staking(
        &self,
        staking: AlkaneId,
        pool: AlkaneId,
        lp_amount: u128,
        lock_duration: u128,
    ) -> Result<CallResponse> {
        self.call(
            &Cellpack {
                target: staking,
                inputs: vec![1, lock_duration, lp_amount],
            },
            &AlkaneTransferParcel(vec![AlkaneTransfer {
                id: pool,
                value: lp_amount,
            }]),
            self.fuel(),
        )
    }

    /// Forwards `lp_amount` LP into `bonding` opcode 1
    /// (`Bond { lp_to_bond, min_fire_out }`). Used by opcodes 5
    /// (ZapInAndBond) and 8 (Bond).
    fn call_bonding(
        &self,
        bonding: AlkaneId,
        pool: AlkaneId,
        lp_amount: u128,
        min_fire_out: u128,
    ) -> Result<CallResponse> {
        self.call(
            &Cellpack {
                target: bonding,
                inputs: vec![1, lp_amount, min_fire_out],
            },
            &AlkaneTransferParcel(vec![AlkaneTransfer {
                id: pool,
                value: lp_amount,
            }]),
            self.fuel(),
        )
    }

    // ── ZapIn + Stake (opcode 4) ─────────────────────────────────────

    fn zap_in_and_stake(
        &self,
        pool: AlkaneId,
        input_token: AlkaneId,
        output_token: AlkaneId,
        amount_in: u128,
        min_lp_tokens: u128,
        staking: AlkaneId,
        lock_duration: u128,
        deadline: u128,
    ) -> Result<CallResponse> {
        self.check_deadline(deadline)?;
        if input_token == output_token {
            return Err(anyhow!("input_token == output_token"));
        }
        // Whitelist check: lock_duration must be an exact value from
        // FIRE constants. Revert before any zap work so we don't burn
        // fuel on pool calls for an invalid duration.
        validate_lock_duration(lock_duration)?;

        let r = self.execute_zap_to_lp(
            pool, input_token, output_token, amount_in, min_lp_tokens,
        )?;

        let stake_resp = self.call_staking(
            staking, r.pool, r.lp_received, lock_duration,
        )?;

        // Build response: position NFT (from staking) + residuals + leftovers
        let mut response = CallResponse::default();
        for tr in &stake_resp.alkanes.0 {
            response.alkanes.0.push(tr.clone());
        }
        if r.residual_input > 0 {
            response.alkanes.0.push(AlkaneTransfer {
                id: r.input_token,
                value: r.residual_input,
            });
        }
        if r.residual_output > 0 {
            response.alkanes.0.push(AlkaneTransfer {
                id: r.output_token,
                value: r.residual_output,
            });
        }
        for tr in r.leftovers {
            response.alkanes.0.push(tr);
        }
        response.alkanes.0 = aggregate_transfers(response.alkanes.0);

        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&r.lp_received.to_le_bytes());
        response.data = data;
        Ok(response)
    }

    // ── ZapIn + Bond (opcode 5) ──────────────────────────────────────

    // ── Standalone Stake (opcode 7) ──────────────────────────────────

    fn stake(
        &self,
        pool: AlkaneId,
        staking: AlkaneId,
        liquidity: u128,
        lock_duration: u128,
        deadline: u128,
    ) -> Result<CallResponse> {
        self.check_deadline(deadline)?;
        validate_lock_duration(lock_duration)?;
        if liquidity == 0 {
            return Err(anyhow!("liquidity must be > 0"));
        }

        let (lp_amount, leftovers) = self.extract_input(pool, liquidity)?;
        let stake_resp = self.call_staking(staking, pool, lp_amount, lock_duration)?;

        // Build response: NFT (from staking) + leftovers
        let mut response = CallResponse::default();
        for tr in &stake_resp.alkanes.0 {
            response.alkanes.0.push(tr.clone());
        }
        for tr in leftovers {
            response.alkanes.0.push(tr);
        }
        response.alkanes.0 = aggregate_transfers(response.alkanes.0);

        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&lp_amount.to_le_bytes());
        response.data = data;
        Ok(response)
    }

    // ── Standalone Bond (opcode 8) ───────────────────────────────────

    fn bond(
        &self,
        pool: AlkaneId,
        bonding: AlkaneId,
        liquidity: u128,
        min_fire_out: u128,
        deadline: u128,
    ) -> Result<CallResponse> {
        self.check_deadline(deadline)?;
        if liquidity == 0 {
            return Err(anyhow!("liquidity must be > 0"));
        }

        let (lp_amount, leftovers) = self.extract_input(pool, liquidity)?;
        let bond_resp = self.call_bonding(bonding, pool, lp_amount, min_fire_out)?;

        // Build response: bond-NFT + immediate FIRE (if any) + leftovers
        let mut response = CallResponse::default();
        for tr in &bond_resp.alkanes.0 {
            response.alkanes.0.push(tr.clone());
        }
        for tr in leftovers {
            response.alkanes.0.push(tr);
        }
        response.alkanes.0 = aggregate_transfers(response.alkanes.0);

        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&lp_amount.to_le_bytes());
        response.data = data;
        Ok(response)
    }

    fn zap_in_and_bond(
        &self,
        pool: AlkaneId,
        input_token: AlkaneId,
        output_token: AlkaneId,
        amount_in: u128,
        min_lp_tokens: u128,
        bonding: AlkaneId,
        min_fire_out: u128,
        deadline: u128,
    ) -> Result<CallResponse> {
        self.check_deadline(deadline)?;
        if input_token == output_token {
            return Err(anyhow!("input_token == output_token"));
        }
        let r = self.execute_zap_to_lp(
            pool, input_token, output_token, amount_in, min_lp_tokens,
        )?;

        let bond_resp = self.call_bonding(
            bonding, r.pool, r.lp_received, min_fire_out,
        )?;

        // Build response: bond-NFT + immediate FIRE (if any) + residuals + leftovers
        let mut response = CallResponse::default();
        for tr in &bond_resp.alkanes.0 {
            response.alkanes.0.push(tr.clone());
        }
        if r.residual_input > 0 {
            response.alkanes.0.push(AlkaneTransfer {
                id: r.input_token,
                value: r.residual_input,
            });
        }
        if r.residual_output > 0 {
            response.alkanes.0.push(AlkaneTransfer {
                id: r.output_token,
                value: r.residual_output,
            });
        }
        for tr in r.leftovers {
            response.alkanes.0.push(tr);
        }
        response.alkanes.0 = aggregate_transfers(response.alkanes.0);

        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&r.lp_received.to_le_bytes());
        response.data = data;
        Ok(response)
    }

    // ── ZapIn + Stake for exact LP (opcode 9) ────────────────────────

    fn zap_in_and_stake_for_exact_lp(
        &self,
        pool: AlkaneId,
        input_token: AlkaneId,
        output_token: AlkaneId,
        lp_out: u128,
        amount_in_max: u128,
        staking: AlkaneId,
        lock_duration: u128,
        deadline: u128,
    ) -> Result<CallResponse> {
        self.check_deadline(deadline)?;
        if input_token == output_token {
            return Err(anyhow!("input_token == output_token"));
        }
        validate_lock_duration(lock_duration)?;
        if lp_out == 0 {
            return Err(anyhow!("lp_out must be > 0"));
        }
        if amount_in_max == 0 {
            return Err(anyhow!("amount_in_max must be > 0"));
        }

        let (token_0, _token_1) = sort_alkanes(input_token, output_token);
        let input_is_0 = input_token == token_0;

        let (reserve_0, reserve_1) = self.query_reserves(pool)?;
        if reserve_0 == 0 || reserve_1 == 0 {
            return Err(anyhow!("pool not seeded — single-side zap impossible"));
        }
        let fee = self.query_fee_per_1000(pool);
        let total_supply = self.query_total_supply(pool)?;
        if total_supply == 0 {
            return Err(anyhow!("pool total_supply is zero"));
        }

        let r_in = if input_is_0 { reserve_0 } else { reserve_1 };
        let (amount_in, _) = amm_logic::calculate_amount_in_for_exact_lp(
            lp_out, r_in, total_supply, fee,
        )?;
        if amount_in > amount_in_max {
            return Err(anyhow!(
                "required amount_in {} > amount_in_max {}",
                amount_in, amount_in_max
            ));
        }

        let r = self.execute_zap_to_lp_with_state(
            pool, input_token, output_token, amount_in, lp_out,
            reserve_0, reserve_1, fee,
        )?;

        let stake_resp = self.call_staking(
            staking, r.pool, r.lp_received, lock_duration,
        )?;

        let mut response = CallResponse::default();
        for tr in &stake_resp.alkanes.0 {
            response.alkanes.0.push(tr.clone());
        }
        if r.residual_input > 0 {
            response.alkanes.0.push(AlkaneTransfer {
                id: r.input_token,
                value: r.residual_input,
            });
        }
        if r.residual_output > 0 {
            response.alkanes.0.push(AlkaneTransfer {
                id: r.output_token,
                value: r.residual_output,
            });
        }
        for tr in r.leftovers {
            response.alkanes.0.push(tr);
        }
        response.alkanes.0 = aggregate_transfers(response.alkanes.0);

        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&r.lp_received.to_le_bytes());
        response.data = data;
        Ok(response)
    }

    // ── ZapIn + Bond for exact LP (opcode 10) ────────────────────────

    fn zap_in_and_bond_for_exact_lp(
        &self,
        pool: AlkaneId,
        input_token: AlkaneId,
        output_token: AlkaneId,
        lp_out: u128,
        amount_in_max: u128,
        bonding: AlkaneId,
        min_fire_out: u128,
        deadline: u128,
    ) -> Result<CallResponse> {
        self.check_deadline(deadline)?;
        if input_token == output_token {
            return Err(anyhow!("input_token == output_token"));
        }
        if lp_out == 0 {
            return Err(anyhow!("lp_out must be > 0"));
        }
        if amount_in_max == 0 {
            return Err(anyhow!("amount_in_max must be > 0"));
        }

        let (token_0, _token_1) = sort_alkanes(input_token, output_token);
        let input_is_0 = input_token == token_0;

        let (reserve_0, reserve_1) = self.query_reserves(pool)?;
        if reserve_0 == 0 || reserve_1 == 0 {
            return Err(anyhow!("pool not seeded — single-side zap impossible"));
        }
        let fee = self.query_fee_per_1000(pool);
        let total_supply = self.query_total_supply(pool)?;
        if total_supply == 0 {
            return Err(anyhow!("pool total_supply is zero"));
        }

        let r_in = if input_is_0 { reserve_0 } else { reserve_1 };
        let (amount_in, _) = amm_logic::calculate_amount_in_for_exact_lp(
            lp_out, r_in, total_supply, fee,
        )?;
        if amount_in > amount_in_max {
            return Err(anyhow!(
                "required amount_in {} > amount_in_max {}",
                amount_in, amount_in_max
            ));
        }

        let r = self.execute_zap_to_lp_with_state(
            pool, input_token, output_token, amount_in, lp_out,
            reserve_0, reserve_1, fee,
        )?;

        let bond_resp = self.call_bonding(
            bonding, r.pool, r.lp_received, min_fire_out,
        )?;

        let mut response = CallResponse::default();
        for tr in &bond_resp.alkanes.0 {
            response.alkanes.0.push(tr.clone());
        }
        if r.residual_input > 0 {
            response.alkanes.0.push(AlkaneTransfer {
                id: r.input_token,
                value: r.residual_input,
            });
        }
        if r.residual_output > 0 {
            response.alkanes.0.push(AlkaneTransfer {
                id: r.output_token,
                value: r.residual_output,
            });
        }
        for tr in r.leftovers {
            response.alkanes.0.push(tr);
        }
        response.alkanes.0 = aggregate_transfers(response.alkanes.0);

        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&r.lp_received.to_le_bytes());
        response.data = data;
        Ok(response)
    }

    fn zap_out(
        &self,
        pool: AlkaneId,
        output_token: AlkaneId,
        liquidity: u128,
        min_out: u128,
        deadline: u128,
    ) -> Result<CallResponse> {
        self.check_deadline(deadline)?;
        if liquidity == 0 {
            return Err(anyhow!("liquidity must be > 0"));
        }

        // Read pool state once; pass to hot-path helper.
        let (token_a, token_b, reserve_a, reserve_b, _ts) =
            self.query_pool_details_full(pool)?;
        if output_token != token_a && output_token != token_b {
            return Err(anyhow!(
                "output_token {:?} is not in pool ({:?}, {:?})",
                output_token, token_a, token_b
            ));
        }
        let fee = self.query_fee_per_1000(pool);

        self.execute_zap_out_with_state(
            pool, output_token, liquidity, min_out,
            token_a, token_b, reserve_a, reserve_b, fee,
        )
    }

    /// "Hot path" ZapOut without duplicate pool reads — used when the
    /// caller (e.g. `zap_out_for_exact_out`) has already read tokens,
    /// reserves, and fee for its own math.
    fn execute_zap_out_with_state(
        &self,
        pool: AlkaneId,
        output_token: AlkaneId,
        liquidity: u128,
        min_out: u128,
        token_a: AlkaneId,
        token_b: AlkaneId,
        reserve_a: u128,
        reserve_b: u128,
        fee: u128,
    ) -> Result<CallResponse> {
        // Extract LP. Pool IS its own LP token (alkane_id same).
        let (lp_amount, leftovers) = self.extract_input(pool, liquidity)?;

        // Step 1: WithdrawAndBurn → (amt_a, amt_b) proportional
        let withdraw_resp = self.call_withdraw_and_burn(pool, lp_amount)?;
        let mut amt_a = 0u128;
        let mut amt_b = 0u128;
        for tr in &withdraw_resp.alkanes.0 {
            if tr.id == token_a {
                amt_a = amt_a.saturating_add(tr.value);
            } else if tr.id == token_b {
                amt_b = amt_b.saturating_add(tr.value);
            }
        }
        if amt_a == 0 || amt_b == 0 {
            return Err(anyhow!("pool burn returned 0 on one side (liquidity too small)"));
        }

        // Step 2: swap "other" side fully → output_token.
        // Pool reserves AFTER withdraw.
        let new_r_a = reserve_a.saturating_sub(amt_a);
        let new_r_b = reserve_b.saturating_sub(amt_b);

        let (input_token, input_amt, own_side, r_in, r_out, output_is_a) =
            if output_token == token_a {
                (token_b, amt_b, amt_a, new_r_b, new_r_a, true)
            } else {
                (token_a, amt_a, amt_b, new_r_a, new_r_b, false)
            };

        let swap_received = if input_amt > 0 && r_in > 0 && r_out > 0 {
            let expected_out =
                amm_logic::calculate_swap_out(input_amt, r_in, r_out, fee)?;
            // amount_0_out is token_a side, amount_1_out is token_b side.
            let (amt_0_out, amt_1_out) = if output_is_a {
                (expected_out, 0u128)
            } else {
                (0u128, expected_out)
            };
            let swap_resp = self.call_swap(
                pool, input_token, input_amt, amt_0_out, amt_1_out,
            )?;
            let mut got = 0u128;
            for tr in &swap_resp.alkanes.0 {
                if tr.id == output_token {
                    got = got.saturating_add(tr.value);
                }
            }
            got
        } else {
            0
        };

        let total_out = own_side.saturating_add(swap_received);
        if total_out < min_out {
            return Err(anyhow!(
                "insufficient output: got {}, want >= {}",
                total_out, min_out
            ));
        }

        let mut response = CallResponse::default();
        response.alkanes.0.push(AlkaneTransfer {
            id: output_token,
            value: total_out,
        });
        for tr in leftovers {
            response.alkanes.0.push(tr);
        }

        response.alkanes.0 = aggregate_transfers(response.alkanes.0);

        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&total_out.to_le_bytes());
        response.data = data;
        Ok(response)
    }

    /// Read `(token_a, token_b, reserve_a, reserve_b, total_supply)`
    /// from pool's PoolDetails (opcode 999). Used only by `ZapOut` and
    /// `ZapInForExactLp`.
    fn query_pool_details_full(
        &self,
        pool: AlkaneId,
    ) -> Result<(AlkaneId, AlkaneId, u128, u128, u128)> {
        let resp = self.staticcall(
            &Cellpack { target: pool, inputs: vec![999] },
            &AlkaneTransferParcel::default(),
            self.fuel(),
        )?;
        if resp.data.len() < 112 {
            return Err(anyhow!("PoolDetails too short: {}", resp.data.len()));
        }
        let token_a = AlkaneId {
            block: u128::from_le_bytes(resp.data[0..16].try_into().unwrap()),
            tx: u128::from_le_bytes(resp.data[16..32].try_into().unwrap()),
        };
        let token_b = AlkaneId {
            block: u128::from_le_bytes(resp.data[32..48].try_into().unwrap()),
            tx: u128::from_le_bytes(resp.data[48..64].try_into().unwrap()),
        };
        let reserve_a = u128::from_le_bytes(resp.data[64..80].try_into().unwrap());
        let reserve_b = u128::from_le_bytes(resp.data[80..96].try_into().unwrap());
        let total_supply = u128::from_le_bytes(resp.data[96..112].try_into().unwrap());
        Ok((token_a, token_b, reserve_a, reserve_b, total_supply))
    }

    // ── ZapOutForExactOut (opcode 6) ─────────────────────────────────

    fn zap_out_for_exact_out(
        &self,
        pool: AlkaneId,
        output_token: AlkaneId,
        output_amount: u128,
        max_lp: u128,
        deadline: u128,
    ) -> Result<CallResponse> {
        self.check_deadline(deadline)?;
        if output_amount == 0 {
            return Err(anyhow!("output_amount must be > 0"));
        }
        if max_lp == 0 {
            return Err(anyhow!("max_lp must be > 0"));
        }

        // Read pool state once (used for both inverse formula AND zap_out execution).
        let (token_a, token_b, reserve_a, reserve_b, total_supply) =
            self.query_pool_details_full(pool)?;
        if output_token != token_a && output_token != token_b {
            return Err(anyhow!(
                "output_token {:?} is not in pool ({:?}, {:?})",
                output_token, token_a, token_b
            ));
        }
        let fee = self.query_fee_per_1000(pool);
        if total_supply == 0 {
            return Err(anyhow!("pool total_supply is zero"));
        }

        // Reserve of the OUTPUT side (the one we want back).
        let r_a = if output_token == token_a { reserve_a } else { reserve_b };

        // Inverse formula → minimum L for desired output_amount.
        let required_lp = amm_logic::calculate_lp_for_exact_out(
            output_amount,
            r_a,
            total_supply,
            fee,
        )?;

        if required_lp > max_lp {
            return Err(anyhow!(
                "required liquidity {} > max_lp {}",
                required_lp,
                max_lp
            ));
        }

        // Pass already-read state to the hot path. min_out = output_amount —
        // catches the edge case where rounding produced < target.
        self.execute_zap_out_with_state(
            pool, output_token, required_lp, output_amount,
            token_a, token_b, reserve_a, reserve_b, fee,
        )
    }

    fn forward(&self) -> Result<CallResponse> {
        let context = self.context()?;
        Ok(CallResponse::forward(&context.incoming_alkanes))
    }

    fn get_name(&self) -> Result<CallResponse> {
        let mut response = CallResponse::default();
        response.data = b"Zippo".to_vec();
        Ok(response)
    }

    fn made_in(&self) -> Result<CallResponse> {
        let mut response = CallResponse::default();
        response.data = b"Winnipeg".to_vec();
        Ok(response)
    }

}

declare_alkane! {
    impl AlkaneResponder for Zap {
        type Message = ZapMessage;
    }
}
