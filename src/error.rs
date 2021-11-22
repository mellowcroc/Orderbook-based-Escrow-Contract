use cosmwasm_std::StdError;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ContractError {
    #[error("{0}")]
    Std(#[from] StdError),

    #[error("Unauthorized")]
    Unauthorized {},

    #[error("Send some coins to create an order")]
    EmptyBalance {},

    #[error("Order taker information is invalid")]
    OrderInvalid(String),

    #[error("Order is already closed")]
    OrderClosed {},

    #[error("Order is reserved for a specific address")]
    OrderReserved {},

    #[error("Order is not matched")]
    OrderUnmatched {},
    // Add any other custom errors you like here.
    // Look at https://docs.rs/thiserror/1.0.21/thiserror/ for details.
}
