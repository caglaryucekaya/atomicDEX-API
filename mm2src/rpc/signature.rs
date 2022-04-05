use common::mm_ctx::MmArc;
use common::mm_error::MmError;
use common::HttpStatusCode;
use derive_more::Display;
use http::StatusCode;
use serde_json::{self as json, Value as Json};

use bitcrypto::{dhash256, ChecksumType};
use keys::{Address, AddressFormat, Signature};
use std::str::FromStr;

#[derive(Serialize, Display, SerializeErrorType)]
#[serde(tag = "error_type", content = "error_data")]
pub enum SignatureError {
    #[display(fmt = "Invalid request: {}", _0)]
    InvalidRequest(String),
}

#[derive(Serialize, Display, SerializeErrorType)]
#[serde(tag = "error_type", content = "error_data")]
pub enum VerificationError {
    #[display(fmt = "Invalid request: {}", _0)]
    InvalidRequest(String),
}

#[derive(Serialize)]
pub struct SignatureResponse {
    signature: String,
}

#[derive(Serialize)]
pub struct VerificationResponse {
    is_valid: bool,
    address: String,
    pubkey: String,
}

pub type SignatureResult<T> = Result<T, MmError<SignatureError>>;

pub type VerificationResult<T> = Result<T, MmError<VerificationError>>;

impl HttpStatusCode for SignatureError {
    fn status_code(&self) -> StatusCode {
        match self {
            SignatureError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
        }
    }
}

impl HttpStatusCode for VerificationError {
    fn status_code(&self) -> StatusCode {
        match self {
            VerificationError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
        }
    }
}

pub async fn sign_message(ctx: MmArc, req: Json) -> SignatureResult<SignatureResponse> {
    let message: String = match json::from_value(req["message"].clone()) {
        Ok(message) => message,
        Err(_) => return MmError::err(SignatureError::InvalidRequest(String::from("No message field"))),
    };

    let key_pair = ctx.secp256k1_key_pair.or(&&|| panic!());
    let private_key = key_pair.private();
    let message_hash = dhash256(message.as_bytes());
    let signature = private_key.sign(&message_hash).unwrap().to_string();

    Ok(SignatureResponse { signature })
}

pub async fn verify_message(ctx: MmArc, req: Json) -> VerificationResult<VerificationResponse> {
    let message: String = match json::from_value(req["message"].clone()) {
        Ok(message) => message,
        Err(_) => return MmError::err(VerificationError::InvalidRequest(String::from("No message field"))),
    };
    let signature: String = match json::from_value(req["signature"].clone()) {
        Ok(signature) => signature,
        Err(_) => return MmError::err(VerificationError::InvalidRequest(String::from("No signature field"))),
    };
    let signature = Signature::from_str(&signature).unwrap();

    let key_pair = ctx.secp256k1_key_pair.or(&&|| panic!());
    let public_key = key_pair.public();
    let message_hash = dhash256(message.as_bytes());
    let is_valid = public_key.verify(&message_hash, &signature).unwrap();

    let address = Address {
        prefix: 0,
        t_addr_prefix: 0,
        hash: public_key.address_hash(),
        checksum_type: ChecksumType::DSHA256,
        hrp: None,
        addr_format: AddressFormat::Standard,
    };

    Ok(VerificationResponse {
        is_valid,
        address: address.to_string(),
        pubkey: public_key.to_string(),
    })
}
