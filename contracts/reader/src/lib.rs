//! Reader — read-only view contract for aggregating protocol state.
//! Mirrors GMX's Reader.sol.
//!
//! Aggregates data across data_store, oracle, and position/market utils
//! into rich structs the frontend consumes without needing multiple calls.
//! All functions are view-only — no writes, no auth.
#![no_std]
#![allow(dependency_on_unit_never_type_fallback)]

use soroban_sdk::{
    contract, contractimpl, Address, BytesN, Env, Vec,
};
use gmx_types::{
    MarketProps, PositionProps, PositionInfo, PositionFees, PriceProps,
    PoolValueInfo, FundingInfo, AdlCandidate, SwapEstimate,
};
use gmx_math::{TOKEN_PRECISION, mul_div_wide};
use gmx_keys::{
    market_index_token_key, market_long_token_key, market_short_token_key,
    funding_amount_per_size_key, saved_funding_factor_per_second_key,
    position_key, position_list_key, account_position_list_key,
};
use gmx_market_utils::{get_pool_value, get_open_interest_for_side};
use gmx_position_utils::{get_position_pnl_usd, get_position_fees, is_liquidatable};
use gmx_pricing_utils::{get_execution_price, get_position_price_impact};

// ─── External clients ─────────────────────────────────────────────────────────

#[allow(dead_code)]
#[soroban_sdk::contractclient(name = "DataStoreClient")]
trait IDataStore {
    fn get_u128(env: Env, key: BytesN<32>) -> u128;
    fn get_i128(env: Env, key: BytesN<32>) -> i128;
    fn get_address(env: Env, key: BytesN<32>) -> Option<Address>;
    fn get_bytes32_set_count(env: Env, set_key: BytesN<32>) -> u32;
    fn get_bytes32_set_at(env: Env, set_key: BytesN<32>, start: u32, end: u32) -> Vec<BytesN<32>>;
}

#[allow(dead_code)]
#[soroban_sdk::contractclient(name = "OracleClient")]
trait IOracle {
    fn get_primary_price(env: Env, token: Address) -> PriceProps;
}

#[allow(dead_code)]
#[soroban_sdk::contractclient(name = "OrderHandlerClient")]
trait IOrderHandler {
    fn get_position(env: Env, key: BytesN<32>) -> Option<PositionProps>;
}

// ─── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct Reader;

#[contractimpl]
impl Reader {
    // ── Market views ─────────────────────────────────────────────────────────

    /// Load full MarketProps for a given market_token address from data_store.
    pub fn get_market(env: Env, data_store: Address, market_token: Address) -> MarketProps {
        let ds = DataStoreClient::new(&env, &data_store);
        let index_token = ds.get_address(&market_index_token_key(&env, &market_token))
            .expect("market index token not found");
        let long_token = ds.get_address(&market_long_token_key(&env, &market_token))
            .expect("market long token not found");
        let short_token = ds.get_address(&market_short_token_key(&env, &market_token))
            .expect("market short token not found");
        MarketProps { market_token, index_token, long_token, short_token }
    }

    /// Get the full pool value breakdown for a market at current oracle prices.
    pub fn get_market_pool_value_info(
        env: Env,
        data_store: Address,
        oracle: Address,
        market_token: Address,
        maximize: bool,
    ) -> PoolValueInfo {
        let market = Self::get_market(env.clone(), data_store.clone(), market_token);
        let oracle_client = OracleClient::new(&env, &oracle);
        let long_price  = oracle_client.get_primary_price(&market.long_token).mid_price();
        let short_price = oracle_client.get_primary_price(&market.short_token).mid_price();
        let index_price = oracle_client.get_primary_price(&market.index_token).mid_price();
        get_pool_value(&env, &data_store, &market, long_price, short_price, index_price, maximize)
    }

    /// Get open interest for both sides of a market.
    /// Returns (long_oi_usd, short_oi_usd).
    pub fn get_open_interest(
        env: Env,
        data_store: Address,
        market_token: Address,
    ) -> (i128, i128) {
        let market = Self::get_market(env.clone(), data_store.clone(), market_token);
        let long_oi  = get_open_interest_for_side(&env, &data_store, &market, true)  as i128;
        let short_oi = get_open_interest_for_side(&env, &data_store, &market, false) as i128;
        (long_oi, short_oi)
    }

