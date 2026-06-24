//! Order handler — create, execute, cancel, update, and freeze orders.
//! Mirrors GMX's OrderHandler.sol.
//!
//! Supported order types (OrderType enum in gmx_types):
//!   MarketSwap, LimitSwap            → routed to swap_utils
//!   MarketIncrease, LimitIncrease    → routed to increase_position_utils
//!   MarketDecrease, LimitDecrease,
//!   StopLossDecrease, Liquidation    → routed to decrease_position_utils
//!
//! Two-step lifecycle (same as deposit/withdrawal):
//!   create_order  → pulls collateral into order_vault, stores OrderProps
//!   execute_order → keeper calls with fresh oracle prices, dispatches by type
//!   cancel_order  → refunds collateral from order_vault to account
//!   update_order  → modify trigger_price / acceptable_price / size before execution
//!   freeze_order  → mark order as frozen (keeper-side circuit breaker)
#![no_std]
#![allow(dependency_on_unit_never_type_fallback)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, BytesN, Env,
    symbol_short, panic_with_error,
};
use gmx_types::{MarketProps, OrderProps, OrderType, PriceProps, PositionProps};
pub use gmx_types::CreateOrderParams;
use gmx_keys::{
    roles,
    order_key, order_list_key, account_order_list_key,
    market_index_token_key, market_long_token_key, market_short_token_key,
    position_key, max_leverage_key, position_fee_factor_key,
    fee_tier_volume_threshold_key, fee_tier_position_fee_factor_key,
    trader_volume_key, trader_volume_window_start_key,
};
use gmx_math::{mul_div_wide, TOKEN_PRECISION, FLOAT_PRECISION};
use gmx_increase_position_utils::{IncreasePositionParams, increase_position};
use gmx_decrease_position_utils::{DecreasePositionParams, decrease_position};
use gmx_swap_utils::swap_with_path;

// ─── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
enum InstanceKey {
    Initialized,
    Admin,
    RoleStore,
    DataStore,
    Oracle,
    OrderVault,
}

// ─── Errors ───────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized    = 1,
    NotInitialized        = 2,
    Unauthorized          = 3,
    OrderNotFound         = 4,
    InvalidOrderType      = 5,
    UnsatisfiedTrigger    = 6,
    PriceTooHigh          = 7,
    PriceTooLow           = 8,
    OrderFrozen           = 9,
    MaxLeverageExceeded   = 10,
}

// ─── External contract clients ────────────────────────────────────────────────

#[allow(dead_code)]
#[soroban_sdk::contractclient(name = "RoleStoreClient")]
trait IRoleStore {
    fn has_role(env: Env, account: Address, role: BytesN<32>) -> bool;
}

#[allow(dead_code)]
#[soroban_sdk::contractclient(name = "DataStoreClient")]
trait IDataStore {
    fn get_u128(env: Env, key: BytesN<32>) -> u128;
    fn set_u128(env: Env, caller: Address, key: BytesN<32>, value: u128) -> u128;
    // Note: the generated client passes `value` by reference (&u128)
    fn apply_delta_to_u128(env: Env, caller: Address, key: BytesN<32>, delta: i128) -> u128;
    fn get_i128(env: Env, key: BytesN<32>) -> i128;
    fn increment_nonce(env: Env, caller: Address) -> u64;
    fn get_address(env: Env, key: BytesN<32>) -> Option<Address>;
    fn add_bytes32_to_set(env: Env, caller: Address, set_key: BytesN<32>, value: BytesN<32>);
    fn remove_bytes32_from_set(env: Env, caller: Address, set_key: BytesN<32>, value: BytesN<32>);
}

// ─── Position event structs (issue #205) ──────────────────────────────────────

#[contracttype]
pub struct PositionOpenedEvent {
    pub key: BytesN<32>,
    pub account: Address,
    pub market: Address,
    pub size_in_usd: i128,
    pub collateral_amount: i128,
    pub is_long: bool,
    pub avg_entry_price: i128,
}

#[contracttype]
pub struct PositionIncreasedEvent {
    pub key: BytesN<32>,
    pub account: Address,
    pub market: Address,
    pub delta_size_usd: i128,
    pub delta_collateral: i128,
    pub new_size_usd: i128,
    pub avg_entry_price: i128,
}

