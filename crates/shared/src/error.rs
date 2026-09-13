use aws_sdk_dynamodb::error::{BuildError, SdkError};
use thiserror::Error;

use crate::stripe::StripeError;

pub(crate) const CONDITIONAL_CHECK_FAILED: &str = "ConditionalCheckFailed";

#[derive(Debug, Error)]
pub enum AppError {
    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("DynamoDB error: {0}")]
    Dynamo(Box<aws_sdk_dynamodb::Error>),

    #[error("Forbidden: {0}")]
    Forbidden(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Payment provider error: {0}")]
    Payment(String),
}

impl AppError {
    pub fn status_code(&self) -> u16 {
        match self {
            Self::BadRequest(_) => 400,
            Self::Forbidden(_) => 403,
            Self::NotFound(_) => 404,
            Self::Conflict(_) => 409,
            Self::Dynamo(_) | Self::Internal(_) => 500,
            Self::Payment(_) => 502,
        }
    }

    pub fn public_message(&self) -> String {
        match self {
            Self::Dynamo(_) | Self::Internal(_) => "internal error".to_string(),
            Self::Payment(_) => "payment provider unavailable".to_string(),
            other => other.to_string(),
        }
    }

    pub fn is_condition_failed(&self) -> bool {
        let Self::Dynamo(err) = self else {
            return false;
        };
        match &**err {
            aws_sdk_dynamodb::Error::ConditionalCheckFailedException(_) => true,
            aws_sdk_dynamodb::Error::TransactionCanceledException(cancelled) => cancelled
                .cancellation_reasons()
                .iter()
                .any(|reason| reason.code() == Some(CONDITIONAL_CHECK_FAILED)),
            _ => false,
        }
    }
}

impl From<aws_sdk_dynamodb::Error> for AppError {
    fn from(err: aws_sdk_dynamodb::Error) -> Self {
        Self::Dynamo(Box::new(err))
    }
}

impl<E, R> From<SdkError<E, R>> for AppError
where
    aws_sdk_dynamodb::Error: From<SdkError<E, R>>,
{
    fn from(err: SdkError<E, R>) -> Self {
        Self::Dynamo(Box::new(err.into()))
    }
}

impl From<BuildError> for AppError {
    fn from(err: BuildError) -> Self {
        Self::Internal(err.to_string())
    }
}

impl From<serde_dynamo::Error> for AppError {
    fn from(err: serde_dynamo::Error) -> Self {
        Self::Internal(err.to_string())
    }
}

impl From<StripeError> for AppError {
    fn from(err: StripeError) -> Self {
        Self::Payment(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_dynamodb::types::error::{ConditionalCheckFailedException, ResourceNotFoundException, TransactionCanceledException};
    use aws_sdk_dynamodb::types::{CancellationReason, KeySchemaElement};
    use std::collections::HashMap;

    fn dynamo(err: aws_sdk_dynamodb::Error) -> AppError {
        AppError::from(err)
    }

    fn condition_failed() -> aws_sdk_dynamodb::Error {
        aws_sdk_dynamodb::Error::ConditionalCheckFailedException(ConditionalCheckFailedException::builder().build())
    }

    fn transaction_cancelled(codes: [&str; 2]) -> aws_sdk_dynamodb::Error {
        let mut builder = TransactionCanceledException::builder();
        for code in codes {
            builder = builder.cancellation_reasons(CancellationReason::builder().code(code).build());
        }
        aws_sdk_dynamodb::Error::TransactionCanceledException(builder.build())
    }

    fn not_found() -> aws_sdk_dynamodb::Error {
        aws_sdk_dynamodb::Error::ResourceNotFoundException(ResourceNotFoundException::builder().build())
    }

    #[test]
    fn status_code_maps_each_variant_to_its_http_status() {
        let cases = [
            (AppError::BadRequest("x".into()), 400),
            (AppError::Forbidden("x".into()), 403),
            (AppError::NotFound("x".into()), 404),
            (AppError::Conflict("x".into()), 409),
            (AppError::Internal("x".into()), 500),
            (dynamo(not_found()), 500),
            (AppError::Payment("x".into()), 502),
        ];
        for (error, expected) in cases {
            assert_eq!(error.status_code(), expected, "{error}");
        }
    }

    #[test]
    fn public_message_hides_internals_but_passes_client_errors_through() {
        // Server-side faults are redacted; client-facing errors keep their text.
        assert_eq!(dynamo(not_found()).public_message(), "internal error");
        assert_eq!(AppError::Internal("secret".into()).public_message(), "internal error");
        assert_eq!(AppError::Payment("stripe down".into()).public_message(), "payment provider unavailable");
        assert_eq!(
            AppError::BadRequest("email is invalid".into()).public_message(),
            "Bad request: email is invalid"
        );
        assert_eq!(AppError::NotFound("raffle x".into()).public_message(), "Not found: raffle x");
    }

    #[test]
    fn only_a_failed_condition_counts_as_condition_failed() {
        let cases = [
            ("a conditional put that lost its condition", dynamo(condition_failed()), true),
            (
                "a transaction cancelled because one item failed its condition",
                dynamo(transaction_cancelled(["ConditionalCheckFailed", "None"])),
                true,
            ),
            (
                "a throttled transaction is a live failure, not a lost condition",
                dynamo(transaction_cancelled(["ThrottlingError", "None"])),
                false,
            ),
            (
                "capacity exhausted on the second item",
                dynamo(transaction_cancelled(["None", "ProvisionedThroughputExceeded"])),
                false,
            ),
            ("a missing table", dynamo(not_found()), false),
            ("an error that never reached DynamoDB", AppError::BadRequest("x".into()), false),
        ];
        for (label, error, expected) in cases {
            assert_eq!(error.is_condition_failed(), expected, "{label}");
        }
    }

    #[test]
    fn conversions_land_in_the_right_variant() {
        // A Stripe failure is a payment error (502).
        let payment = AppError::from(StripeError::Api {
            status: 400,
            message: "No such customer".into(),
        });
        assert!(matches!(payment, AppError::Payment(_)));
        assert_eq!(payment.status_code(), 502);

        // A builder validation failure is an internal error (500).
        let build_err = KeySchemaElement::builder().build().unwrap_err();
        assert!(matches!(AppError::from(build_err), AppError::Internal(_)));

        // A deserialisation failure is an internal error (500).
        let mut item: HashMap<String, aws_sdk_dynamodb::types::AttributeValue> = HashMap::new();
        item.insert("value".into(), aws_sdk_dynamodb::types::AttributeValue::S("not a number".into()));
        let serde_err = serde_dynamo::aws_sdk_dynamodb_1::from_item::<u64>(item).unwrap_err();
        assert!(matches!(AppError::from(serde_err), AppError::Internal(_)));
    }
}