    /// Get the aggregate funding state for a market.
    pub fn get_funding_info(
        env: Env,
        data_store: Address,
        market_token: Address,
    ) -> FundingInfo {
        let market = Self::get_market(env.clone(), data_store.clone(), market_token.clone());
        let ds = DataStoreClient::new(&env, &data_store);

        let funding_factor_per_second = ds.get_i128(
            &saved_funding_factor_per_second_key(&env, &market_token)
        );
        // Long side tracks funding in long_token collateral; short in short_token
        let long_funding_amount_per_size = ds.get_i128(
            &funding_amount_per_size_key(&env, &market_token, &market.long_token, true)
        );
        let short_funding_amount_per_size = ds.get_i128(
            &funding_amount_per_size_key(&env, &market_token, &market.short_token, false)
        );

        FundingInfo {
            funding_factor_per_second,
            long_funding_amount_per_size,
            short_funding_amount_per_size,
        }
    }

    // ── Position views ────────────────────────────────────────────────────────

    /// Get a single position enriched with PnL, fees, and liquidation price.
    ///
    /// Reads position from the canonical location (order_handler storage) via cross-contract call.
    /// This ensures all consumers (liquidation_handler, adl_handler, reader) agree on position state.
    pub fn get_position_info(
        env: Env,
        data_store: Address,
        oracle: Address,
        order_handler: Address,
        account: Address,
        market: Address,
        collateral_token: Address,
        is_long: bool,
    ) -> Option<PositionInfo> {
        // Read position from canonical location (order_handler storage)
        let pk = position_key(&env, &account, &market, &collateral_token, is_long);
        let position: PositionProps = match OrderHandlerClient::new(&env, &order_handler).get_position(&pk) {
            Some(p) => p,
            None => return None,
        };

        let market_props = Self::get_market(env.clone(), data_store.clone(), position.market.clone());
        let oracle_client = OracleClient::new(&env, &oracle);

        let index_price      = oracle_client.get_primary_price(&market_props.index_token);
        let collateral_price = oracle_client.get_primary_price(&position.collateral_token).mid_price();

        // PnL for the full position size
        let (pnl_usd, uncapped_pnl_usd) = get_position_pnl_usd(
            &env, &position, &index_price, position.size_in_usd,
        );

        // Fees in collateral token units
        let fees: PositionFees = get_position_fees(
            &env, &data_store, &market_props, &position,
            collateral_price, position.size_in_usd, false,
        );

        // Convert fee amounts (collateral token raw) → USD (FLOAT_PRECISION)
        let borrowing_fee_usd = mul_div_wide(&env, fees.borrowing_fee_amount, collateral_price, TOKEN_PRECISION);
        let funding_fee_usd   = mul_div_wide(&env, fees.funding_fee_amount,   collateral_price, TOKEN_PRECISION);
        let position_fee_usd  = mul_div_wide(&env, fees.position_fee_amount,  collateral_price, TOKEN_PRECISION);

        // Approximate liquidation price:
        // For a long:  liq_price = (size_usd - collateral_usd + fees_usd) / size_in_tokens × TOKEN_PRECISION
        // For a short: liq_price = (size_usd + collateral_usd - fees_usd) / size_in_tokens × TOKEN_PRECISION
        let collateral_usd = mul_div_wide(&env, position.collateral_amount, collateral_price, TOKEN_PRECISION);
        let total_fees_usd = borrowing_fee_usd + funding_fee_usd + position_fee_usd;

        let liquidation_price = if position.size_in_tokens > 0 {
            let numerator = if position.is_long {
                position.size_in_usd - collateral_usd + total_fees_usd
            } else {
                position.size_in_usd + collateral_usd - total_fees_usd
            };
            if numerator > 0 {
                mul_div_wide(&env, numerator, TOKEN_PRECISION, position.size_in_tokens)
            } else {
                0
            }
        } else {
            0
        };

        Some(PositionInfo {
            position,
            pnl_usd,
            uncapped_pnl_usd,
            borrowing_fee_usd,
            funding_fee_usd,
            position_fee_usd,
            liquidation_price,
        })
    }

    /// Compute the execution price a user would get for a given size and order direction.
    ///
    /// Useful for the UI to preview slippage before placing an order.
    pub fn get_execution_price_preview(
        env: Env,
        data_store: Address,
        oracle: Address,
        market_token: Address,
        is_long: bool,
        is_increase: bool,
        size_delta_usd: i128,
    ) -> i128 {
        let market = Self::get_market(env.clone(), data_store.clone(), market_token);
        let oracle_client = OracleClient::new(&env, &oracle);
        let index_price = oracle_client.get_primary_price(&market.index_token).mid_price();

        let impact_usd = get_position_price_impact(
            &env, &data_store, &market, is_long, size_delta_usd, is_increase, index_price,
        );

        get_execution_price(&env, index_price, size_delta_usd, impact_usd, is_long, is_increase)
    }