#[contracttype]
pub struct PositionDecreasedEvent {
    pub key: BytesN<32>,
    pub account: Address,
    pub market: Address,
    pub delta_size_usd: i128,
    pub pnl_usd: i128,
    pub execution_price: i128,
}

#[contracttype]
pub struct PositionClosedEvent {
    pub key: BytesN<32>,
    pub account: Address,
    pub market: Address,
    pub pnl_usd: i128,
    pub execution_price: i128,
}

#[contracttype]
pub struct PositionLiquidatedEvent {
    pub key: BytesN<32>,
    pub account: Address,
    pub market: Address,
    pub execution_price: i128,
    pub remaining_collateral: i128,
}

#[allow(dead_code)]
#[soroban_sdk::contractclient(name = "OracleClient")]
trait IOracle {
    fn get_primary_price(env: Env, token: Address) -> PriceProps;
}

#[allow(dead_code)]
#[soroban_sdk::contractclient(name = "OrderVaultClient")]
trait IOrderVault {
    fn record_transfer_in(env: Env, token: Address) -> i128;
    fn transfer_out(env: Env, caller: Address, token: Address, receiver: Address, amount: i128);
}

// ─── Position storage key (must match increase/decrease position utils) ───────

/// Positions are stored in this contract's persistent storage under this key.
/// The #[contracttype] XDR encoding must match the one in increase/decrease_position_utils.
#[contracttype]
pub enum PositionStorageKey {
    Position(BytesN<32>),
}

// ─── Order-frozen flag (stored alongside OrderProps) ──────────────────────────

#[contracttype]
pub enum OrderStorageKey {
    Order(BytesN<32>),
    OrderFrozen(BytesN<32>),
}

// ─── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct OrderHandler;

#[contractimpl]
impl OrderHandler {
    /// One-time setup.
    pub fn initialize(
        env: Env,
        admin: Address,
        role_store: Address,
        data_store: Address,
        oracle: Address,
        order_vault: Address,
    ) {
        admin.require_auth();
        if env.storage().instance().has(&InstanceKey::Initialized) {
            panic_with_error!(&env, Error::AlreadyInitialized);
        }
        env.storage().instance().set(&InstanceKey::Initialized, &true);
        env.storage().instance().set(&InstanceKey::Admin, &admin);
        env.storage().instance().set(&InstanceKey::RoleStore, &role_store);
        env.storage().instance().set(&InstanceKey::DataStore, &data_store);
        env.storage().instance().set(&InstanceKey::Oracle, &oracle);
        env.storage().instance().set(&InstanceKey::OrderVault, &order_vault);
    }

    /// Create a new order and pull collateral into the order vault.
    ///
    /// For increase/swap order types: caller must have already transferred
    /// collateral to the order_vault; we call record_transfer_in to snapshot it.
    /// Returns the order key.
    pub fn create_order(env: Env, caller: Address, params: CreateOrderParams) -> BytesN<32> {
        caller.require_auth();

        let data_store: Address = env.storage().instance().get(&InstanceKey::DataStore).unwrap();
        let order_vault: Address = env.storage().instance().get(&InstanceKey::OrderVault).unwrap();
        let handler = env.current_contract_address();
        let ds = DataStoreClient::new(&env, &data_store);

        // Record collateral arrival for increase/swap orders
        let is_increase_or_swap = matches!(
            params.order_type,
            OrderType::MarketIncrease | OrderType::LimitIncrease | OrderType::StopIncrease |
            OrderType::MarketSwap     | OrderType::LimitSwap
        );
        let collateral_delta_amount = if is_increase_or_swap {
            let received = OrderVaultClient::new(&env, &order_vault)
                .record_transfer_in(&params.initial_collateral_token);
            received.max(0)
        } else {
            params.collateral_delta_amount
        };

        // Generate unique key
        let nonce = ds.increment_nonce(&handler);
        let key = order_key(&env, nonce);

        let order = OrderProps {
            account:                  caller.clone(),
            receiver:                 params.receiver,
            market:                   params.market.clone(),
            initial_collateral_token: params.initial_collateral_token,
            swap_path:                params.swap_path,
            size_delta_usd:           params.size_delta_usd,
            collateral_delta_amount,
            trigger_price:            params.trigger_price,
            acceptable_price:         params.acceptable_price,
            execution_fee:            params.execution_fee,
            min_output_amount:        params.min_output_amount,
            order_type:               params.order_type,
            is_long:                  params.is_long,
            updated_at_time:          env.ledger().timestamp(),
        };

        env.storage().persistent().set(&OrderStorageKey::Order(key.clone()), &order);

        ds.add_bytes32_to_set(&handler, &order_list_key(&env), &key);
        ds.add_bytes32_to_set(&handler, &account_order_list_key(&env, &caller), &key);

        env.events().publish((symbol_short!("ord_crt"),), (key.clone(), caller, params.market));
        key
    }

