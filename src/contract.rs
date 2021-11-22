#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    from_binary, to_binary, Addr, BankMsg, Deps, DepsMut, Env, MessageInfo, Response, StdResult,
    SubMsg, Uint128, WasmMsg,
};
use cw0::NativeBalance;
use cw2::set_contract_version;
use cw20::{Balance, Cw20CoinVerified, Cw20ExecuteMsg, Cw20ReceiveMsg};

use crate::error::ContractError;
use crate::msg::{ExecuteMsg, InstantiateMsg, OpenOrderMsg, OrderResponse, ReceiveMsg};
use crate::state::{next_id, GenericBalance, Order, ORDERS};

// version info for migration info
const CONTRACT_NAME: &str = "crates.io:orderbook-escrow";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    _msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    // no setup
    Ok(Response::default())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::OpenOrder(msg) => {
            execute_open_order(deps, Balance::from(info.funds), &info.sender, msg)
        }
        ExecuteMsg::CloseOrder { order_id } => {
            execute_close_order(deps, Balance::from(info.funds), &info.sender, order_id)
        }
        ExecuteMsg::Receive(msg) => execute_receive(deps, info, msg),
    }
}

pub fn execute_receive(
    deps: DepsMut,
    info: MessageInfo,
    wrapper: Cw20ReceiveMsg,
) -> Result<Response, ContractError> {
    let msg: ReceiveMsg = from_binary(&wrapper.msg)?;
    let balance = Balance::Cw20(Cw20CoinVerified {
        address: info.sender,
        amount: wrapper.amount,
    });
    let api = deps.api;
    match msg {
        ReceiveMsg::OpenOrder(msg) => {
            execute_open_order(deps, balance, &api.addr_validate(&wrapper.sender)?, msg)
        }
        ReceiveMsg::CloseOrder { order_id } => execute_close_order(
            deps,
            balance,
            &api.addr_validate(&wrapper.sender)?,
            order_id,
        ),
    }
}

pub fn execute_open_order(
    deps: DepsMut,
    balance: Balance,
    sender: &Addr,
    message: OpenOrderMsg,
) -> Result<Response, ContractError> {
    if balance.is_empty() {
        return Err(ContractError::EmptyBalance {});
    }

    if message.taker_token.native.is_empty() && message.taker_token.cw20.is_empty() {
        return Err(ContractError::OrderInvalid(String::from(
            "At least one native/cw20 token should be specified as a taker.",
        )));
    } else if message.taker_token.native.is_empty() && message.taker_token.cw20.len() > 1 {
        return Err(ContractError::OrderInvalid(String::from(
            "Only one cw20 token can be specified as a taker.",
        )));
    } else if !message.taker_token.native.is_empty() && !message.taker_token.cw20.is_empty() {
        return Err(ContractError::OrderInvalid(String::from(
            "Only one native or cw20 token can be specified as a taker.",
        )));
    }

    let maker_order_balance = match balance {
        Balance::Native(balance) => {
            if !message.taker_token.native.is_empty() {
                return Err(ContractError::OrderInvalid(String::from(
                    "Maker and taker tokens cannot both be native tokens.",
                )));
            }
            GenericBalance {
                native: balance.0,
                cw20: vec![],
            }
        }
        Balance::Cw20(token) => {
            if !message.taker_token.cw20.is_empty()
                && message.taker_token.cw20[0].address == token.address
            {
                return Err(ContractError::OrderInvalid(String::from(
                    "Maker and taker tokens cannot be the same cw20 tokens.",
                )));
            }
            GenericBalance {
                native: vec![],
                cw20: vec![token],
            }
        }
    };

    let order = Order {
        maker_address: sender.clone(),
        maker_token: maker_order_balance,
        taker_token: message.taker_token,
        target_address: message.target_address,
        is_open: true,
    };

    let id = next_id(deps.storage)?;
    ORDERS.save(deps.storage, id.into(), &order)?;

    Ok(Response::new()
        .add_attribute("method", "open_order")
        .add_attribute("order_id", id.to_string()))
}