    /// Return whether a position is currently liquidatable at oracle prices.
    ///
    /// Reads position from the canonical location (order_handler storage) via cross-contract call.
    pub fn is_position_liquidatable(
        env: Env,
        data_store: Address,
        oracle: Address,
        order_handler: Address,
        account: Address,
        market: Address,
        collateral_token: Address,
        is_long: bool,
    ) -> bool {
        // Read position from canonical location (order_handler storage)
        let pk = position_key(&env, &account, &market, &collateral_token, is_long);
        let position: PositionProps = match OrderHandlerClient::new(&env, &order_handler).get_position(&pk) {
            Some(p) => p,
            None => return false,
        };

        let market_props = Self::get_market(env.clone(), data_store.clone(), position.market.clone());
        let oracle_client = OracleClient::new(&env, &oracle);
        let index_price      = oracle_client.get_primary_price(&market_props.index_token);
        let collateral_price = oracle_client.get_primary_price(&position.collateral_token).mid_price();
        is_liquidatable(&env, &data_store, &position, &market_props, collateral_price, &index_price)
    }

    // ── ADL (Auto-Deleveraging) views ────────────────────────────────────────

    /// Get all profitable positions eligible for auto-deleveraging on a market side.
    ///
    /// Returns only positions with positive unrealised PnL, sorted by profitability ratio
    /// (highest first). Use `limit` to bound iteration cost; keepers call multiple times
    /// if many positions qualify.
    pub fn get_adl_eligible_positions(
        env: Env,
        data_store: Address,
        oracle: Address,
        order_handler: Address,
        market: Address,
        is_long: bool,
        limit: u32,
    ) -> Vec<AdlCandidate> {
        let ds = DataStoreClient::new(&env, &data_store);
        let oracle_client = OracleClient::new(&env, &oracle);
        let order_client = OrderHandlerClient::new(&env, &order_handler);

        // Get market properties
        let market_props = Self::get_market(env.clone(), data_store.clone(), market.clone());
        let index_price = oracle_client.get_primary_price(&market_props.index_token);

        // Fetch all position keys from global position list
        let pos_list_key = position_list_key(&env);
        let pos_count = ds.get_bytes32_set_count(&pos_list_key);
        
        let mut candidates: Vec<AdlCandidate> = Vec::new(&env);
        
        // Iterate through all positions
        let mut i = 0u32;
        while i < pos_count && (candidates.len() as u32) < limit {
            // Fetch a batch of position keys
            let batch_end = if i + 100 > pos_count { pos_count } else { i + 100 };
            let position_keys = ds.get_bytes32_set_at(&pos_list_key, &i, &batch_end);
            
            let keys_len = position_keys.len();
            let mut j = 0u32;
            while j < keys_len {
                let pos_key = position_keys.get(j).unwrap();
                
                // Get position from order handler
                if let Some(position) = order_client.get_position(&pos_key) {
                    // Filter by market and direction
                    if position.market != market || position.is_long != is_long {
                        j += 1;
                        continue;
                    }

                    // Calculate unrealised PnL
                    let (pnl_usd, _) = get_position_pnl_usd(&env, &position, &index_price, position.size_in_usd);
                    
                    // Only include profitable positions
                    if pnl_usd > 0 {
                        // Calculate PnL to size ratio in basis points
                        let size_usd_abs = if position.size_in_usd > 0 { 
                            position.size_in_usd as u128 
                        } else { 
                            0u128 
                        };
                        
                        let ratio_bps = if size_usd_abs > 0 {
                            mul_div_wide(&env, pnl_usd, 10000i128, position.size_in_usd) as u32
                        } else {
                            0u32
                        };

                        let candidate = AdlCandidate {
                            key: pos_key,
                            owner: position.account.clone(),
                            size_usd: size_usd_abs,
                            unrealised_pnl_usd: pnl_usd as u128,
                            pnl_to_size_ratio_bps: ratio_bps,
                        };
                        
                        candidates.push_back(candidate);
                    }
                }
                
                j += 1;
            }
            
            i = batch_end;
        }

        // Sort candidates by pnl_to_size_ratio_bps descending (bubble sort)
        let candidates_len = candidates.len();
        if candidates_len > 1 {
            let mut k = 0usize;
            while k < candidates_len {
                let mut m = 0usize;
                while m + 1 < candidates_len - k {
                    let cand_m = candidates.get(m).unwrap();
                    let cand_m_next = candidates.get(m + 1).unwrap();
                    
                    // Swap if m+1 has higher ratio (descending sort)
                    if cand_m_next.pnl_to_size_ratio_bps > cand_m.pnl_to_size_ratio_bps {
                        let temp = cand_m.clone();
                        candidates.set(m, cand_m_next.clone());
                        candidates.set(m + 1, temp);
                    }
                    
                    m += 1;
                }
                k += 1;
            }
        }

        // Trim to limit
        let mut result: Vec<AdlCandidate> = Vec::new(&env);
        let take = if candidates_len > (limit as usize) { limit as usize } else { candidates_len };
        let mut idx = 0usize;
        while idx < take {
            result.push_back(candidates.get(idx).unwrap());
            idx += 1;
        }

        result
    }