    /// Execute a pending order (called by keeper).
    pub fn execute_order(env: Env, keeper: Address, key: BytesN<32>) {
        keeper.require_auth();
        require_order_keeper(&env, &keeper);

        let data_store: Address = env.storage().instance().get(&InstanceKey::DataStore).unwrap();
        let order_vault: Address = env.storage().instance().get(&InstanceKey::OrderVault).unwrap();
        let oracle: Address = env.storage().instance().get(&InstanceKey::Oracle).unwrap();
        let handler = env.current_contract_address();

        // Load order
        let order: OrderProps = env.storage().persistent()
            .get(&OrderStorageKey::Order(key.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::OrderNotFound));

        // Check frozen
        let is_frozen: bool = env.storage().persistent()
            .get(&OrderStorageKey::OrderFrozen(key.clone()))
            .unwrap_or(false);
        if is_frozen {
            panic_with_error!(&env, Error::OrderFrozen);
        }

        // Load market props
        let market = load_market_props(&env, &data_store, &order.market);

        // Fetch oracle prices
        let oracle_client = OracleClient::new(&env, &oracle);
        let index_price   = oracle_client.get_primary_price(&market.index_token);
        let collateral_price = oracle_client
            .get_primary_price(&order.initial_collateral_token)
            .mid_price();

        // Trigger price checks for non-market orders
        match order.order_type {
            OrderType::LimitIncrease if index_price.min > order.trigger_price => {
                panic_with_error!(&env, Error::UnsatisfiedTrigger);
            }
            OrderType::LimitDecrease if index_price.max < order.trigger_price => {
                panic_with_error!(&env, Error::UnsatisfiedTrigger);
            }
            OrderType::StopLossDecrease if index_price.min > order.trigger_price => {
                panic_with_error!(&env, Error::UnsatisfiedTrigger);
            }
            _ => {}
        }

        let ds = DataStoreClient::new(&env, &data_store);

        // ── Fee tier resolution (#204) ─────────────────────────────────────────
        // Determine the trader's effective position fee factor from their tier.
        // Tiers 0-4: tier 0 is default (no threshold), higher tiers require more volume.
        let effective_fee_factor = resolve_fee_tier(
            &env, &ds, &handler, &order.account, &order.market, order.size_delta_usd,
        );

        // Dispatch by order type
        match order.order_type {
            OrderType::MarketSwap | OrderType::LimitSwap => {
                // Transfer collateral from vault to first market in path
                let first_market = order.swap_path.get(0)
                    .unwrap_or_else(|| panic_with_error!(&env, Error::InvalidOrderType));
                OrderVaultClient::new(&env, &order_vault).transfer_out(
                    &handler,
                    &order.initial_collateral_token,
                    &first_market,
                    &order.collateral_delta_amount,
                );
                let (_token_out, amount_out) = swap_with_path(
                    &env, &data_store, &handler, &oracle,
                    &order.initial_collateral_token,
                    order.collateral_delta_amount,
                    &order.swap_path,
                    &order.receiver,
                );
                if amount_out < order.min_output_amount {
                    panic_with_error!(&env, Error::PriceTooLow);
                }
            }

            OrderType::MarketIncrease | OrderType::LimitIncrease | OrderType::StopIncrease => {
                // Transfer collateral from vault into the market pool
                OrderVaultClient::new(&env, &order_vault).transfer_out(
                    &handler,
                    &order.initial_collateral_token,
                    &market.market_token,
                    &order.collateral_delta_amount,
                );

                // Apply tier fee override if trader qualified for a discount (#204)
                let fee_key_pos = position_fee_factor_key(&env, &order.market, true);
                let fee_key_neg = position_fee_factor_key(&env, &order.market, false);
                let original_fee_pos = if effective_fee_factor > 0 {
                    let orig = ds.get_u128(&fee_key_pos);
                    ds.set_u128(&handler, &fee_key_pos, &effective_fee_factor);
                    orig
                } else { 0 };
                let original_fee_neg = if effective_fee_factor > 0 {
                    let orig = ds.get_u128(&fee_key_neg);
                    ds.set_u128(&handler, &fee_key_neg, &effective_fee_factor);
                    orig
                } else { 0 };

                // Capture pre-increase position state for event emission (#205)
                let pos_key = position_key(&env, &order.account, &order.market, &order.initial_collateral_token, order.is_long);
                let pre: Option<PositionProps> = env.storage().persistent()
                    .get(&PositionStorageKey::Position(pos_key.clone()));
                let (pre_size, pre_collateral) = pre
                    .as_ref()
                    .map(|p| (p.size_in_usd, p.collateral_amount))
                    .unwrap_or((0, 0));
                let is_new_position = pre_size == 0;

                let updated = increase_position(&env, &IncreasePositionParams {
                    data_store:        &data_store,
                    caller:            &handler,
                    account:           &order.account,
                    receiver:          &order.receiver,
                    market:            &market,
                    collateral_token:  &order.initial_collateral_token,
                    size_delta_usd:    order.size_delta_usd,
                    collateral_amount: order.collateral_delta_amount,
                    acceptable_price:  order.acceptable_price,
                    is_long:           order.is_long,
                    index_token_price: &index_price,
                    collateral_price,
                    current_time:      env.ledger().timestamp(),
                });

                // Restore original fee factors after execution (#204)
                if effective_fee_factor > 0 {
                    ds.set_u128(&handler, &fee_key_pos, &original_fee_pos);
                    ds.set_u128(&handler, &fee_key_neg, &original_fee_neg);
                }

                // Max leverage check (#206)
                // max_leverage stored as BPS: 5000 = 50x. 0 = uncapped.
                let max_leverage = ds.get_u128(&max_leverage_key(&env, &order.market));
                if max_leverage > 0 && updated.collateral_amount > 0 {
                    let collateral_usd = mul_div_wide(
                        &env, updated.collateral_amount, collateral_price, TOKEN_PRECISION,
                    );
                    if collateral_usd > 0 {
                        // effective_leverage_bps = size * 100 / collateral (both in FLOAT_PRECISION)
                        let effective_bps = (updated.size_in_usd as u128)
                            .saturating_mul(100)
                            / (collateral_usd as u128);
                        if effective_bps > max_leverage {
                            panic_with_error!(&env, Error::MaxLeverageExceeded);
                        }
                    }
                }

                // Compute avg entry price for events (#205)
                let avg_entry_price = if updated.size_in_tokens > 0 {
                    mul_div_wide(&env, updated.size_in_usd, TOKEN_PRECISION, updated.size_in_tokens)
                } else { 0 };

                if is_new_position {
                    env.events().publish(
                        (symbol_short!("pos_open"),),
                        PositionOpenedEvent {
                            key: pos_key,
                            account: order.account.clone(),
                            market: order.market.clone(),
                            size_in_usd: updated.size_in_usd,
                            collateral_amount: updated.collateral_amount,
                            is_long: order.is_long,
                            avg_entry_price,
                        },
                    );
                } else {
                    env.events().publish(
                        (symbol_short!("pos_inc"),),
                        PositionIncreasedEvent {
                            key: pos_key,
                            account: order.account.clone(),
                            market: order.market.clone(),
                            delta_size_usd: updated.size_in_usd - pre_size,
                            delta_collateral: updated.collateral_amount - pre_collateral,
                            new_size_usd: updated.size_in_usd,
                            avg_entry_price,
                        },
                    );
                }
            }

            OrderType::MarketDecrease | OrderType::LimitDecrease |
            OrderType::StopLossDecrease | OrderType::Liquidation => {
                // Capture position key for events (#205)
                let pos_key = position_key(&env, &order.account, &order.market, &order.initial_collateral_token, order.is_long);

                let result = decrease_position(&env, &DecreasePositionParams {
                    data_store:        &data_store,
                    caller:            &handler,
                    account:           &order.account,
                    receiver:          &order.receiver,
                    market:            &market,
                    collateral_token:  &order.initial_collateral_token,
                    size_delta_usd:    order.size_delta_usd,
                    acceptable_price:  order.acceptable_price,
                    is_long:           order.is_long,
                    index_token_price: &index_price,
                    collateral_price,
                    current_time:      env.ledger().timestamp(),
                });

                // Emit position events (#205)
                if result.is_fully_closed {
                    env.events().publish(
                        (symbol_short!("pos_cls"),),
                        PositionClosedEvent {
                            key: pos_key,
                            account: order.account.clone(),
                            market: order.market.clone(),
                            pnl_usd: result.pnl_usd,
                            execution_price: result.execution_price,
                        },
                    );
                } else {
                    env.events().publish(
                        (symbol_short!("pos_dec"),),
                        PositionDecreasedEvent {
                            key: pos_key,
                            account: order.account.clone(),
                            market: order.market.clone(),
                            delta_size_usd: order.size_delta_usd,
                            pnl_usd: result.pnl_usd,
                            execution_price: result.execution_price,
                        },
                    );
                }
            }
        }

        // Remove order
        remove_order(&env, &data_store, &handler, &key, &order.account);

        env.events().publish((symbol_short!("ord_exe"),), (key, order.account));
    }

