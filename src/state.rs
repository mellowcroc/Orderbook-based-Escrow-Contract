use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use cosmwasm_std::{Addr, Coin, StdResult, Storage};
use cw_storage_plus::{Item, Map, U64Key};

use cw20::{Balance, Cw20CoinVerified};

#[derive(Serialize, Deserialize, Clone, PartialEq, JsonSchema, Debug, Default)]
pub struct GenericBalance {
    pub native: Vec<Coin>,
    pub cw20: Vec<Cw20CoinVerified>,
}

impl GenericBalance {
    pub fn add_tokens(&mut self, add: Balance) {
        match add {
            Balance::Native(balance) => {
                for token in balance.0 {
                    let index = self.native.iter().enumerate().find_map(|(i, exist)| {
                        if exist.denom == token.denom {
                            Some(i)
                        } else {
                            None
                        }
                    });
                    match index {
                        Some(idx) => self.native[idx].amount += token.amount,
                        None => self.native.push(token),
                    }
                }
            }
            Balance::Cw20(token) => {
                let index = self.cw20.iter().enumerate().find_map(|(i, exist)| {
                    if exist.address == token.address {
                        Some(i)
                    } else {
                        None
                    }
                });
                match index {
                    Some(idx) => self.cw20[idx].amount += token.amount,
                    None => self.cw20.push(token),
                }
            }
        };
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, JsonSchema, Debug)]
pub struct Order {
    pub maker_address: Addr,
    pub maker_token: GenericBalance,
    pub taker_token: GenericBalance,
    pub target_address: Option<String>,
    pub is_open: bool,
}

pub const ORDERS: Map<U64Key, Order> = Map::new("orders");
pub const ORDER_COUNT: Item<u64> = Item::new("order_count");

pub fn next_id(store: &mut dyn Storage) -> StdResult<u64> {
    let id: u64 = ORDER_COUNT.may_load(store)?.unwrap_or_default() + 1;
    ORDER_COUNT.save(store, &id)?;
    Ok(id)
}
