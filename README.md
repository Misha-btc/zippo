# zippo

Single-side zap + stake/bond router for oyl-amm pools on ALKANES.

Takes one token from the user, optimally splits it into swap+deposit, and
adds liquidity through oyl-amm in a single transaction. Forwarding LP into
FIRE staking/bonding is a separate transaction (ops 7/8) — the combined
zap+stake/bond opcodes were removed because a real FIRE hop pushes the
total past the 3.5M fuel budget (see Fuel cost).

## Opcodes

| # | Method | Purpose |
|---:|---|---|
| 1 | `ZapIn` | exact-in: spend X of token, get ≥Y LP |
| 2 | `ZapInForExactLp` | exact-out: want exactly Y LP, spend ≤X of token |
| 3 | `ZapOut` | exact-in: burn X LP, get ≥Y output |
| 6 | `ZapOutForExactOut` | exact-out: want exactly Y output, burn ≤X LP |
| 7 | `Stake` | LP already in hand → forward to staking with validation |
| 8 | `Bond` | LP already in hand → forward to bonding |
| 50 | `Forward` | no-op (deploy marker) |
| 99 | `GetName` | view → returns `"Zippo"` |
| 100 | `MadeIn` | view → returns `"Winnipeg"` |

Opcodes 4/5/9/10 (`ZapInAndStake`, `ZapInAndBond` and their exact-LP
variants) existed but were **removed**: measured against the real FIRE
stack they cost 3.6M–4.8M fuel (103–138% of the 3.5M budget) and could
never confirm on-chain. Use the 2-tx flow instead: `ZapIn` → `Stake` /
`Bond` (each fits with ample headroom).

Every mutating opcode takes a `deadline` (block height): `0` disables
the check, any other value reverts the call once `height > deadline`.

The exact-LP variant (op 2) mints **at least** `lp_out` — the
forward-checked inverse (see Math) can mint a hair more, and the
surplus is returned so no LP dust is orphaned.

## Math

### Forward zap (op 1)

Closed-form solution for the optimal swap split on a single-side deposit:

```
a · s² + Rx · (1000 + a) · s − 1000 · A · Rx = 0
```

where `a = 1000 − fee_per_1000`, `Rx` is the input-side reserve, and `A`
is `amount_in`.

Solved via the standard quadratic formula. After the swap, the remaining
amount and the received output are deposited proportionally to the new
reserves.

### Inverse zap (op 2)

Given a target LP `L`, find the minimum `amount_in`:

```
s    = L · 1000 · Rx / ((1000−fee) · ts)
A_in = s + L · (Rx + s) / ts
```

Both values are rounded up, then **forward-checked**: the closed form
inverts continuous math, but the pool executes three cascaded integer
floors (swap output, proportional deposit, LP mint), which can shave
1–2 LP off the target — an exact closed-form inverse does not exist.
The contract replicates the pool's exact floor cascade
(`predict_lp_for_amount_in`) and bumps `amount_in` until the predicted
mint is ≥ L, so the pool is guaranteed to mint ≥ L LP (typically L or
L+1; surplus over `lp_out` is returned as change).

### Inverse unzap (op 6)

Given a target output amount `T`, find the minimum `L`:

```
1000 · Ra · f² − (Ra · (2000−fee) + T · fee) · f + 1000 · T = 0
f = L / ts
```

Smaller root plus a safety margin (+3 wei) to absorb integer-rounding in
`pool.WithdrawAndBurn`.

## Lock duration whitelist

Opcode 7 accepts only exact values from `fire-constants`:

| `lock_duration` | Period | Multiplier |
|---:|---|---:|
| `0` | no lock | 1.0× |
| `1_050` | WEEK | 1.25× |
| `4_375` | MONTH | 1.5× |
| `13_125` | THREE_MONTHS | 2.0× |
| `26_250` | SIX_MONTHS | 2.5× |
| `52_500` | YEAR | 3.0× |

Any other value (even off-by-one like `52_501`) reverts before any zap
work is done.

## State guarantees

* **Stateless** — the contract has no storage. Each call is isolated.
* **Pass-through** — nothing accumulates in the contract. Math conservation
  is proven by tests (a ZapIn→ZapOut round-trip loses exactly 13 bps =
  two swap fees, not a wei more).
* **Atomic revert** — any `Err` returned from a handler reverts the entire
  protostone: incoming alkanes are refunded via the `:v0:v0` pointer, and
  pool state is untouched.
* **No stranded transfers** — anything the pool returns beyond the
  expected swap output / LP (e.g. deposit change) is swept into the
  caller's refund. A dropped transfer would strand tokens forever in a
  stateless contract.
* **Exact-LP never undershoots** — op 2's `amount_in` is
  forward-checked against the pool's integer floor cascade before any
  tokens move, so the mint always covers `lp_out` (the bare closed form
  could undershoot by 1–2 LP and revert at the final check).