    /// Cancel a pending order and refund collateral to the account.
    pub fn cancel_order(env: Env, caller: Address, key: BytesN<32>) {
        caller.require_auth();

        let data_store: Address = env.storage().instance().get(&InstanceKey::DataStore).unwrap();
        let order_vault: Address = env.storage().instance().get(&InstanceKey::OrderVault).unwrap();
        let role_store: Address  = env.storage().instance().get(&InstanceKey::RoleStore).unwrap();
        let handler = env.current_contract_address();

        let order: OrderProps = env.storage().persistent()
            .get(&OrderStorageKey::Order(key.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::OrderNotFound));

        let is_keeper = RoleStoreClient::new(&env, &role_store)
            .has_role(&caller, &roles::order_keeper(&env));
        if caller != order.account && !is_keeper {
            panic_with_error!(&env, Error::Unauthorized);
        }

        // Refund collateral for increase/swap order types
        let needs_refund = matches!(
            order.order_type,
            OrderType::MarketIncrease | OrderType::LimitIncrease | OrderType::StopIncrease |
            OrderType::MarketSwap     | OrderType::LimitSwap
        );
        if needs_refund && order.collateral_delta_amount > 0 {
            OrderVaultClient::new(&env, &order_vault).transfer_out(
                &handler,
                &order.initial_collateral_token,
                &order.account,
                &order.collateral_delta_amount,
            );
        }

        remove_order(&env, &data_store, &handler, &key, &order.account);

        env.events().publish((symbol_short!("ord_can"),), (key, order.account));
    }

