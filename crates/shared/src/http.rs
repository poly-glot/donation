use chrono::{DateTime, Utc};
use lambda_http::http::Method;
use lambda_http::http::header::{AUTHORIZATION, CACHE_CONTROL, HeaderValue};
use lambda_http::{Body, Request, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::auth::{Claims, Cognito, bearer};
use crate::error::AppError;

pub async fn authorise(cognito: &Cognito, request: &Request, now: DateTime<Utc>) -> Result<Claims, AppError> {
    if request.method() != Method::POST {
        return Err(AppError::NotFound("route".into()));
    }

    let authorization = request.headers().get(AUTHORIZATION).and_then(|value| value.to_str().ok());
    cognito.verify(bearer(authorization)?, now).await
}

pub fn body<T: DeserializeOwned>(request: &Request) -> Result<T, AppError> {
    serde_json::from_slice(request.body().as_ref()).map_err(|err| AppError::BadRequest(format!("invalid body: {err}")))
}

pub fn answered<T: Serialize>(result: Result<T, AppError>) -> Response<Body> {
    match result {
        Ok(value) => private(json(200, &value)),
        Err(err) => private(refused(&err)),
    }
}

pub fn refused(err: &AppError) -> Response<Body> {
    let status = err.status_code();
    if status >= 500 {
        tracing::error!(status, error = %err, "request failed");
    } else {
        tracing::warn!(status, error = %err, "request rejected");
    }

    json(status, &json!({ "error": err.public_message() }))
}

pub fn private(mut response: Response<Body>) -> Response<Body> {
    response.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub fn json<T: Serialize>(status: u16, body: &T) -> Response<Body> {
    let payload = serde_json::to_string(body).unwrap_or_else(|_| "{}".to_string());

    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(payload))
        .unwrap_or_else(|_| Response::new(Body::Empty))
}
