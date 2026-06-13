//! zippo — single-side zap + stake/bond router for oyl-amm pools.
//!
//! Opcodes:
//!   1   ZapIn              — single-asset → LP (exact-in)
//!   2   ZapInForExactLp    — single-asset → exact LP target (exact-out)
//!   3   ZapOut             — LP → single-asset (exact-in)
//!   6   ZapOutForExactOut  — LP → exact token target (exact-out)
//!   7   Stake              — standalone forward LP into staking
//!   8   Bond               — standalone forward LP into bonding
//!   50  Forward            — no-op deploy marker
//!   99  GetName            — view: returns "Zippo"
//!   100 MadeIn             — view: returns "Winnipeg"
//!
//! Baseline ZapIn flow:
//!   1. Read pool state once (PoolDetails 999 + GetTotalFee 20) and
//!      validate that the caller's pair matches the pool's pair.
//!   2. Compute the optimal swap `s` via the quadratic formula.
//!   3. Swap `s` input → output directly on the pool (opcode 3).
//!   4. Call AddLiquidity (opcode 1) with the proportionally-capped deposit.
//!   5. Return the LP + dust + any other incoming tokens to the caller.
//!
//! Every mutating opcode takes a `deadline` (block height): `0` disables
//! the check, any other value reverts the call once `height > deadline`.

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

// ─── Message Dispatch ────────────────────────────────────────────────

