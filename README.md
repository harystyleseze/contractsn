```
  ███████╗ ██████╗ ██╗  ██╗
  ██╔════╝██╔═══██╗██║  ██║
  ███████╗██║   ██║███████║
  ╚════██║██║   ██║╚════██║
  ███████║╚██████╔╝     ██║
  ╚══════╝ ╚═════╝      ╚═╝
           · m a r k e t ·

   Perpetuals Exchange on Stellar / Soroban
```

---

SO4.market is a decentralised perpetuals and spot exchange built on [Stellar](https://stellar.org) using [Soroban](https://soroban.stellar.org) smart contracts (SDK 25, Rust).

The protocol implements an isolated-market LP model with two-step keeper execution, dynamic funding rates, borrowing fees, price impact curves, auto-deleveraging, and on-chain liquidations — all adapted faithfully to Soroban's execution environment.

---

## Architecture

```
ExchangeRouter  ──► DepositHandler    ──► (mint LP tokens, update pool)
                ──► WithdrawalHandler ──► (burn LP tokens, return collateral)
                ──► OrderHandler      ──► IncreasePositionUtils
                                      ──► DecreasePositionUtils
                                      ──► SwapUtils
                ──► LiquidationHandler
                ──► AdlHandler

All handlers read/write ──► DataStore  (universal key-value store)
All handlers price via  ──► Oracle     (keeper-fed min/max price pairs)
All handlers check      ──► RoleStore  (role-based access control)

MarketFactory  ──► deploy MarketToken (SEP-41 LP) + register in DataStore
Reader         ──► stateless views over DataStore (no writes)
```

### Contract Map

| Contract | Description |
|---|---|
| `data_store` | Universal typed key-value store. All protocol state lives here. |
| `role_store` | Role-based access control — CONTROLLER, MARKET_KEEPER, ORDER_KEEPER, etc. |
| `oracle` | Keeper-fed price store. Prices expire per ledger. Ed25519-verified. |
| `market_token` | SEP-41 LP token deployed per market by `market_factory`. |
| `market_factory` | Deterministically deploys `market_token` instances and registers markets. |
| `deposit_vault` | Holds long/short tokens between deposit creation and keeper execution. |
| `deposit_handler` | Two-step deposit lifecycle: create → (keeper) execute / cancel. |
| `withdrawal_vault` | Holds LP tokens between withdrawal creation and keeper execution. |
| `withdrawal_handler` | Two-step withdrawal lifecycle: create → (keeper) execute / cancel. |
| `order_vault` | Holds collateral for pending orders. |
| `order_handler` | Full order lifecycle with routing by `OrderType` to position/swap utils. |
| `liquidation_handler` | Force-closes under-collateralised positions (LIQUIDATION_KEEPER). |
| `adl_handler` | Auto-deleverages profitable positions when pool PnL exceeds threshold. |
| `fee_handler` | Claims accumulated protocol fees and user funding fee credits. |
| `referral_storage` | On-chain referral code registry with tier-based rebate/discount config. |
| `reader` | Read-only aggregate views: positions, markets, OI, funding, liquidation checks. |
| `exchange_router` | Single user entry point. Supports multicall for atomic multi-step actions. |

### Shared Libraries

| Crate | Description |
|---|---|
| `libs/types` | All shared structs: `MarketProps`, `PositionProps`, `OrderProps`, `PriceProps`, `PositionFees`, etc. |
| `libs/math` | `FLOAT_PRECISION` (10³⁰), `TOKEN_PRECISION` (10⁷), `mul_div_wide` (I256), `pow_factor`, `sqrt_fp`. |
| `libs/keys` | ~58 deterministic `sha256`-based key derivation functions. |
| `libs/market_utils` | Pool value, open interest, PnL, funding state, borrowing fees, pool/OI validation. |
| `libs/position_utils` | Per-position PnL, fee breakdown, funding settlement, leverage validation, liquidation check. |
| `libs/pricing_utils` | Swap and position price impact, execution price, impact pool management. |
| `libs/swap_utils` | Single-hop and multi-hop token swaps through market pools. |
| `libs/increase_position_utils` | Open or increase a long/short position (14-step flow). |
| `libs/decrease_position_utils` | Partial or full position close with PnL settlement (14-step flow). Accepts an optional `swap_path` — when non-empty, collateral output is swapped through the specified markets before reaching the receiver. |

---

## Key Financial Mechanics

### Price Precision
- All USD values use `FLOAT_PRECISION = 10^30`.
- All token amounts use `TOKEN_PRECISION = 10^7` (Stellar's 7-decimal standard).
- Wide multiplication via Soroban's `I256` host functions prevents overflow.

### LP Minting
```
mint_amount        = deposit_usd × TOKEN_PRECISION / market_token_price
market_token_price = pool_value / lp_supply  (1 USD on first deposit)
```

### Price Impact
```
initial_diff = |sideA_usd - sideB_usd|
next_diff    = |after_delta|
positive_impact (improves balance) → paid from impact pool, capped by pool balance
negative_impact (worsens balance)  → paid into impact pool
impact = factor × (diff ^ exponent)
```

### Funding Rate
```
funding_factor_per_second = funding_factor × (|long_oi - short_oi| / total_oi) ^ exponent
funding_amount_per_size  += factor_per_second × dt × index_token_price
```

### Borrowing Fee
```
cumulative_borrowing_factor += borrowing_factor × dt × (open_interest / pool_amount)
position_borrow_fee          = (cumulative_factor_now - factor_at_open) × size_in_tokens
```

### Liquidation
```
remaining   = collateral_usd - borrowing_fees - funding_fees + unrealised_pnl
liquidatable when: remaining < min_collateral_factor × position_size_usd
```

---

## Order Collateral Flow

> **Resolves issue #47 — Unify order collateral transfer model.**

The protocol follows a single, canonical two-step collateral path.
`exchange_router` is the only entry point that may touch a caller's tokens.
`order_handler` is a passive consumer that only reads from the vault.

### Chosen model: Router-push, Handler-snapshot

```
User
 │
 │  multicall([
 │    SendTokens { token, receiver: order_vault, amount },  ← Step 1
 │    CreateOrder { params },                               ← Step 2
 │  ])
 ▼
ExchangeRouter
 │
 ├─ Step 1: token::Client(token).transfer(caller → order_vault, amount)
 │
 └─ Step 2: OrderHandlerClient.create_order(caller, params)
               │
               └─ OrderVault.record_transfer_in(token)
                    │  snapshot = on_chain_balance − last_recorded
                    │  REVERT if snapshot ≤ 0  (ZeroCollateral)
                    └─ stores order with collateral_delta_amount = snapshot
```

**Why this model:**
- The router holds the user's token approval; keeping the pull there means
  handlers never need their own approval and cannot silently double-pull.
- `record_transfer_in` acts as a snapshot oracle: it always reflects exactly
  what arrived since the last order, so double-submission is impossible.
- Decrease / stop-loss / liquidation orders do **not** deposit collateral;
  `create_order` skips the vault snapshot for those order types.

**Balance invariant:** after every `create_order` or `transfer_out`, the vault's
DataStore recorded balance equals its actual on-chain SEP-41 balance.

---

## Canonical Storage Model

> **Resolves issue #2 — Decide the canonical storage model for requests and positions.**

All transient request types (Deposits, Withdrawals, Orders) and long-lived Positions live in **handler-local persistent storage** within their respective contracts rather than in the shared global `data_store`.

### Chosen Architecture: Local Persistent Storage

```
DepositHandler      ──► (Local persistent storage: DepositProps)
WithdrawalHandler   ──► (Local persistent storage: WithdrawalProps)
OrderHandler        ──► (Local persistent storage: OrderProps & PositionProps)
```

- **Deposits**: Persisted locally in `deposit_handler` using `LocalKey::Deposit(nonce)`.
- **Withdrawals**: Persisted locally in `withdrawal_handler` using `LocalKey::Withdrawal(nonce)`.
- **Orders**: Persisted locally in `order_handler` using `OrderStorageKey::Order(nonce)`.
- **Positions**: Persisted locally in `order_handler` using `PositionStorageKey::Position(key)`.

### Rationale

1. **Storage Rent (TTL) Isolation**: Soroban requires rent (TTL) for persistent storage. Distributing user-specific transient requests (deposits, withdrawals, orders) and positions to their local handler contracts isolates their TTL management. This prevents the shared `data_store` from becoming an eviction risk or billing bottleneck.
2. **Access Control & Encapsulation**: Storing positions and orders within `order_handler` ensures that only authorized logic in `order_handler` (e.g. `create_order`, `execute_order`) can mutate position state. If stored in a shared `data_store`, any contract with write access to the `data_store` could corrupt position records.
3. **CPU Instruction / Serialization Savings**: Centralized databases like `data_store` store values in generic key-value maps. Cross-contract struct serialization/deserialization into `data_store` introduces significant CPU instruction overhead. Storing structs locally allows direct, type-safe serialization within the contract's namespace.

---

## Multi-hop Swap Semantics

> **Resolves issue #57 — Audit multi-hop swap token movement semantics.**

### Physical token movement model

Tokens move **physically** between pool contracts on every hop.
No virtual accounting shortcut is used — each intermediate transfer is
a real SEP-41 `transfer` call on-chain.

For a two-hop path `A → B → C` via `[market_1, market_2]`:

```
                 order_vault
                      │  transfer_out(token_A → market_1)
                      ▼
          ┌─── market_1 (pool: A, B) ───┐
          │  pool_A += input_A          │
          │  pool_B -= output_B         │
          │  SEP-41 transfer:           │
          │  token_B → market_2  ───────┼────────────────────┐
          └─────────────────────────────┘                    ▼
                                             ┌─── market_2 (pool: B, C) ───┐
                                             │  pool_B += output_B         │
                                             │  pool_C -= output_C         │
                                             │  SEP-41 transfer:           │
                                             │  token_C → receiver  ───────┼──► User
                                             └─────────────────────────────┘
```

**Pool balance invariant (holds after every multi-hop execution):**

| pool          | token   | DataStore record             | on-chain balance |
|---------------|---------|------------------------------|-----------------|
| market_1      | token_A | + input_A                    | + input_A       |
| market_1      | token_B | − output_B                   | − output_B      |
| market_2      | token_B | + output_B                   | + output_B      |
| market_2      | token_C | − output_C                   | − output_C      |

### Duplicate market guard

A swap path with a repeated market address causes double-mutation of pool
state and corrupts price-impact accounting.  `swap_with_path` rejects any
path with duplicate market addresses before any state is touched.

---

## Prerequisites

### 1. Rust toolchain

```bash
# Install Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Add the wasm target (required for Soroban contract compilation)
rustup target add wasm32-unknown-unknown
```

### 2. Stellar CLI

```bash
# Install from crates.io with wasm optimiser enabled
cargo install --locked stellar-cli --features opt

# Verify
stellar --version
```

### 3. Docker (optional — required for local node)

[Install Docker Desktop](https://docs.docker.com/get-docker/) if you want to run a fully local Stellar node instead of using testnet.

---

## Keys & Identity

Every transaction on Stellar must be signed by a key pair. The CLI manages keys in a local keystore (`~/.config/stellar/identity/`).

### Generate a new key pair

```bash
# Generate and store a key named "alice" globally (persists across projects)
stellar keys generate --global alice --network testnet

# Print the public address for "alice"
stellar keys address alice

# Print the secret key (keep this safe — do not commit it)
stellar keys show alice
```

### Import an existing secret key

```bash
stellar keys add alice --secret-key
# You will be prompted to paste the secret key (starts with S...)
```

### List all stored keys

```bash
stellar keys ls
```

### Fund a key on testnet (Friendbot airdrop)

```bash
# Requests 10,000 XLM from the testnet Friendbot faucet
stellar keys fund alice --network testnet

# Verify the balance
stellar contract invoke \
  --id CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC \
  --source alice --network testnet \
  -- balance --id $(stellar keys address alice)
```

### Configure the network shorthand (optional)

```bash
# "testnet" is pre-configured. To add a custom RPC:
stellar network add localnet \
  --rpc-url http://localhost:8000/soroban/rpc \
  --network-passphrase "Standalone Network ; February 2017"
```

---

## Build

### Type-check only (fastest — no wasm output)

```bash
cargo check --workspace
```

### Lint with Clippy

```bash
cargo clippy --workspace -- -D warnings
```

### Build all contracts to wasm

```bash
stellar contract build
```

Output: `target/wasm32-unknown-unknown/release/<contract_name>.wasm`

> The `stellar contract build` command compiles every `cdylib` crate in the workspace automatically.

### Build a single contract

```bash
stellar contract build --package data-store
stellar contract build --package role-store
stellar contract build --package oracle
stellar contract build --package market-factory
stellar contract build --package deposit-handler
stellar contract build --package withdrawal-handler
stellar contract build --package order-handler
stellar contract build --package liquidation-handler
stellar contract build --package adl-handler
stellar contract build --package fee-handler
stellar contract build --package referral-storage
stellar contract build --package reader
stellar contract build --package exchange-router
```

### Optimised release build

```bash
# Adds wasm-opt shrinking pass — use this before uploading to mainnet
stellar contract build --release
```

### Inspect a compiled wasm

```bash
# Print all exported function names
stellar contract inspect \
  --wasm target/wasm32-unknown-unknown/release/order_handler.wasm
```

---

## Test

All tests run inside the Soroban sandbox (no network required). The SDK provides a full mock host environment with storage, auth, and events.

### Run the full test suite

```bash
cargo test --workspace
```

### Test a specific crate

```bash
# Shared libraries
cargo test -p gmx-math
cargo test -p gmx-keys
cargo test -p gmx-market-utils
cargo test -p gmx-position-utils
cargo test -p gmx-pricing-utils
cargo test -p gmx-swap-utils
cargo test -p gmx-decrease-position-utils

# Core contracts
cargo test -p data-store
cargo test -p role-store
cargo test -p oracle
cargo test -p fee-handler

# Handler contracts
cargo test -p deposit-handler
cargo test -p withdrawal-handler
cargo test -p order-handler
```

### Run a single test by name

```bash
cargo test -p gmx-market-utils apply_delta_to_pool_amount_works
cargo test -p oracle set_and_get_price
cargo test -p deposit-handler create_and_execute_deposit
```

### Show test output (disable output capture)

```bash
cargo test --workspace -- --nocapture
```

### Run tests with a filter pattern

```bash
# All tests whose name contains "deposit"
cargo test --workspace deposit
```

### Check test coverage (requires cargo-llvm-cov)

```bash
cargo install cargo-llvm-cov
cargo llvm-cov --workspace --open
```

---

## Local Node

For end-to-end integration testing without using public testnet.

### Start a local Stellar node

```bash
stellar network start local
```

This starts a Docker container with a local Stellar + Soroban node on `http://localhost:8000`.

### Generate and fund a local key

```bash
stellar keys generate --global dev --network local
stellar keys fund dev --network local
```

### Stop the local node

```bash
stellar network stop local
```

All state is ephemeral — restarting clears everything.

---

## Makefile

A `Makefile` is provided for the most common workflows.

| Target | Description |
|---|---|
| `make build` | Compile all contracts to wasm via `stellar contract build` |
| `make check` | Type-check without producing wasm (`cargo check`) |
| `make lint` | Run Clippy with warnings as errors |
| `make test` | Run the full Soroban sandbox test suite |
| `make deploy-all` | Deploy the full protocol graph to **testnet** (default) |
| `make deploy-contract` | Deploy one standalone contract Wasm for debugging |
| `make upgrade-contract` | Upload new Wasm and upgrade one existing deployed contract |
| `make upgrade-all` | Upgrade every deployed protocol contract listed in `UPGRADE_CONTRACTS` |
| `make deploy-mainnet` | Deploy the full protocol graph to **mainnet** |
| `make clean` | Remove `target/` and `.deployed/` |

### Deploy and upgrade

```bash
# Full testnet deployment (default network)
make deploy-all

# Testnet with a different key name
make deploy-all SOURCE=mykey

# Mainnet
make deploy-mainnet SOURCE=mykey

# Override both
make deploy-all NETWORK=mainnet SOURCE=mykey

# Deploy one standalone contract Wasm for debugging
make deploy-contract CONTRACT=reader NETWORK=testnet SOURCE=mykey

# Upgrade one deployed contract in-place
make upgrade-contract CONTRACT=deposit_handler NETWORK=testnet SOURCE=mykey

# Upgrade every deployed protocol contract listed in UPGRADE_CONTRACTS
make upgrade-all NETWORK=testnet SOURCE=mykey
```

`SOURCE` must match a key stored in your local Stellar keystore (see [Keys & Identity](#keys--identity) above).

The full deploy script (`scripts/deploy.sh`) handles the full sequence automatically:
builds → uploads wasm blobs → deploys each contract → calls `initialize` → grants `CONTROLLER` roles → prints a summary table → saves all addresses to `.deployed/<NETWORK>.env`.

If `.deployed/<NETWORK>.env` already exists, `make deploy-all` refuses to create
a second protocol graph and prints the appropriate upgrade commands. To
intentionally create a fresh deployment, use:

```bash
make deploy-force NETWORK=testnet SOURCE=mykey
```

`deploy-contract` is deliberately standalone: it deploys one Wasm and prints the
new contract address, but it does not update `.deployed/<NETWORK>.env` or wire
the contract into the current protocol deployment.

Upgrade commands require the deployed contract to already expose an admin-gated
`upgrade(env, new_wasm_hash)` function that calls
`env.deployer().update_current_contract_wasm(new_wasm_hash)`.

### Deployed address file

After a successful deploy, addresses are written to `.deployed/testnet.env` (or `mainnet.env`):

```bash
# Source the file to load all contract addresses into your shell
source .deployed/testnet.env
echo $EXCHANGE_ROUTER
```

---

## Deploy to Testnet (manual)

The steps below are the manual equivalent of `make deploy`, useful for debugging individual steps or partial re-deploys.

Contracts must be deployed in dependency order: stores first, then handlers that depend on them, then the router last. The sequence below captures the full stack.

### Step 1 — Build wasm blobs

```bash
stellar contract build
```

### Step 2 — Upload wasm blobs

Each `upload` uploads bytecode and returns a `WASM_HASH`. Record each one — the hash is stable as long as the code doesn't change, so you only need to re-upload after rebuilding.

```bash
ROLE_STORE_HASH=$(stellar contract upload \
  --wasm target/wasm32-unknown-unknown/release/role_store.wasm \
  --source alice --network testnet)

DATA_STORE_HASH=$(stellar contract upload \
  --wasm target/wasm32-unknown-unknown/release/data_store.wasm \
  --source alice --network testnet)

ORACLE_HASH=$(stellar contract upload \
  --wasm target/wasm32-unknown-unknown/release/oracle.wasm \
  --source alice --network testnet)

MARKET_TOKEN_HASH=$(stellar contract upload \
  --wasm target/wasm32-unknown-unknown/release/market_token.wasm \
  --source alice --network testnet)

MARKET_FACTORY_HASH=$(stellar contract upload \
  --wasm target/wasm32-unknown-unknown/release/market_factory.wasm \
  --source alice --network testnet)

DEPOSIT_VAULT_HASH=$(stellar contract upload \
  --wasm target/wasm32-unknown-unknown/release/deposit_vault.wasm \
  --source alice --network testnet)

DEPOSIT_HANDLER_HASH=$(stellar contract upload \
  --wasm target/wasm32-unknown-unknown/release/deposit_handler.wasm \
  --source alice --network testnet)

WITHDRAWAL_VAULT_HASH=$(stellar contract upload \
  --wasm target/wasm32-unknown-unknown/release/withdrawal_vault.wasm \
  --source alice --network testnet)

WITHDRAWAL_HANDLER_HASH=$(stellar contract upload \
  --wasm target/wasm32-unknown-unknown/release/withdrawal_handler.wasm \
  --source alice --network testnet)

ORDER_VAULT_HASH=$(stellar contract upload \
  --wasm target/wasm32-unknown-unknown/release/order_vault.wasm \
  --source alice --network testnet)

ORDER_HANDLER_HASH=$(stellar contract upload \
  --wasm target/wasm32-unknown-unknown/release/order_handler.wasm \
  --source alice --network testnet)

LIQUIDATION_HANDLER_HASH=$(stellar contract upload \
  --wasm target/wasm32-unknown-unknown/release/liquidation_handler.wasm \
  --source alice --network testnet)

ADL_HANDLER_HASH=$(stellar contract upload \
  --wasm target/wasm32-unknown-unknown/release/adl_handler.wasm \
  --source alice --network testnet)

FEE_HANDLER_HASH=$(stellar contract upload \
  --wasm target/wasm32-unknown-unknown/release/fee_handler.wasm \
  --source alice --network testnet)

REFERRAL_STORAGE_HASH=$(stellar contract upload \
  --wasm target/wasm32-unknown-unknown/release/referral_storage.wasm \
  --source alice --network testnet)

READER_HASH=$(stellar contract upload \
  --wasm target/wasm32-unknown-unknown/release/reader.wasm \
  --source alice --network testnet)

EXCHANGE_ROUTER_HASH=$(stellar contract upload \
  --wasm target/wasm32-unknown-unknown/release/exchange_router.wasm \
  --source alice --network testnet)
```

### Step 3 — Capture your admin address

```bash
ALICE=$(stellar keys address alice)
```

### Step 4 — Deploy core infrastructure

```bash
ROLE_STORE=$(stellar contract deploy \
  --wasm-hash $ROLE_STORE_HASH \
  --source alice --network testnet \
  -- initialize --admin $ALICE)

DATA_STORE=$(stellar contract deploy \
  --wasm-hash $DATA_STORE_HASH \
  --source alice --network testnet \
  -- initialize --role_store $ROLE_STORE)

ORACLE=$(stellar contract deploy \
  --wasm-hash $ORACLE_HASH \
  --source alice --network testnet \
  -- initialize --admin $ALICE --role_store $ROLE_STORE --data_store $DATA_STORE)
```

### Step 5 — Deploy market factory

```bash
MARKET_FACTORY=$(stellar contract deploy \
  --wasm-hash $MARKET_FACTORY_HASH \
  --source alice --network testnet \
  -- initialize \
     --admin $ALICE \
     --role_store $ROLE_STORE \
     --data_store $DATA_STORE \
     --market_token_wasm_hash $MARKET_TOKEN_HASH)
```

### Step 6 — Deploy vaults and handlers

```bash
DEPOSIT_VAULT=$(stellar contract deploy \
  --wasm-hash $DEPOSIT_VAULT_HASH \
  --source alice --network testnet \
  -- initialize --admin $ALICE --role_store $ROLE_STORE)

DEPOSIT_HANDLER=$(stellar contract deploy \
  --wasm-hash $DEPOSIT_HANDLER_HASH \
  --source alice --network testnet \
  -- initialize \
     --admin $ALICE \
     --role_store $ROLE_STORE \
     --data_store $DATA_STORE \
     --oracle $ORACLE \
     --deposit_vault $DEPOSIT_VAULT)

WITHDRAWAL_VAULT=$(stellar contract deploy \
  --wasm-hash $WITHDRAWAL_VAULT_HASH \
  --source alice --network testnet \
  -- initialize --admin $ALICE --role_store $ROLE_STORE)

WITHDRAWAL_HANDLER=$(stellar contract deploy \
  --wasm-hash $WITHDRAWAL_HANDLER_HASH \
  --source alice --network testnet \
  -- initialize \
     --admin $ALICE \
     --role_store $ROLE_STORE \
     --data_store $DATA_STORE \
     --oracle $ORACLE \
     --withdrawal_vault $WITHDRAWAL_VAULT)

ORDER_VAULT=$(stellar contract deploy \
  --wasm-hash $ORDER_VAULT_HASH \
  --source alice --network testnet \
  -- initialize --admin $ALICE --role_store $ROLE_STORE)

ORDER_HANDLER=$(stellar contract deploy \
  --wasm-hash $ORDER_HANDLER_HASH \
  --source alice --network testnet \
  -- initialize \
     --admin $ALICE \
     --role_store $ROLE_STORE \
     --data_store $DATA_STORE \
     --oracle $ORACLE \
     --order_vault $ORDER_VAULT)
```

### Step 7 — Deploy risk handlers and periphery

```bash
LIQUIDATION_HANDLER=$(stellar contract deploy \
  --wasm-hash $LIQUIDATION_HANDLER_HASH \
  --source alice --network testnet \
  -- initialize \
     --admin $ALICE \
     --role_store $ROLE_STORE \
     --data_store $DATA_STORE \
     --oracle $ORACLE \
     --order_handler $ORDER_HANDLER)

ADL_HANDLER=$(stellar contract deploy \
  --wasm-hash $ADL_HANDLER_HASH \
  --source alice --network testnet \
  -- initialize \
     --admin $ALICE \
     --role_store $ROLE_STORE \
     --data_store $DATA_STORE \
     --oracle $ORACLE \
     --order_handler $ORDER_HANDLER)

FEE_HANDLER=$(stellar contract deploy \
  --wasm-hash $FEE_HANDLER_HASH \
  --source alice --network testnet \
  -- initialize \
     --admin $ALICE \
     --role_store $ROLE_STORE \
     --data_store $DATA_STORE)

REFERRAL_STORAGE=$(stellar contract deploy \
  --wasm-hash $REFERRAL_STORAGE_HASH \
  --source alice --network testnet \
  -- initialize --admin $ALICE)

# Reader is stateless — no initialize call needed
READER=$(stellar contract deploy \
  --wasm-hash $READER_HASH \
  --source alice --network testnet)
```

### Step 8 — Deploy exchange router

```bash
EXCHANGE_ROUTER=$(stellar contract deploy \
  --wasm-hash $EXCHANGE_ROUTER_HASH \
  --source alice --network testnet \
  -- initialize \
     --admin $ALICE \
     --role_store $ROLE_STORE \
     --data_store $DATA_STORE \
     --deposit_handler $DEPOSIT_HANDLER \
     --withdrawal_handler $WITHDRAWAL_HANDLER \
     --order_handler $ORDER_HANDLER \
     --fee_handler $FEE_HANDLER)
```

### Step 9 — Grant CONTROLLER role to all handlers

Handlers need `CONTROLLER` to write to `data_store` and withdraw from market pools:

```bash
for CONTRACT in \
  $DEPOSIT_HANDLER \
  $WITHDRAWAL_HANDLER \
  $ORDER_HANDLER \
  $LIQUIDATION_HANDLER \
  $ADL_HANDLER \
  $FEE_HANDLER \
  $EXCHANGE_ROUTER
do
  stellar contract invoke \
    --id $ROLE_STORE \
    --source alice --network testnet \
    -- grant_role --account $CONTRACT --role CONTROLLER
done
```

### Step 10 — Grant keeper roles (optional, for a test keeper account)

```bash
# Generate a dedicated keeper key
stellar keys generate --global keeper --network testnet
stellar keys fund keeper --network testnet
KEEPER=$(stellar keys address keeper)

# Market keeper (price feeds, execute deposits/withdrawals)
stellar contract invoke --id $ROLE_STORE --source alice --network testnet \
  -- grant_role --account $KEEPER --role MARKET_KEEPER

# Order keeper (execute pending orders)
stellar contract invoke --id $ROLE_STORE --source alice --network testnet \
  -- grant_role --account $KEEPER --role ORDER_KEEPER

# Liquidation keeper
stellar contract invoke --id $ROLE_STORE --source alice --network testnet \
  -- grant_role --account $KEEPER --role LIQUIDATION_KEEPER

# ADL keeper
stellar contract invoke --id $ROLE_STORE --source alice --network testnet \
  -- grant_role --account $KEEPER --role ADL_KEEPER

# Fee keeper (sweep protocol fees)
stellar contract invoke --id $ROLE_STORE --source alice --network testnet \
  -- grant_role --account $KEEPER --role FEE_KEEPER
```

---

## Invoke Contracts (Examples)

### Create a market
```bash
stellar contract invoke --id $MARKET_FACTORY \
  --source alice --network testnet \
  -- create_market \
     --index_token <ETH_TOKEN_ADDRESS> \
     --long_token  <WETH_TOKEN_ADDRESS> \
     --short_token <USDC_TOKEN_ADDRESS>
```

### Read pool value
```bash
stellar contract invoke --id <READER_ADDRESS> \
  --source alice --network testnet \
  -- get_market_pool_value_info \
     --data_store $DATA_STORE \
     --oracle $ORACLE \
     --market_token <MARKET_TOKEN_ADDRESS> \
     --maximize false
```

### Open a long position via exchange router
```bash
stellar contract invoke --id $EXCHANGE_ROUTER \
  --source alice --network testnet \
  -- create_order \
     --market <MARKET_TOKEN_ADDRESS> \
     --receiver <ALICE_ADDRESS> \
     --initial_collateral_token <USDC_ADDRESS> \
     --size_delta_usd 1000000000000000000000000000000000 \
     --collateral_delta_amount 1000000000 \
     --trigger_price 0 \
     --acceptable_price 0 \
     --execution_fee 100000 \
     --min_output_amount 0 \
     --order_type MarketIncrease \
     --is_long true
```

---

## Project Structure

```
contracts/
├── Cargo.toml                    # workspace root
├── README.md                     # this file
│
├── contracts/
│   ├── data_store/               # universal KV store
│   ├── role_store/               # access control
│   ├── market_token/             # SEP-41 LP token
│   ├── market_factory/           # deterministic market deploy
│   ├── oracle/                   # keeper-fed prices (ed25519)
│   ├── deposit_vault/            # token custody for deposits
│   ├── deposit_handler/          # deposit lifecycle
│   ├── withdrawal_vault/         # LP custody for withdrawals
│   ├── withdrawal_handler/       # withdrawal lifecycle
│   ├── order_vault/              # collateral custody for orders
│   ├── order_handler/            # full order lifecycle
│   ├── liquidation_handler/      # force-close underwater positions
│   ├── adl_handler/              # auto-deleverage profitable positions
│   ├── fee_handler/              # fee distribution and claims
│   ├── referral_storage/         # referral codes and tier rebates
│   ├── reader/                   # stateless aggregate views
│   └── exchange_router/          # user entry point, multicall
│
└── libs/
    ├── types/                    # shared #[contracttype] structs
    ├── math/                     # precision constants and safe math
    ├── keys/                     # sha256 key derivation (~58 functions)
    ├── market_utils/             # pool, OI, funding, borrowing math
    ├── position_utils/           # per-position PnL, fees, validation
    ├── pricing_utils/            # price impact, execution price
    ├── swap_utils/               # single and multi-hop swaps
    ├── increase_position_utils/  # position open/increase logic
    └── decrease_position_utils/  # position close/decrease logic
```

---

## EVM → Soroban Reference

| Solidity / EVM | Soroban / Rust |
|---|---|
| `bytes32` | `BytesN<32>` |
| `keccak256(abi.encode(...))` | `env.crypto().sha256(bytes)` |
| `mapping(bytes32 => uint256)` | `env.storage().persistent().set(key, val)` |
| `uint256` | `u128` (or `U256` for overflow-sensitive paths) |
| `int256` | `i128` (or `I256`) |
| `address` | `Address` |
| `block.timestamp` | `env.ledger().timestamp()` |
| `ERC-20` | SEP-41 via `soroban_sdk::token::Client` |
| `CREATE2` | `env.deployer().with_address(deployer, salt).deploy_v2(wasm, args)` |
| `emit Event(...)` | `env.events().publish((Symbol,), data)` |
| `msg.sender` | passed as `Address` arg + `caller.require_auth()` |
| `onlyRole` modifier | `role_store.has_role(caller, role)` cross-contract call |
| `ReentrancyGuard` | not needed — Soroban execution is atomic per transaction |

---

## Implementation Status

| Phase | Description | Status |
|---|---|---|
| 1 | Foundation — data_store, role_store, types, math, keys | ✅ Complete |
| 2 | Market infrastructure — market_token, market_factory, market_utils | ✅ Complete |
| 3 | Oracle — keeper-fed prices, ed25519 verification | ✅ Complete |
| 4 | Liquidity — deposit and withdrawal vaults + handlers | ✅ Complete |
| 5 | Trading — order vault, position utils, order handler | ✅ Complete |
| 6 | Risk — liquidation handler, ADL handler | ✅ Complete |
| 7 | Periphery — fee handler, referral storage, reader | ✅ Complete |
| 8 | Router — exchange router with multicall | ✅ Complete |
| 9 | Test hardening — fee/PnL/output on partial+full close, frozen-order state machine, collateral-output swap path | ✅ Complete |

---

## Upgrade Policy

Every core contract exposes an `upgrade(env, new_wasm_hash)` function that authenticates the stored admin before calling `env.deployer().update_current_contract_wasm(new_wasm_hash)`. Upgrades must be deliberate, role-gated decisions.

| Contract | Upgradeable | Role required | Notes |
|---|---|---|---|
| `data_store` | ✅ Yes | admin | Stores all protocol state. Upgrade only with a verified migration plan. |
| `role_store` | ✅ Yes | admin | Controls all access gates. Upgrade with extreme caution. |
| `market_factory` | ✅ Yes | admin | Owns market deployment logic. Review new market token wasm hash. |
| `market_token` | ❌ Immutable | — | LP token. Once deployed per market it must never change. |
| `oracle` | ✅ Yes | admin | Pricing source. Validate new price feed logic before upgrade. |
| `deposit_handler` | ✅ Yes | admin | — |
| `withdrawal_handler` | ✅ Yes | admin | — |
| `order_handler` | ✅ Yes | admin | — |
| `liquidation_handler` | ✅ Yes | admin | — |
| `adl_handler` | ✅ Yes | admin | — |
| `fee_handler` | ✅ Yes | admin | — |
| `referral_storage` | ✅ Yes | admin | — |
| `reader` | ✅ Yes | admin | Read-only; upgrade risk is low. |
| `exchange_router` | ✅ Yes | admin | User entry point. Test multicall paths after every upgrade. |

**Upgrade workflow:**

```sh
# Single contract
make upgrade-contract CONTRACT=deposit_handler NETWORK=testnet SOURCE=deployer

# Dry-run (prints planned actions, submits nothing)
make upgrade-all DRY_RUN=1 NETWORK=testnet SOURCE=deployer

# All upgradeable contracts
make upgrade-all NETWORK=testnet SOURCE=deployer
```

`market_token` is listed in `IMMUTABLE_CONTRACTS` in `mx/upgrade.mk` and is skipped by `make upgrade-all`.

---

## Contract Responsibility Matrix

> **Resolves issue #4 — Create a contract responsibility matrix.**

This matrix maps every contract under `contracts/*` to its state ownership, initialization parameters, access controls, collaboration graph, events, and upgrade characteristics.

| Contract | State Keys Owned | Init Arguments | Access Control / Roles Checked | Collaborating Contracts | Emitted Events | Upgrade Policy |
|---|---|---|---|---|---|---|
| `role_store` | `Admin`, `Initialized`, `(account, role) -> bool` | `admin: Address` | `Admin` auth for modification. | None | `RoleGranted`, `RoleRevoked` | ✅ Upgradeable (Admin) |
| `data_store` | `Admin`, `RoleStore`, arbitrary KV pairs | `admin: Address`, `role_store: Address` | `CONTROLLER` role for state mutation. | `role_store` | None | ✅ Upgradeable (Admin) |
| `oracle` | `Admin`, `RoleStore`, `DataStore`, `Passphrase`, temporary prices | `admin`, `role_store`, `data_store`, `passphrase: Bytes` | `price_keeper` / `order_keeper` role to set prices. | `role_store`, `data_store` | `prices_set` | ✅ Upgradeable (Admin) |
| `market_factory` | `Admin`, `RoleStore`, `DataStore`, `MarketTokenWasmHash` | `admin`, `role_store`, `data_store` | `MARKET_KEEPER` role to create markets. | Deploys `market_token`; writes to `data_store`. | `wasm_set`, `mkt_new` | ✅ Upgradeable (Admin) |
| `market_token` | Token balances, allowances, metadata | `admin`, `role_store`, `decimal`, `name`, `symbol` | `CONTROLLER` role to mint/burn. | `role_store` | SEP-41 Transfer, Approval, Mint, Burn | ❌ Immutable |
| `deposit_vault` | `RoleStore`, `TokenBalance(Address)` | `admin`, `role_store` | `CONTROLLER` role for `transfer_out`. | SEP-41 tokens, `role_store` | None | ❌ Immutable |
| `withdrawal_vault` | `RoleStore`, `TokenBalance(Address)` | `admin`, `role_store` | `CONTROLLER` role for `transfer_out`. | `market_token`, `role_store` | None | ❌ Immutable |
| `order_vault` | `RoleStore`, `TokenBalance(Address)` | `admin`, `role_store` | `CONTROLLER` role for `transfer_out`. | SEP-41 tokens, `role_store` | None | ❌ Immutable |
| `deposit_handler` | `Admin`, `RoleStore`, `DataStore`, `Oracle`, `DepositVault`, `LocalKey::Deposit(nonce)` | `admin`, `role_store`, `data_store`, `oracle`, `deposit_vault` | `ORDER_KEEPER` role to execute/cancel. | `deposit_vault`, `market_token`, `data_store`, `oracle`, `role_store` | `dep_req`, `dep_exec`, `dep_fail` | ✅ Upgradeable (Admin) |
| `withdrawal_handler` | `Admin`, `RoleStore`, `DataStore`, `Oracle`, `WithdrawalVault`, `LocalKey::Withdrawal(nonce)` | `admin`, `role_store`, `data_store`, `oracle`, `withdrawal_vault` | `ORDER_KEEPER` role to execute/cancel. | `withdrawal_vault`, `market_token`, `data_store`, `oracle`, `role_store` | `wd_req`, `wd_exec`, `wd_fail` | ✅ Upgradeable (Admin) |
| `order_handler` | `Admin`, `RoleStore`, `DataStore`, `Oracle`, `OrderVault`, `OrderStorageKey::Order(nonce)`, `PositionStorageKey::Position(key)` | `admin`, `role_store`, `data_store`, `oracle`, `order_vault` | `ORDER_KEEPER` for orders. `LIQUIDATION_KEEPER`/`ADL_KEEPER`/`CONTROLLER` for positions. | `order_vault`, `data_store`, `oracle`, `role_store`, libs | `ord_req`, `ord_exec`, `ord_fail`, `pos_update` | ✅ Upgradeable (Admin) |
| `liquidation_handler` | `Admin`, `RoleStore`, `DataStore`, `Oracle`, `OrderHandler` | `admin`, `role_store`, `data_store`, `oracle`, `order_handler` | `LIQUIDATION_KEEPER` role to liquidate. | `order_handler`, `data_store`, `oracle`, `role_store` | `liq_req` | ✅ Upgradeable (Admin) |
| `adl_handler` | `Admin`, `RoleStore`, `DataStore`, `Oracle`, `OrderHandler` | `admin`, `role_store`, `data_store`, `oracle`, `order_handler` | `ADL_KEEPER` role to execute ADL. | `order_handler`, `data_store`, `oracle`, `role_store` | `adl_req` | ✅ Upgradeable (Admin) |
| `fee_handler` | `Admin`, `RoleStore`, `DataStore` | `admin`, `role_store`, `data_store` | `FEE_KEEPER` role to claim fees. | `data_store`, `role_store`, SEP-41 tokens | `fees_claimed` | ✅ Upgradeable (Admin) |
| `referral_storage` | `Admin`, `ReferralKey::CodeOwner`, `ReferralKey::TraderCode`, `ReferralKey::ReferrerTier`, `ReferralKey::TierConfig` | `admin: Address` | `Admin` auth for configuring tiers. | None | `CodeRegistered`, `TraderCodeSet` | ✅ Upgradeable (Admin) |
| `reader` | None (Stateless) | None | None (Public view-only) | `data_store`, `oracle` | None | ✅ Upgradeable (Admin) |
| `exchange_router` | `Admin`, `RoleStore`, `DataStore`, `DepositHandler`, `WithdrawalHandler`, `OrderHandler` | `admin`, `role_store`, `data_store`, `deposit_handler`, `withdrawal_handler`, `order_handler` | None (Public user entry point) | `deposit_handler`, `withdrawal_handler`, `order_handler`, SEP-41 tokens | None | ✅ Upgradeable (Admin) |

---

## Contributing

SO4.market is being built in the open. All nine implementation phases are complete — the full protocol logic is live in Rust/Soroban. See the issue tracker for integration tests, optimisation tasks, and frontend work.

See [CONTRIBUTING.md](CONTRIBUTING.md) for deployment, and upgrade workflow rules.

---

## License

MIT