    /// Update a pending order's trigger/acceptable price or size delta.
    pub fn update_order(
        env: Env,
        caller: Address,
        key: BytesN<32>,
        size_delta_usd: i128,
        acceptable_price: i128,
        trigger_price: i128,
        min_output_amount: i128,
    ) {
        caller.require_auth();

        let mut order: OrderProps = env.storage().persistent()
            .get(&OrderStorageKey::Order(key.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::OrderNotFound));

        if caller != order.account {
            panic_with_error!(&env, Error::Unauthorized);
        }

        order.size_delta_usd    = size_delta_usd;
        order.acceptable_price  = acceptable_price;
        order.trigger_price     = trigger_price;
        order.min_output_amount = min_output_amount;
        order.updated_at_time   = env.ledger().timestamp();

        env.storage().persistent().set(&OrderStorageKey::Order(key.clone()), &order);

        // Clear frozen flag if set (order is being updated = re-enabled)
        env.storage().persistent().remove(&OrderStorageKey::OrderFrozen(key.clone()));

        env.events().publish((symbol_short!("ord_upd"),), (key, caller));
    }

    /// Freeze an order that cannot currently be executed.
    pub fn freeze_order(env: Env, keeper: Address, key: BytesN<32>) {
        keeper.require_auth();
        require_order_keeper(&env, &keeper);

        let _order: OrderProps = env.storage().persistent()
            .get(&OrderStorageKey::Order(key.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::OrderNotFound));

        env.storage().persistent().set(&OrderStorageKey::OrderFrozen(key.clone()), &true);

        env.events().publish((symbol_short!("ord_frz"),), key);
    }