pub fn execute_close_order(
    deps: DepsMut,
    balance: Balance,
    taker_address: &Addr,
    order_id: u64,
) -> Result<Response, ContractError> {
    // TODO: Do we need to handle invalid order_id with a different ContractError?
    // find the Order from the id
    let mut order = ORDERS.load(deps.storage, order_id.into())?;
    if !order.is_open {
        return Err(ContractError::OrderClosed {});
    }

    // Reject if target address exists and is not equal to the order taker address
    match &order.target_address {
        Some(target_address) => {
            if taker_address.clone() != deps.api.addr_validate(target_address.as_str())? {
                return Err(ContractError::OrderReserved {});
            }
        }
        _ => {}
    };

    let taker_order_balance = match balance {
        Balance::Native(balance) => GenericBalance {
            native: balance.0,
            cw20: vec![],
        },
        Balance::Cw20(token) => GenericBalance {
            native: vec![],
            cw20: vec![token],
        },
    };

    if taker_order_balance != order.taker_token {
        return Err(ContractError::OrderUnmatched {});
    }

    order.is_open = false;
    ORDERS.save(deps.storage, order_id.into(), &order)?;

    let maker_messages = send_tokens(&order.maker_address, &taker_order_balance)?;
    let taker_messages = send_tokens(&taker_address, &order.maker_token)?;

    Ok(Response::new()
        .add_attribute("method", "close_order")
        .add_attribute("order_id", order_id.to_string())
        .add_submessages(maker_messages)
        .add_submessages(taker_messages))
}

fn send_tokens(to: &Addr, balance: &GenericBalance) -> StdResult<Vec<SubMsg>> {
    let native_balance = &balance.native;
    let mut msgs: Vec<SubMsg> = if native_balance.is_empty() {
        vec![]
    } else {
        vec![SubMsg::new(BankMsg::Send {
            to_address: to.into(),
            amount: native_balance.to_vec(),
        })]
    };

    let cw20_balance = &balance.cw20;
    let cw20_msgs: StdResult<Vec<_>> = cw20_balance
        .iter()
        .map(|c| {
            let msg = Cw20ExecuteMsg::Transfer {
                recipient: to.into(),
                amount: c.amount,
            };
            let exec = SubMsg::new(WasmMsg::Execute {
                contract_addr: c.address.to_string(),
                msg: to_binary(&msg)?,
                funds: vec![],
            });
            Ok(exec)
        })
        .collect();
    msgs.append(&mut cw20_msgs?);
    Ok(msgs)
}

