use serde::{Deserialize, Serialize};
use hyperlane_core::{HyperlaneDomain, HyperlaneMessage, U256};

#[derive(Debug, Deserialize, Serialize)]
pub struct ValidityRequest {
    pub domain: HyperlaneDomain,
    pub message: HyperlaneMessage,
}


#[derive(Debug, Deserialize, Serialize)]
pub struct ValidityResponse {
    pub message: HyperlaneMessage,
    pub metadata: Vec<u8>,
    pub gas_limit: U256,
    pub error: Option<String>
}

pub fn new_validity_response_error(error_msg: String) -> ValidityResponse {
    return ValidityResponse {
        message: HyperlaneMessage{
            version: 0,
            nonce: 0,
            origin: 0,
            sender: Default::default(),
            destination: 0,
            recipient: Default::default(),
            body: vec![],
        },
        metadata: Vec::new(),
        gas_limit: U256::from(0),
        error: Some(error_msg.parse().unwrap())
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ValidityBatchRequest {
    pub requests: Vec<ValidityRequest>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ValidityBatchResponse {
    pub responses: Vec<ValidityResponse>,
}