    /// Return a stored order by key, or None if not found.
    pub fn get_order(env: Env, key: BytesN<32>) -> Option<OrderProps> {
        env.storage().persistent().get(&OrderStorageKey::Order(key))
    }

    /// Return a stored position by its position_key (sha256 hash), or None.
    /// Used by liquidation_handler and adl_handler to check position health.
    pub fn get_position(env: Env, key: BytesN<32>) -> Option<PositionProps> {
        env.storage().persistent().get(&PositionStorageKey::Position(key))
    }

    /// Force-liquidate a position. Called by the liquidation_handler after role/health checks.
    ///
    /// Positions live in order_handler storage, so liquidation must run here.
    pub fn liquidate_position(
        env: Env,
        keeper: Address,  // must have LIQUIDATION_KEEPER role
        account: Address,
        market: Address,
        collateral_token: Address,
        is_long: bool,
    ) {
        keeper.require_auth();
        require_liquidation_keeper(&env, &keeper);

        let data_store: Address = env.storage().instance().get(&InstanceKey::DataStore).unwrap();
        let oracle: Address = env.storage().instance().get(&InstanceKey::Oracle).unwrap();
        let handler = env.current_contract_address();

        let market_props = load_market_props(&env, &data_store, &market);
        let oracle_client = OracleClient::new(&env, &oracle);
        let index_price      = oracle_client.get_primary_price(&market_props.index_token);
        let collateral_price = oracle_client.get_primary_price(&collateral_token).mid_price();

        // Load position to get size
        let pk = position_key(&env, &account, &market, &collateral_token, is_long);
        let position: PositionProps = env.storage().persistent()
            .get(&PositionStorageKey::Position(pk.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::OrderNotFound));

        // Validate liquidatability
        if !gmx_position_utils::is_liquidatable(
            &env, &data_store, &position, &market_props, collateral_price, &index_price,
        ) {
            panic_with_error!(&env, Error::InvalidOrderType);
        }

        let result = decrease_position(&env, &DecreasePositionParams {
            data_store:        &data_store,
            caller:            &handler,
            account:           &account,
            receiver:          &account,
            market:            &market_props,
            collateral_token:  &collateral_token,
            size_delta_usd:    position.size_in_usd,
            acceptable_price:  0,
            is_long,
            index_token_price: &index_price,
            collateral_price,
            current_time:      env.ledger().timestamp(),
        });

        env.events().publish(
            (symbol_short!("pos_liq"),),
            PositionLiquidatedEvent {
                key: pk,
                account,
                market,
                execution_price: result.execution_price,
                remaining_collateral: result.remaining_collateral,
            },
        );
    }