#[derive(MessageDispatch)]
pub enum ZapMessage {
    /// Single-side zap → LP.
    /// `input_token` is what the caller sends in incoming.
    /// `output_token` is the other side of the pool (used for swap output).
    /// Both must match the pool's pair exactly, otherwise the call
    /// reverts up front.
    /// `amount_in` is the exact amount of `input_token` to zap. Must be
    /// `> 0` and `≤ total_incoming` (matching the `oyl-amm factory`
    /// convention). Excess `total_incoming − amount_in` is returned to
    /// the caller as change via the leftovers list (the same pattern as
    /// `_return_leftovers`).
    /// Any other tokens in incoming (including a stray `output_token`)
    /// are returned untouched.
    /// `deadline` is a block height; `0` disables the expiry check.
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
    /// forward-checked closed-form inverse (guaranteed to mint ≥
    /// `lp_out` despite the pool's integer floors), reverts if
    /// `amount_in > amount_in_max`, and otherwise executes the zap. The unused
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
/// proportional cap, and leftovers from incoming (plus anything the
/// pool returned that we didn't expect — swept so nothing strands in
/// this stateless contract).
struct ZapInResult {
    lp_received: u128,
    pool: AlkaneId,
    input_token: AlkaneId,
    output_token: AlkaneId,
    residual_input: u128,
    residual_output: u128,
    leftovers: Vec<AlkaneTransfer>,
}

/// Snapshot of pool state, read once per call (PoolDetails + fee).
/// `token_a` is the pool's `/alkane/0` side (canonical order), so
/// `reserve_a`/`reserve_b` line up with the swap opcode's
/// `amount_0_out`/`amount_1_out`.
struct PoolState {
    token_a: AlkaneId,
    token_b: AlkaneId,
    reserve_a: u128,
    reserve_b: u128,
    total_supply: u128,
    fee_per_1000: u128,
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

/// Assemble a `CallResponse` from a transfer list (duplicates collapsed)
/// and return data.
fn make_response(transfers: Vec<AlkaneTransfer>, data: Vec<u8>) -> CallResponse {
    CallResponse {
        alkanes: AlkaneTransferParcel(aggregate_transfers(transfers)),
        data,
    }
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

/// Check that `{input_token, output_token}` is exactly the pool's pair
/// and return `true` when `input_token` is the `token_a` (`/alkane/0`)
/// side. A mismatched pair reverts here, before any tokens move —
/// instead of surfacing as an opaque pool-side error mid-zap.
fn orient_pair(
    input_token: AlkaneId,
    output_token: AlkaneId,
    state: &PoolState,
) -> Result<bool> {
    if input_token == state.token_a && output_token == state.token_b {
        Ok(true)
    } else if input_token == state.token_b && output_token == state.token_a {
        Ok(false)
    } else {
        Err(anyhow!(
            "tokens ({:?}, {:?}) do not match pool pair ({:?}, {:?})",
            input_token, output_token, state.token_a, state.token_b
        ))
    }
}

impl Zap {
    // ── Pool queries ─────────────────────────────────────────────────

    /// Read the full pool snapshot: PoolDetails (opcode 999) for the
    /// pair, reserves, and total supply, plus GetTotalFee (opcode 20).
    /// Two staticcalls — every mutating opcode needs all of it, and
    /// PoolDetails subsumes the old separate GetReserves(97) /
    /// total-supply reads.
    fn read_pool_state(&self, pool: AlkaneId) -> Result<PoolState> {
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
        let fee_per_1000 = self.query_fee_per_1000(pool)?;
        Ok(PoolState {
            token_a,
            token_b,
            reserve_a,
            reserve_b,
            total_supply,
            fee_per_1000,
        })
    }

    /// GetTotalFee (opcode 20) → `fee_per_1000`. A failed or malformed
    /// read is a hard error: silently assuming a default fee would make
    /// the swap under-ask (donating the difference to the pool) whenever
    /// the real fee is lower, or revert opaquely whenever it's higher.
    fn query_fee_per_1000(&self, pool: AlkaneId) -> Result<u128> {
        let resp = self.staticcall(
            &Cellpack { target: pool, inputs: vec![20] },
            &AlkaneTransferParcel::default(),
            self.fuel(),
        )?;
        if resp.data.len() < 16 {
            return Err(anyhow!("GetTotalFee response too short: {}", resp.data.len()));
        }
        Ok(u128::from_le_bytes(resp.data[0..16].try_into().unwrap()))
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

    /// Forwards `lp_amount` LP into `staking` opcode 1
    /// (`Stake { lock_duration, amount }`). Returns the staking response
    /// (typically the position NFT).
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
    /// (`Bond { lp_to_bond, min_fire_out }`).
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

    // ── Helpers ──────────────────────────────────────────────────────

    /// `deadline == 0` disables the check; any other value reverts the
    /// call once the current block height exceeds it.
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
                leftovers.push(*tr);
            }
        }
        if total_in == 0 {
            return Err(anyhow!("input token {:?} not present in incoming", input_token));
        }
        if amount_in > total_in {
            return Err(anyhow!(
                "required input {} exceeds {} received of token {:?}",
                amount_in, total_in, input_token
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

    /// Package a zap-to-LP result for the caller: `transfers` carries
    /// the op-specific payload (the minted LP); residuals and leftovers
    /// are appended, duplicates collapsed. `data` is the minted LP
    /// amount (16 LE bytes).
    fn package_zap_response(
        r: ZapInResult,
        mut transfers: Vec<AlkaneTransfer>,
    ) -> CallResponse {
        if r.residual_input > 0 {
            transfers.push(AlkaneTransfer {
                id: r.input_token,
                value: r.residual_input,
            });
        }
        if r.residual_output > 0 {
            transfers.push(AlkaneTransfer {
                id: r.output_token,
                value: r.residual_output,
            });
        }
        transfers.extend(r.leftovers);

        make_response(transfers, r.lp_received.to_le_bytes().to_vec())
    }

    /// Derive the minimum `amount_in` for an exact-LP target (preamble
    /// of opcode 2): read pool state, validate the pair, run the
    /// forward-checked closed-form inverse, and enforce `amount_in_max`.
    fn derive_exact_lp_input(
        &self,
        pool: AlkaneId,
        input_token: AlkaneId,
        output_token: AlkaneId,
        lp_out: u128,
        amount_in_max: u128,
    ) -> Result<(u128, PoolState)> {
        if lp_out == 0 {
            return Err(anyhow!("lp_out must be > 0"));
        }
        if amount_in_max == 0 {
            return Err(anyhow!("amount_in_max must be > 0"));
        }
        let state = self.read_pool_state(pool)?;
        let input_is_a = orient_pair(input_token, output_token, &state)?;
        if state.reserve_a == 0 || state.reserve_b == 0 {
            return Err(anyhow!("pool not seeded — single-side zap impossible"));
        }
        if state.total_supply == 0 {
            return Err(anyhow!("pool total_supply is zero"));
        }
        let (r_in, r_out) = if input_is_a {
            (state.reserve_a, state.reserve_b)
        } else {
            (state.reserve_b, state.reserve_a)
        };
        let (amount_in, _) = amm_logic::calculate_amount_in_for_exact_lp(
            lp_out, r_in, r_out, state.total_supply, state.fee_per_1000,
        )?;
        if amount_in > amount_in_max {
            return Err(anyhow!(
                "required amount_in {} > amount_in_max {}",
                amount_in, amount_in_max
            ));
        }
        Ok((amount_in, state))
    }

    // ── Core zap-to-LP ───────────────────────────────────────────────

    /// Core zap-to-LP logic that does *not* build a `CallResponse`. It
    /// returns the components so callers can decide where the LP goes —
    /// hand it back to the user (as in `zap_in`) or forward it into a
    /// staking/bonding contract. Thin wrapper: reads pool state and
    /// delegates to `_with_state`.
    fn execute_zap_to_lp(
        &self,
        pool: AlkaneId,
        input_token: AlkaneId,
        output_token: AlkaneId,
        amount_in: u128,
        min_lp_tokens: u128,
    ) -> Result<ZapInResult> {
        let state = self.read_pool_state(pool)?;
        self.execute_zap_to_lp_with_state(
            pool, input_token, output_token, amount_in, min_lp_tokens, &state,
        )
    }

    /// "Hot path" without duplicate pool reads — used when the caller
    /// (`zap_in_for_exact_lp` and derivatives) has already read the
    /// pool state for its own math. Within a single runtime invocation
    /// pool state is immutable between our mutate ops, so reusing the
    /// snapshot is safe.
    ///
    /// Anything the pool returns beyond the expected swap output / LP
    /// (e.g. deposit change) is swept into leftovers — this contract is
    /// stateless, so a dropped transfer would strand tokens forever.
    fn execute_zap_to_lp_with_state(
        &self,
        pool: AlkaneId,
        input_token: AlkaneId,
        output_token: AlkaneId,
        amount_in: u128,
        min_lp_tokens: u128,
        state: &PoolState,
    ) -> Result<ZapInResult> {
        let input_is_a = orient_pair(input_token, output_token, state)?;
        if state.reserve_a == 0 || state.reserve_b == 0 {
            return Err(anyhow!("pool not seeded — single-side zap impossible"));
        }

        let (amount_in, mut leftovers) = self.extract_input(input_token, amount_in)?;

        let (reserve_in, reserve_out) = if input_is_a {
            (state.reserve_a, state.reserve_b)
        } else {
            (state.reserve_b, state.reserve_a)
        };

        // 1) Optimal swap split.
        let swap_amount = amm_logic::calculate_single_side_swap(
            amount_in, reserve_in, state.fee_per_1000,
        );
        if swap_amount == 0 {
            return Err(anyhow!("amount_in too small to compute optimal swap split"));
        }

        // 2) Swap input → output. amount_0_out / amount_1_out follow pool ordering.
        let expected_out = amm_logic::calculate_swap_out(
            swap_amount, reserve_in, reserve_out, state.fee_per_1000,
        )?;
        let (amt_0_out, amt_1_out) = if input_is_a {
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
            } else {
                leftovers.push(*tr); // unexpected refund — back to caller
            }
        }
        if got_output == 0 {
            return Err(anyhow!("pool swap returned 0 output (input too small or pool degenerate)"));
        }

        let have_input = amount_in - swap_amount;
        let have_output = got_output;
        let new_reserve_in = reserve_in.saturating_add(swap_amount);
        let new_reserve_out = reserve_out.saturating_sub(got_output);

        // 3) Cap to the exact ratio — otherwise AddLiquidity would eat
        // dust. Shared with `predict_lp_for_amount_in` so the exact-LP
        // forward check can never diverge from execution.
        let (deposit_input, deposit_output) = amm_logic::plan_deposit(
            have_input, have_output, new_reserve_in, new_reserve_out,
        );
        if deposit_input == 0 || deposit_output == 0 {
            return Err(anyhow!("computed deposit is zero on one side (amount_in too small for this pool)"));
        }

        // 4) AddLiquidity — pass amounts in pool order.
        let (amount_a_dep, amount_b_dep) = if input_is_a {
            (deposit_input, deposit_output)
        } else {
            (deposit_output, deposit_input)
        };
        let add_resp = self.call_add_liquidity(
            pool, state.token_a, amount_a_dep, state.token_b, amount_b_dep,
        )?;

        let mut lp_received = 0u128;
        for tr in &add_resp.alkanes.0 {
            if tr.id == pool {
                lp_received = lp_received.saturating_add(tr.value);
            } else {
                leftovers.push(*tr); // deposit change from the pool — back to caller
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

    // ── ZapIn (opcode 1) ─────────────────────────────────────────────

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
        let r = self.execute_zap_to_lp(
            pool, input_token, output_token, amount_in, min_lp_tokens,
        )?;
        let lp = vec![AlkaneTransfer { id: r.pool, value: r.lp_received }];
        Ok(Self::package_zap_response(r, lp))
    }

    // ── ZapInForExactLp (opcode 2) ───────────────────────────────────

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
        let (amount_in, state) = self.derive_exact_lp_input(
            pool, input_token, output_token, lp_out, amount_in_max,
        )?;
        // Reuse the already-read pool state (immutable until the swap
        // fires) instead of reading it a second time.
        let r = self.execute_zap_to_lp_with_state(
            pool, input_token, output_token, amount_in, lp_out, &state,
        )?;
        let lp = vec![AlkaneTransfer { id: r.pool, value: r.lp_received }];
        Ok(Self::package_zap_response(r, lp))
    }

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

        // Response: NFT (from staking) + leftovers; data = staked LP
        // amount followed by the staking contract's own response data.
        let mut transfers = stake_resp.alkanes.0;
        transfers.extend(leftovers);
        let mut data = Vec::with_capacity(16 + stake_resp.data.len());
        data.extend_from_slice(&lp_amount.to_le_bytes());
        data.extend_from_slice(&stake_resp.data);

        Ok(make_response(transfers, data))
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

        // Response: bond-NFT + immediate FIRE (if any) + leftovers;
        // data = bonded LP amount followed by the bonding response data.
        let mut transfers = bond_resp.alkanes.0;
        transfers.extend(leftovers);
        let mut data = Vec::with_capacity(16 + bond_resp.data.len());
        data.extend_from_slice(&lp_amount.to_le_bytes());
        data.extend_from_slice(&bond_resp.data);

        Ok(make_response(transfers, data))
    }

    // ── ZapOut (opcode 3) ────────────────────────────────────────────

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
        let state = self.read_pool_state(pool)?;
        if output_token != state.token_a && output_token != state.token_b {
            return Err(anyhow!(
                "output_token {:?} is not in pool ({:?}, {:?})",
                output_token, state.token_a, state.token_b
            ));
        }
        self.execute_zap_out_with_state(pool, output_token, liquidity, min_out, &state)
    }

    /// "Hot path" ZapOut without duplicate pool reads — used when the
    /// caller (e.g. `zap_out_for_exact_out`) has already read the pool
    /// state for its own math. Unexpected transfers from the pool are
    /// swept into the refund instead of being dropped.
    fn execute_zap_out_with_state(
        &self,
        pool: AlkaneId,
        output_token: AlkaneId,
        liquidity: u128,
        min_out: u128,
        state: &PoolState,
    ) -> Result<CallResponse> {
        // Extract LP. Pool IS its own LP token (alkane_id same).
        let (lp_amount, mut leftovers) = self.extract_input(pool, liquidity)?;

        // Step 1: WithdrawAndBurn → (amt_a, amt_b) proportional
        let withdraw_resp = self.call_withdraw_and_burn(pool, lp_amount)?;
        let mut amt_a = 0u128;
        let mut amt_b = 0u128;
        for tr in &withdraw_resp.alkanes.0 {
            if tr.id == state.token_a {
                amt_a = amt_a.saturating_add(tr.value);
            } else if tr.id == state.token_b {
                amt_b = amt_b.saturating_add(tr.value);
            } else {
                leftovers.push(*tr); // unexpected — back to caller
            }
        }
        if amt_a == 0 || amt_b == 0 {
            return Err(anyhow!("pool burn returned 0 on one side (liquidity too small)"));
        }

        // Step 2: swap "other" side fully → output_token.
        // Pool reserves AFTER withdraw.
        let new_r_a = state.reserve_a.saturating_sub(amt_a);
        let new_r_b = state.reserve_b.saturating_sub(amt_b);

        let (input_token, input_amt, own_side, r_in, r_out, output_is_a) =
            if output_token == state.token_a {
                (state.token_b, amt_b, amt_a, new_r_b, new_r_a, true)
            } else {
                (state.token_a, amt_a, amt_b, new_r_a, new_r_b, false)
            };

        let swap_received = if input_amt > 0 && r_in > 0 && r_out > 0 {
            let expected_out = amm_logic::calculate_swap_out(
                input_amt, r_in, r_out, state.fee_per_1000,
            )?;
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
                } else {
                    leftovers.push(*tr); // unexpected refund — back to caller
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

        let mut transfers = vec![AlkaneTransfer {
            id: output_token,
            value: total_out,
        }];
        transfers.extend(leftovers);

        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&total_out.to_le_bytes());

        Ok(make_response(transfers, data))
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
        let state = self.read_pool_state(pool)?;
        if output_token != state.token_a && output_token != state.token_b {
            return Err(anyhow!(
                "output_token {:?} is not in pool ({:?}, {:?})",
                output_token, state.token_a, state.token_b
            ));
        }
        if state.total_supply == 0 {
            return Err(anyhow!("pool total_supply is zero"));
        }

        // Reserve of the OUTPUT side (the one we want back).
        let r_a = if output_token == state.token_a {
            state.reserve_a
        } else {
            state.reserve_b
        };

        // Inverse formula → minimum L for desired output_amount.
        let required_lp = amm_logic::calculate_lp_for_exact_out(
            output_amount,
            r_a,
            state.total_supply,
            state.fee_per_1000,
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
            pool, output_token, required_lp, output_amount, &state,
        )
    }

    // ── Views / misc ─────────────────────────────────────────────────

    fn forward(&self) -> Result<CallResponse> {
        let context = self.context()?;
        Ok(CallResponse::forward(&context.incoming_alkanes))
    }

    fn get_name(&self) -> Result<CallResponse> {
        Ok(CallResponse {
            data: b"Zippo".to_vec(),
            ..Default::default()
        })
    }

    fn made_in(&self) -> Result<CallResponse> {
        Ok(CallResponse {
            data: b"Winnipeg".to_vec(),
            ..Default::default()
        })
    }
}

declare_alkane! {
    impl AlkaneResponder for Zap {
        type Message = ZapMessage;
    }
}