fn query_order(deps: Deps, id: u64) -> StdResult<OrderResponse> {
    let order = ORDERS.load(deps.storage, id.into())?;
    Ok(OrderResponse {
        maker_address: order.maker_address,
        maker_token: order.maker_token,
        taker_token: order.taker_token,
        target_address: order.target_address,
        is_open: order.is_open,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmwasm_std::testing::{
        mock_dependencies, mock_env, mock_info, MockApi, MockQuerier, MockStorage,
    };
    use cosmwasm_std::{coins, CosmosMsg, Empty, OwnedDeps};

    #[test]
    fn order_native_to_cw20() {
        let mut deps = mock_dependencies(&[]);
        instantiate_contract(&mut deps);

        let cw20_token_contract = String::from("my-cw20-token");
        let cw20_token_amount = Uint128::new(12345);
        let cw20_tokens = create_cw20_tokens(&cw20_token_contract, cw20_token_amount);

        let msg = OpenOrderMsg {
            taker_token: cw20_tokens.clone(),
            target_address: None,
        };
        let maker = String::from("maker");
        let balance = coins(100, "native");
        let info = mock_info(&maker, &balance);
        let res = execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::OpenOrder(msg.clone()),
        )
        .unwrap();
        assert_eq!(0, res.messages.len());
        assert_eq!(("method", "open_order"), res.attributes[0]);
        assert_eq!(("order_id", "1"), res.attributes[1]);

        // Check that order is correctly opened
        let order = query_order(deps.as_ref(), 1).unwrap();
        assert_eq!(&maker, order.maker_address.as_str());
        assert_eq!(balance, order.maker_token.native);
        assert_eq!(cw20_tokens.cw20, order.taker_token.cw20);
        assert_eq!(None, order.target_address);
        assert_eq!(true, order.is_open);

        // Close the open order
        let taker = String::from("taker");
        let receive = Cw20ReceiveMsg {
            sender: taker.clone(),
            amount: cw20_token_amount,
            msg: to_binary(&ExecuteMsg::CloseOrder { order_id: 1 }).unwrap(),
        };
        let info = mock_info(&cw20_token_contract, &[]);
        let msg = ExecuteMsg::Receive(receive.clone());
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();
        assert_eq!(2, res.messages.len());
        assert_eq!(("method", "close_order"), res.attributes[0]);
        assert_eq!(("order_id", "1"), res.attributes[1]);
        let send_msg = Cw20ExecuteMsg::Transfer {
            recipient: maker,
            amount: cw20_token_amount,
        };
        assert_eq!(
            res.messages[0],
            SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: cw20_token_contract.clone(),
                msg: to_binary(&send_msg).unwrap(),
                funds: vec![]
            }))
        );
        assert_eq!(
            res.messages[1],
            SubMsg::new(BankMsg::Send {
                to_address: taker,
                amount: balance,
            })
        );

        // Check that order is closed
        let order = query_order(deps.as_ref(), 1).unwrap();
        assert_eq!(false, order.is_open);
    }

    #[test]
    fn order_cw20_to_native() {
        let mut deps = mock_dependencies(&[]);
        instantiate_contract(&mut deps);

        let mut native_tokens = GenericBalance::default();
        native_tokens.add_tokens(Balance::Native(NativeBalance(coins(100, "native"))));
        let msg = OpenOrderMsg {
            taker_token: native_tokens.clone(),
            target_address: None,
        };

        let cw20_token_contract = String::from("my-cw20-token");
        let cw20_token_amount = Uint128::new(12345);
        let maker = String::from("maker");
        let receive = Cw20ReceiveMsg {
            sender: maker.clone(),
            amount: cw20_token_amount,
            msg: to_binary(&ExecuteMsg::OpenOrder(msg)).unwrap(),
        };
        let info = mock_info(&cw20_token_contract, &[]);
        let msg = ExecuteMsg::Receive(receive.clone());
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();
        assert_eq!(0, res.messages.len());
        assert_eq!(("method", "open_order"), res.attributes[0]);
        assert_eq!(("order_id", "1"), res.attributes[1]);

        // Check that order is correctly opened
        let order = query_order(deps.as_ref(), 1).unwrap();
        assert_eq!(maker, order.maker_address.as_str());
        assert_eq!(
            create_cw20_tokens(&cw20_token_contract, cw20_token_amount).cw20,
            order.maker_token.cw20
        );
        assert_eq!(native_tokens.native, order.taker_token.native);
        assert_eq!(None, order.target_address);
        assert_eq!(true, order.is_open);

        // Close the open order
        let taker = String::from("taker");
        let balance = coins(100, "native");
        let info = mock_info(&taker, &balance);
        let res = execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::CloseOrder { order_id: 1 },
        )
        .unwrap();
        assert_eq!(2, res.messages.len());
        assert_eq!(("method", "close_order"), res.attributes[0]);
        assert_eq!(("order_id", "1"), res.attributes[1]);
        assert_eq!(
            res.messages[0],
            SubMsg::new(BankMsg::Send {
                to_address: maker,
                amount: balance,
            })
        );
        let send_msg = Cw20ExecuteMsg::Transfer {
            recipient: taker,
            amount: cw20_token_amount,
        };
        assert_eq!(
            res.messages[1],
            SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: cw20_token_contract.clone(),
                msg: to_binary(&send_msg).unwrap(),
                funds: vec![]
            }))
        );

        // Check that order is closed
        let order = query_order(deps.as_ref(), 1).unwrap();
        assert_eq!(false, order.is_open);
    }

    #[test]
    fn order_cw20_to_cw20() {
        let mut deps = mock_dependencies(&[]);
        instantiate_contract(&mut deps);

        let xyz_token_contract = String::from("xyz-token");
        let xyz_token_amount = Uint128::new(12345);
        let xyz_tokens = create_cw20_tokens(&xyz_token_contract, xyz_token_amount);
        let msg = OpenOrderMsg {
            taker_token: xyz_tokens.clone(),
            target_address: None,
        };

        let abc_token_contract = String::from("abc-token");
        let abc_token_amount = Uint128::new(12345);
        let maker = String::from("maker");
        let receive = Cw20ReceiveMsg {
            sender: maker.clone(),
            amount: abc_token_amount,
            msg: to_binary(&ExecuteMsg::OpenOrder(msg)).unwrap(),
        };
        let info = mock_info(&abc_token_contract, &[]);
        let msg = ExecuteMsg::Receive(receive.clone());
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();
        assert_eq!(0, res.messages.len());
        assert_eq!(("method", "open_order"), res.attributes[0]);
        assert_eq!(("order_id", "1"), res.attributes[1]);

        // Check that order is correctly opened
        let order = query_order(deps.as_ref(), 1).unwrap();
        assert_eq!(maker, order.maker_address.as_str());
        assert_eq!(
            create_cw20_tokens(&abc_token_contract, abc_token_amount).cw20,
            order.maker_token.cw20
        );
        assert_eq!(xyz_tokens.cw20, order.taker_token.cw20);
        assert_eq!(None, order.target_address);
        assert_eq!(true, order.is_open);

        // Close the open order
        let taker = String::from("taker");
        let receive = Cw20ReceiveMsg {
            sender: taker.clone(),
            amount: xyz_token_amount,
            msg: to_binary(&ExecuteMsg::CloseOrder { order_id: 1 }).unwrap(),
        };
        let info = mock_info(&xyz_token_contract, &[]);
        let res = execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::Receive(receive),
        )
        .unwrap();
        assert_eq!(2, res.messages.len());
        assert_eq!(("method", "close_order"), res.attributes[0]);
        assert_eq!(("order_id", "1"), res.attributes[1]);
        let send_msg_0 = Cw20ExecuteMsg::Transfer {
            recipient: maker,
            amount: xyz_token_amount,
        };
        assert_eq!(
            res.messages[0],
            SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: xyz_token_contract,
                msg: to_binary(&send_msg_0).unwrap(),
                funds: vec![]
            }))
        );
        let send_msg_1 = Cw20ExecuteMsg::Transfer {
            recipient: taker,
            amount: abc_token_amount,
        };
        assert_eq!(
            res.messages[1],
            SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: abc_token_contract,
                msg: to_binary(&send_msg_1).unwrap(),
                funds: vec![]
            }))
        );

        // Check that order is closed
        let order = query_order(deps.as_ref(), 1).unwrap();
        assert_eq!(false, order.is_open);
    }

    #[test]
    fn open_multiple_orders() {
        let mut deps = mock_dependencies(&[]);
        instantiate_contract(&mut deps);

        let cw20_token_contract = String::from("my-cw20-token");
        let mut cw20_token_amount = Uint128::new(12345);
        let cw20_tokens = create_cw20_tokens(&cw20_token_contract, cw20_token_amount);

        let msg = OpenOrderMsg {
            taker_token: cw20_tokens.clone(),
            target_address: None,
        };
        let maker = String::from("maker");
        let first_order_balance = coins(100, "native");
        let res = execute(
            deps.as_mut(),
            mock_env(),
            mock_info(&maker, &first_order_balance),
            ExecuteMsg::OpenOrder(msg.clone()),
        )
        .unwrap();
        assert_eq!(0, res.messages.len());
        assert_eq!(("method", "open_order"), res.attributes[0]);
        assert_eq!(("order_id", "1"), res.attributes[1]);

        let second_order_balance = coins(200, "native");
        let res = execute(
            deps.as_mut(),
            mock_env(),
            mock_info(&maker, &second_order_balance),
            ExecuteMsg::OpenOrder(msg.clone()),
        )
        .unwrap();
        assert_eq!(0, res.messages.len());
        assert_eq!(("method", "open_order"), res.attributes[0]);
        assert_eq!(("order_id", "2"), res.attributes[1]);

        let third_order_balance = coins(300, "native");
        let res = execute(
            deps.as_mut(),
            mock_env(),
            mock_info(&maker, &third_order_balance),
            ExecuteMsg::OpenOrder(msg.clone()),
        )
        .unwrap();
        assert_eq!(0, res.messages.len());
        assert_eq!(("method", "open_order"), res.attributes[0]);
        assert_eq!(("order_id", "3"), res.attributes[1]);

        // Check that orders are correctly opened
        let order = query_order(deps.as_ref(), 1).unwrap();
        assert_eq!(first_order_balance, order.maker_token.native);
        let order = query_order(deps.as_ref(), 2).unwrap();
        assert_eq!(second_order_balance, order.maker_token.native);
        let order = query_order(deps.as_ref(), 3).unwrap();
        assert_eq!(third_order_balance, order.maker_token.native);
    }

    #[test]
    fn close_order_with_invalid_target_address_fails() {
        let mut deps = mock_dependencies(&[]);
        instantiate_contract(&mut deps);

        let cw20_token_contract = String::from("my-cw20-token");
        let cw20_token_amount = Uint128::new(12345);
        let cw20_tokens = create_cw20_tokens(&cw20_token_contract, cw20_token_amount);

        let msg = OpenOrderMsg {
            taker_token: cw20_tokens.clone(),
            target_address: Some(String::from("target")),
        };
        let maker = String::from("maker");
        let balance = coins(100, "native");
        let info = mock_info(&maker, &balance);
        let res = execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::OpenOrder(msg.clone()),
        )
        .unwrap();
        assert_eq!(0, res.messages.len());
        assert_eq!(("method", "open_order"), res.attributes[0]);
        assert_eq!(("order_id", "1"), res.attributes[1]);

        // Close the open order
        let receive = Cw20ReceiveMsg {
            sender: String::from("taker"),
            amount: Uint128::new(12345),
            msg: to_binary(&ExecuteMsg::CloseOrder { order_id: 1 }).unwrap(),
        };
        let info = mock_info(&cw20_token_contract, &[]);
        let msg = ExecuteMsg::Receive(receive.clone());
        let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
        assert!(matches!(err, ContractError::OrderReserved {}));
    }

    #[test]
    fn close_order_with_valid_target_address_succeeds() {
        let mut deps = mock_dependencies(&[]);
        instantiate_contract(&mut deps);

        let cw20_token_contract = String::from("my-cw20-token");
        let cw20_token_amount = Uint128::new(12345);
        let cw20_tokens = create_cw20_tokens(&cw20_token_contract, cw20_token_amount);

        let msg = OpenOrderMsg {
            taker_token: cw20_tokens.clone(),
            target_address: Some(String::from("target")),
        };
        let maker = String::from("maker");
        let balance = coins(100, "native");
        let info = mock_info(&maker, &balance);
        let res = execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::OpenOrder(msg.clone()),
        )
        .unwrap();
        assert_eq!(0, res.messages.len());
        assert_eq!(("method", "open_order"), res.attributes[0]);
        assert_eq!(("order_id", "1"), res.attributes[1]);

        // Close the open order
        let receive = Cw20ReceiveMsg {
            sender: String::from("target"),
            amount: Uint128::new(12345),
            msg: to_binary(&ExecuteMsg::CloseOrder { order_id: 1 }).unwrap(),
        };
        let info = mock_info(&cw20_token_contract, &[]);
        let msg = ExecuteMsg::Receive(receive.clone());
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();
        assert_eq!(2, res.messages.len());
        assert_eq!(("method", "close_order"), res.attributes[0]);
        assert_eq!(("order_id", "1"), res.attributes[1]);
    }

    #[test]
    fn close_order_with_invalid_token_fails() {
        let mut deps = mock_dependencies(&[]);
        instantiate_contract(&mut deps);

        let cw20_token_contract = String::from("my-cw20-token");
        let cw20_token_amount = Uint128::new(12345);
        let cw20_tokens = create_cw20_tokens(&cw20_token_contract, cw20_token_amount);

        let msg = OpenOrderMsg {
            taker_token: cw20_tokens.clone(),
            target_address: None,
        };
        let maker = String::from("maker");
        let balance = coins(100, "native");
        let info = mock_info(&maker, &balance);
        let res = execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::OpenOrder(msg.clone()),
        )
        .unwrap();
        assert_eq!(0, res.messages.len());
        assert_eq!(("method", "open_order"), res.attributes[0]);
        assert_eq!(("order_id", "1"), res.attributes[1]);

        // Try to close the open order with a wrong token
        let wrong_token_contract = String::from("wrong-token");
        let wrong_token_amount = Uint128::new(12345);

        let receive = Cw20ReceiveMsg {
            sender: String::from("taker"),
            amount: wrong_token_amount,
            msg: to_binary(&ExecuteMsg::CloseOrder { order_id: 1 }).unwrap(),
        };
        let info = mock_info(&wrong_token_contract, &[]);
        let msg = ExecuteMsg::Receive(receive.clone());
        let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
        assert!(matches!(err, ContractError::OrderUnmatched {}));
    }

    #[test]
    fn open_order_with_same_cw20_token_fails() {
        let mut deps = mock_dependencies(&[]);
        instantiate_contract(&mut deps);

        let cw20_token_contract = String::from("my-cw20-token");
        let cw20_token_amount = Uint128::new(12345);
        let cw20_tokens = create_cw20_tokens(&cw20_token_contract, cw20_token_amount);

        let msg = OpenOrderMsg {
            taker_token: cw20_tokens.clone(),
            target_address: None,
        };
        let maker = String::from("maker");
        let receive = Cw20ReceiveMsg {
            sender: maker.clone(),
            amount: cw20_token_amount,
            msg: to_binary(&ExecuteMsg::OpenOrder(msg)).unwrap(),
        };
        let info = mock_info(&cw20_token_contract, &[]);
        let msg = ExecuteMsg::Receive(receive.clone());
        let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
        assert!(matches!(err, ContractError::OrderInvalid(msg)));
    }

    fn instantiate_contract(deps: &mut OwnedDeps<MockStorage, MockApi, MockQuerier, Empty>) {
        let msg = InstantiateMsg {};
        let info = mock_info("anyone", &[]);
        let res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();
        assert_eq!(0, res.messages.len());
    }

    fn create_cw20_tokens(contract_address: &String, amount: Uint128) -> GenericBalance {
        let mut tokens = GenericBalance::default();
        tokens.add_tokens(Balance::Cw20(Cw20CoinVerified {
            address: Addr::unchecked(contract_address),
            amount,
        }));
        tokens
    }
}