    /// Partially close a profitable position for ADL. Called by adl_handler after checks.
    pub fn execute_adl(
        env: Env,
        keeper: Address,  // must have ADL_KEEPER role
        account: Address,
        market: Address,
        collateral_token: Address,
        is_long: bool,
        size_delta_usd: i128,
    ) {
        keeper.require_auth();
        require_adl_keeper(&env, &keeper);

        let data_store: Address = env.storage().instance().get(&InstanceKey::DataStore).unwrap();
        let oracle: Address = env.storage().instance().get(&InstanceKey::Oracle).unwrap();
        let handler = env.current_contract_address();

        let market_props = load_market_props(&env, &data_store, &market);
        let oracle_client = OracleClient::new(&env, &oracle);
        let index_price      = oracle_client.get_primary_price(&market_props.index_token);
        let collateral_price = oracle_client.get_primary_price(&collateral_token).mid_price();

        let result = decrease_position(&env, &DecreasePositionParams {
            data_store:        &data_store,
            caller:            &handler,
            account:           &account,
            receiver:          &account,
            market:            &market_props,
            collateral_token:  &collateral_token,
            size_delta_usd,
            acceptable_price:  0,
            is_long,
            index_token_price: &index_price,
            collateral_price,
            current_time:      env.ledger().timestamp(),
        });

        env.events().publish(
            (symbol_short!("adl_exe"),),
            (account, market, size_delta_usd, result.pnl_usd),
        );
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn require_order_keeper(env: &Env, caller: &Address) {
    let role_store: Address = env.storage().instance().get(&InstanceKey::RoleStore).unwrap();
    if !RoleStoreClient::new(env, &role_store).has_role(caller, &roles::order_keeper(env)) {
        panic_with_error!(env, Error::Unauthorized);
    }
}

fn require_liquidation_keeper(env: &Env, caller: &Address) {
    let role_store: Address = env.storage().instance().get(&InstanceKey::RoleStore).unwrap();
    if !RoleStoreClient::new(env, &role_store).has_role(caller, &roles::liquidation_keeper(env)) {
        panic_with_error!(env, Error::Unauthorized);
    }
}

fn require_adl_keeper(env: &Env, caller: &Address) {
    let role_store: Address = env.storage().instance().get(&InstanceKey::RoleStore).unwrap();
    if !RoleStoreClient::new(env, &role_store).has_role(caller, &roles::adl_keeper(env)) {
        panic_with_error!(env, Error::Unauthorized);
    }
}

fn load_market_props(env: &Env, data_store: &Address, market_token: &Address) -> MarketProps {
    let ds = DataStoreClient::new(env, data_store);
    let index_token = ds.get_address(&market_index_token_key(env, market_token))
        .expect("market index token not found");
    let long_token = ds.get_address(&market_long_token_key(env, market_token))
        .expect("market long token not found");
    let short_token = ds.get_address(&market_short_token_key(env, market_token))
        .expect("market short token not found");
    MarketProps { 
        market_token: market_token.clone(), 
        index_token, 
        long_token, 
        short_token 
    }
}

fn remove_order(env: &Env, data_store: &Address, caller: &Address, key: &BytesN<32>, account: &Address) {
    env.storage().persistent().remove(&OrderStorageKey::Order(key.clone()));
    env.storage().persistent().remove(&OrderStorageKey::OrderFrozen(key.clone()));
    let ds = DataStoreClient::new(env, data_store);
    ds.remove_bytes32_from_set(caller, &order_list_key(env), key);
    ds.remove_bytes32_from_set(caller, &account_order_list_key(env, account), key);
}

/// Resolve the trader's tier-based position fee factor for a market (issue #204).
///
/// Checks the trader's rolling 30-day volume (in ledger units: 259_200 = 30d × 24h × 12/hr at 5s/ledger),
/// resets the window if expired, accumulates the order's size, then walks tiers 0-4 to find the
/// best (lowest) fee factor the trader qualifies for. Returns 0 when no tier override applies
/// (caller keeps the default fee stored in data_store).
fn resolve_fee_tier(
    env: &Env,
    ds: &DataStoreClient,
    caller: &Address,
    account: &Address,
    market: &Address,
    size_delta_usd: i128,
) -> u128 {
    // 30 days in ledgers at ~5 s/ledger: 30 × 24 × 60 × 60 / 5 = 518_400
    const WINDOW_LEDGERS: u32 = 518_400;

    let vol_key   = trader_volume_key(env, account, market);
    let start_key = trader_volume_window_start_key(env, account, market);

    let window_start = ds.get_u128(&start_key) as u32;
    let current_ledger: u32 = env.ledger().sequence();

    // Reset rolling window if expired
    let current_volume = if current_ledger.saturating_sub(window_start) > WINDOW_LEDGERS {
        let seq_as_u128 = current_ledger as u128;
        ds.set_u128(caller, &start_key, &seq_as_u128);
        let zero: u128 = 0;
        ds.set_u128(caller, &vol_key, &zero);
        0u128
    } else {
        ds.get_u128(&vol_key)
    };

    // Accumulate this order's volume
    let size_abs = if size_delta_usd < 0 { 0u128 } else { size_delta_usd as u128 };
    let new_volume = current_volume.saturating_add(size_abs);
    ds.set_u128(caller, &vol_key, &new_volume);

    // Walk tiers 0-4, pick the highest tier whose threshold ≤ trader volume
    // (tier 0 threshold is typically 0, i.e. everyone qualifies)
    let mut best_fee_factor: u128 = 0;
    for tier in 0u32..5 {
        let threshold = ds.get_u128(&fee_tier_volume_threshold_key(env, market, tier));
        if threshold == 0 && tier > 0 {
            // Undefined tier — stop scanning
            break;
        }
        if new_volume >= threshold {
            let ff = ds.get_u128(&fee_tier_position_fee_factor_key(env, market, tier));
            if ff > 0 {
                best_fee_factor = ff;
            }
        }
    }
    best_fee_factor
}
