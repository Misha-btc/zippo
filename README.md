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

Every mutating opcode takes a `deadline` (block height): `0` disables
the check, any other value reverts the call once `height > deadline`.

The exact-LP variants (2, 9, 10) mint **at least** `lp_out` — ceil
rounding can mint a hair more, and the surplus is returned (op 2) or
forwarded along with the target amount (ops 9, 10) so no LP dust is
orphaned.

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

Opcodes 4, 7, and 9 accept only exact values from `fire-constants`:

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
live regtest (`metashrew_view simulate`, within +2-3%):

| Op | Cross-contract calls | Fuel (harness) | Fuel (regtest) | Budget |
|---|---:|---:|---:|---:|
| 1 ZapIn | 4 (PoolDetails + fee + swap + add_liq) | 2,189,152 | 2,253,125 | 64% |
| 2 ZapInForExactLp | 4 (PoolDetails + fee + swap + add_liq) | 2,211,284 | 2,258,223 | 65% |
| 3 ZapOut | 4 (PoolDetails + fee + withdraw + swap) | 1,970,479 | 2,022,556 | 58% |
| 6 ZapOutForExactOut | 4 | 2,146,572 | 2,190,162 | 63% |
| 4 ZapInAndStake | 5 (+ staking hop) | 2,350,309 * | — | 67% * |
| 5 ZapInAndBond | 5 (+ bonding hop) | 2,350,309 * | — | 67% * |
| 7 Stake standalone | 2 | 220,792 * | — | 6% * |
| 8 Bond standalone | 2 | 220,792 * | — | 6% * |

\* ops 4/5/7/8 measured against a mock proto-token stake/bond target; a
real FIRE staking hop (position-NFT CREATECHILD + beacon delegatecall)
costs more — re-measure against the full FIRE stack before treating the
~1.15M headroom as final.

Cross-contract calls account for ~80-90% of total fuel. Pure math is
under 10%. Earlier figures (op 1: 1,355,488; op 2: ~1,520,000) were
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

WASM artifact: `target/wasm32-unknown-unknown/release/zippo.wasm` (~241K).

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
* **35 unit tests** for the math (forward/inverse round-trip, edge
  cases, U512 overflow regressions at u128-scale reserves)
* **33 integration tests** via fire's `wasm-bindgen-test` harness
* **5 real regtest e2e** (via `alkanes-cli` + bitcoind + metashrew)

## Dependencies

* [`alkanes-runtime`](https://github.com/kungfuflex/alkanes-rs) — runtime / messaging
* [`alkanes-support`](https://github.com/kungfuflex/alkanes-rs) — types
* [`oyl-amm`](https://github.com/kungfuflex/alkanes-rs) (target pool) — opcodes 1, 2, 3, 20, 999
* [`fire-staking`](../../alkanes/fire-staking/) (target stake) — opcode 1
* [`fire-bonding`](../../alkanes/fire-bonding/) (target bond) — opcode 1

No state calls into `fire-token` or other parts of FIRE — the contract
treats the pool and the staking/bonding contracts as black boxes.