* **Pair validation up front** — the caller's `(input_token,
  output_token)` must match the pool's pair exactly; a mismatch reverts
  before any tokens move instead of surfacing as an opaque pool error
  mid-zap.
* **Fee read is load-bearing** — a failed `GetTotalFee` is a hard error,
  never a silent default. Assuming a wrong fee would either donate the
  difference to the pool (real fee lower) or revert opaquely (higher).
* **Overflow-safe math** — discriminants and `L·1000·R`-scale products
  are computed in U512; `ruint` wraps silently on overflow in release
  builds, so the old U256 intermediates produced garbage splits for
  reserves above ~2^117. Regression-tested across the full u128 domain.
* **No duplicate pool reads** — each opcode reads the pool snapshot
  (PoolDetails 999 + GetTotalFee 20) exactly once; the
  `_with_state` hot paths reuse it for both the inverse formula and
  execution (see `ZapInForExactLp` / `ZapOutForExactOut`), saving ~175K
  fuel.

## Fuel cost

Measured against the 3,500,000-fuel regtest budget under current
mainnet fuel rules (post-V217 accounting + CHANGE1 tariffs), via
`view::simulate_parcel` in the in-process harness and cross-checked on
live regtest (`metashrew_view simulate`, within +2-3%). Ops 7/8 measured
on regtest against the real FIRE stack (staking epoch-clone + bonding
behind beacon/upgradeable proxies):

| Op | Cross-contract calls | Fuel (harness) | Fuel (regtest) | Budget |
|---|---:|---:|---:|---:|
| 1 ZapIn | 4 (PoolDetails + fee + swap + add_liq) | 2,188,571 | 2,252,544 | 64% |
| 2 ZapInForExactLp | 4 (PoolDetails + fee + swap + add_liq) | 2,532,059 | 2,414,035 | 72% |
| 3 ZapOut | 4 (PoolDetails + fee + withdraw + swap) | 1,969,906 | 2,021,983 | 58% |
| 6 ZapOutForExactOut | 4 | 2,146,000 | 2,189,590 | 63% |
| 7 Stake standalone | 2 (+ FIRE staking hop) | — | 1,443,721 | 41% |
| 8 Bond standalone | 2 (+ FIRE bonding hop) | — | 2,645,827 | 76% |

The removed combined ops measured on regtest against real FIRE:
ZapInAndStake 3,615,549 (103%), ZapInAndStakeForExactLp 3,624,536
(104%), ZapInAndBond 4,817,670 (138%), ZapInAndBondForExactLp
4,826,642 (138%) — all over budget (the on-chain attempt reverted with
"all fuel consumed by WebAssembly"). The FIRE staking hop costs ~1.4M
(position-NFT CREATECHILD + two beacon delegatecalls), the bonding hop
~2.6M (oracle + treasury + FIRE mint), which a single tx cannot absorb
on top of a ~2.2M zap. Hence the 2-tx flow.

Cross-contract calls account for ~80-90% of total fuel. Pure math is
under 10%. Op 2's forward-check costs ~155K per prediction pass (U512
sqrt in the split formula); states that trigger the correction loop add
one or two more passes (worst measured: 2,572,897 = 74%).

Earlier figures (op 1: 1,355,488; op 2: ~1,520,000) were
measured under pre-V217 fuel accounting, which silently skipped
host-side charges (extcall/storage/load) — the jump reflects the
accounting fix and CHANGE1 tariffs, not code regressions (the
PoolDetails consolidation itself costs only ~110K).

## Build

```bash
# CC must be a wasm32-capable clang (homebrew llvm); the prefix is
# /opt/homebrew on Apple-silicon and /usr/local on Intel Macs.
CC="$(brew --prefix llvm)/bin/clang" \
    cargo build --release --target wasm32-unknown-unknown
```

WASM artifact: `target/wasm32-unknown-unknown/release/zippo.wasm` (~244K).

## Tests

```bash
# Native unit tests for the math (fast)
cargo test --lib

# Integration tests (in-process WASM harness via fire/)
cd ../../  # into fire/
CC="/opt/homebrew/opt/llvm/bin/clang" \
    cargo test --target wasm32-unknown-unknown zap_test
```

Coverage:
* **40 unit tests** for the math (forward/inverse round-trip, edge
  cases, U512 overflow regressions at u128-scale reserves, exact-LP
  undershoot regression at a captured regtest pool state, deposit-cap
  ratio properties)
* **29 integration tests** via fire's `wasm-bindgen-test` harness
* **5 real regtest e2e** (via `alkanes-cli` + bitcoind + metashrew)

## Dependencies

* [`alkanes-runtime`](https://github.com/kungfuflex/alkanes-rs) — runtime / messaging
* [`alkanes-support`](https://github.com/kungfuflex/alkanes-rs) — types
* [`oyl-amm`](https://github.com/kungfuflex/alkanes-rs) (target pool) — opcodes 1, 2, 3, 20, 999
* [`fire-staking`](../../alkanes/fire-staking/) (target stake) — opcode 1
* [`fire-bonding`](../../alkanes/fire-bonding/) (target bond) — opcode 1

No state calls into `fire-token` or other parts of FIRE — the contract
treats the pool and the staking/bonding contracts as black boxes.