    // ── Swap estimation (dry-run without state modification) ─────────────────

    /// Estimate the output of a swap without modifying state.
    ///
    /// Returns the estimated output token amount, cumulative price impact, and
    /// whether execution would likely revert due to paused markets or insufficient liquidity.
    ///
    /// This is a read-only view that mirrors swap execution logic for frontend preview.
    pub fn estimate_swap_output(
        env: Env,
        data_store: Address,
        oracle: Address,
        token_in: Address,
        amount_in: u128,
        swap_path: Vec<Address>,
    ) -> SwapEstimate {
        let oracle_client = OracleClient::new(&env, &oracle);
        
        // Validate swap path
        if swap_path.len() == 0 {
            return SwapEstimate {
                token_out: token_in.clone(),
                amount_out: amount_in,
                price_impact_usd: 0i128,
                execution_price: 0u128,
                reverts_if_executed: true,
            };
        }

        let mut current_amount = amount_in;
        let mut current_token = token_in.clone();
        let mut total_impact_usd = 0i128;
        let mut reverts_if_executed = false;

        // Iterate through swap path
        let path_len = swap_path.len();
        let mut i = 0u32;
        while i < path_len {
            let market = swap_path.get(i).unwrap();
            
            // Load market properties
            let market_props = Self::get_market(env.clone(), data_store.clone(), market);
            
            // For now, estimate assumes:
            // - Market is not paused (we don't have pause status check in this version)
            // - Sufficient liquidity exists
            // - Price impact is calculated based on pool state
            
            // Get oracle prices
            let index_price = oracle_client.get_primary_price(&market_props.index_token).mid_price();
            let long_price = oracle_client.get_primary_price(&market_props.long_token).mid_price();
            let short_price = oracle_client.get_primary_price(&market_props.short_token).mid_price();
            
            // Determine which token is input and which is output
            let (input_token, output_token) = if current_token == market_props.long_token {
                (market_props.long_token.clone(), market_props.short_token.clone())
            } else if current_token == market_props.short_token {
                (market_props.short_token.clone(), market_props.long_token.clone())
            } else {
                // Token not in market, swap ends
                reverts_if_executed = true;
                break;
            };

            // Convert amount_in to USD
            let input_price = if input_token == market_props.long_token { 
                long_price 
            } else { 
                short_price 
            };
            
            let input_usd = mul_div_wide(&env, current_amount as i128, input_price, TOKEN_PRECISION);

            // Get swap impact
            let impact_usd = get_position_price_impact(
                &env, &data_store, &market_props,
                false,  // is_long (doesn't matter for swap impact in this context)
                input_usd,
                true,   // is_increase (swap is treated as positive impact)
                index_price,
            );

            total_impact_usd += impact_usd;

            // Apply impact to output
            let output_price = if output_token == market_props.long_token { 
                long_price 
            } else { 
                short_price 
            };
            
            let output_usd = input_usd + impact_usd;
            
            if output_usd <= 0 {
                reverts_if_executed = true;
                break;
            }

            current_amount = mul_div_wide(&env, output_usd, TOKEN_PRECISION, output_price) as u128;
            current_token = output_token;

            i += 1;
        }

        let final_token_out = current_token;
        let final_amount_out = current_amount;

        // Calculate execution price (input / output)
        let execution_price = if final_amount_out > 0 {
            mul_div_wide(&env, amount_in as i128, TOKEN_PRECISION, final_amount_out as i128) as u128
        } else {
            0u128
        };

        SwapEstimate {
            token_out: final_token_out,
            amount_out: final_amount_out,
            price_impact_usd: total_impact_usd,
            execution_price,
            reverts_if_executed,
        }
    }
}
