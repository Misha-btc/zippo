# zippo

Single-side zap + stake/bond router for oyl-amm pools on ALKANES.

Takes one token from the user, optimally splits it into swap+deposit, adds
liquidity through oyl-amm, and (optionally) forwards LP into FIRE staking
or bonding — all atomically in a single transaction.

## Opcodes

| # | Method | Purpose |
|---:|---|---|
| 1 | `ZapIn` | exact-in: spend X of token, get ≥Y LP |
| 2 | `ZapInForExactLp` | exact-out: want exactly Y LP, spend ≤X of token |
| 3 | `ZapOut` | exact-in: burn X LP, get ≥Y output |
| 6 | `ZapOutForExactOut` | exact-out: want exactly Y output, burn ≤X LP |
| 4 | `ZapInAndStake` | zap → LP → FIRE staking (atomic) |
| 5 | `ZapInAndBond` | zap → LP → FIRE bonding (atomic) |
| 9 | `ZapInAndStakeForExactLp` | exact-LP target → stake atomically |
| 10 | `ZapInAndBondForExactLp` | exact-LP target → bond atomically |
| 7 | `Stake` | LP already in hand → forward to staking with validation |
| 8 | `Bond` | LP already in hand → forward to bonding |
| 50 | `Forward` | no-op (deploy marker) |
| 99 | `GetName` | view → returns `"Zippo"` |
| 100 | `MadeIn` | view → returns `"Winnipeg"` |

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

Both values are rounded up — the pool is guaranteed to mint ≥ L LP.

### Inverse unzap (op 6)

Given a target output amount `T`, find the minimum `L`:

```
1000 · Ra · f² − (Ra · (2000−fee) + T · fee) · f + 1000 · T = 0
f = L / ts
```

Smaller root plus a safety margin (+3 wei) to absorb integer-rounding in
`pool.WithdrawAndBurn`.

## Lock duration whitelist

Opcodes 4 and 7 accept only exact values from `fire-constants`:

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
* **No duplicate pool reads** — `execute_zap_to_lp_with_state` and
  `execute_zap_out_with_state` accept pre-read state from their caller
  (see `ZapInForExactLp` / `ZapOutForExactOut`), saving ~175K fuel.

## Fuel cost (measured on regtest)

| Op | Total | Cross-contract calls |
|---|---:|---:|
| 1 ZapIn | 1,355,488 | 4 (reserves + fee + swap + add_liq) |
| 2 ZapInForExactLp | ~1,520,000 | 5 (+ PoolDetails) |
| 3 ZapOut | ~1,400,000 | 4 (PoolDetails + fee + withdraw + swap) |
| 6 ZapOutForExactOut | ~1,450,000 | 4 |
| 4 ZapInAndStake | ~1,950,000 | 5 (+ staking hop) |
| 5 ZapInAndBond | ~1,950,000 | 5 (+ bonding hop) |

Cross-contract calls account for ~80-90% of total fuel. Pure math is
under 10%.

## Build

```bash
CC="/opt/homebrew/opt/llvm/bin/clang" \
    cargo build --release --target wasm32-unknown-unknown
```

WASM artifact: `target/wasm32-unknown-unknown/release/zippo.wasm` (~234K).

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
* **29 unit tests** for the math (forward/inverse round-trip, edge cases)
* **24 integration tests** via fire's `wasm-bindgen-test` harness
* **5 real regtest e2e** (via `alkanes-cli` + bitcoind + metashrew)

## Dependencies

* [`alkanes-runtime`](https://github.com/kungfuflex/alkanes-rs) — runtime / messaging
* [`alkanes-support`](https://github.com/kungfuflex/alkanes-rs) — types
* [`oyl-amm`](https://github.com/kungfuflex/alkanes-rs) (target pool) — opcodes 1, 2, 3, 20, 97, 999
* [`fire-staking`](../../alkanes/fire-staking/) (target stake) — opcode 1
* [`fire-bonding`](../../alkanes/fire-bonding/) (target bond) — opcode 1

No state calls into `fire-token` or other parts of FIRE — the contract
treats the pool and the staking/bonding contracts as black boxes.
