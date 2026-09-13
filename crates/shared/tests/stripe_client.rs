//! The live Stripe HTTP client, exercised against a mock server.
//!
//! Everywhere else Stripe is stubbed through the `PaymentGateway` trait, so these
//! are the only tests that drive the real request building, the auth and
//! idempotency headers, and the response parsing in `StripeClient` itself —
//! without ever touching Stripe. The client is pointed at a `wiremock` server via
//! `StripeClient::with_base_url`.

use std::time::Duration;

use shared::stripe::{ChargeOutcome, OffSessionCharge, PaymentGateway, PaymentIntentRequest, StripeClient, StripeError};
use tokio::time::timeout;
use wiremock::matchers::{body_string_contains, header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockBuilder, MockServer, ResponseTemplate};

const SECRET: &str = "sk_test_123";
const CHARGE_PAGE_SIZE: &str = "100";
const CARD_DECLINED: &str = r#"{"error":{"type":"card_error","code":"card_declined","decline_code":"insufficient_funds","payment_intent":{"id":"pi_3"}}}"#;

fn client(server: &MockServer) -> StripeClient {
    StripeClient::with_base_url(SECRET, server.uri()).expect("build client")
}

fn authorised_post(route: &str) -> MockBuilder {
    Mock::given(method("POST"))
        .and(path(route.to_string()))
        .and(header("authorization", format!("Bearer {SECRET}").as_str()))
}

fn authorised_get(route: &str) -> MockBuilder {
    Mock::given(method("GET"))
        .and(path(route.to_string()))
        .and(header("authorization", format!("Bearer {SECRET}").as_str()))
}

fn json_response(status: u16, body: &str) -> ResponseTemplate {
    ResponseTemplate::new(status)
        .set_body_string(body)
        .insert_header("content-type", "application/json")
}

fn ok(body: &str) -> ResponseTemplate {
    json_response(200, body)
}

fn succeeded(payment_intent_id: &str) -> ChargeOutcome {
    ChargeOutcome::Succeeded {
        payment_intent_id: payment_intent_id.into(),
    }
}

fn pending(payment_intent_id: &str) -> ChargeOutcome {
    ChargeOutcome::Pending {
        payment_intent_id: payment_intent_id.into(),
    }
}

fn declined(payment_intent_id: &str, code: &str) -> ChargeOutcome {
    ChargeOutcome::Declined {
        payment_intent_id: Some(payment_intent_id.into()),
        code: code.into(),
    }
}

fn checkout_intent() -> PaymentIntentRequest {
    PaymentIntentRequest {
        amount_pence: 2_500,
        customer_id: Some("cus_1".into()),
        save_payment_method: true,
        idempotency_key: "ord_1".into(),
        description: "Winter Poppy Raffle 2026".into(),
        metadata: vec![("orderId".into(), "ord_1".into())],
    }
}

fn subscription_charge() -> OffSessionCharge {
    OffSessionCharge {
        amount_pence: 1_000,
        customer_id: "cus_1".into(),
        payment_method_id: "pm_1".into(),
        idempotency_key: "sub_sub-1_winter-2026".into(),
        description: "Winter Poppy Raffle 2026: 10 subscription tickets".into(),
        metadata: vec![("orderId".into(), "sub_sub-1_winter-2026".into())],
    }
}

#[tokio::test]
async fn a_repeated_signup_reuses_the_same_stripe_customer() {
    let server = MockServer::start().await;
    authorised_post("/customers")
        .and(header("idempotency-key", "customer_ent-1"))
        .respond_with(ok(r#"{"id":"cus_new","object":"customer"}"#))
        .mount(&server)
        .await;

    let id = client(&server).create_customer("ada@example.com", "Ada Lovelace", "ent-1").await.unwrap();
    assert_eq!(id, "cus_new", "the key is derived from the entrant, so a retried signup lands on one customer");
}

#[tokio::test]
async fn create_payment_intent_returns_the_id_and_client_secret() {
    let server = MockServer::start().await;
    let request = checkout_intent();
    let idempotency_key = request.idempotency_key.clone();
    authorised_post("/payment_intents")
        .and(header("idempotency-key", idempotency_key.as_str()))
        .respond_with(ok(r#"{"id":"pi_1","client_secret":"pi_1_secret","status":"requires_payment_method"}"#))
        .mount(&server)
        .await;

    let intent = client(&server).create_payment_intent(request).await.unwrap();
    assert_eq!(intent.id, "pi_1");
    assert_eq!(intent.client_secret, "pi_1_secret");
}

#[tokio::test]
async fn off_session_charge_maps_stripe_status_to_a_charge_outcome() {
    let cases: [(&str, u16, &str, ChargeOutcome); 3] = [
        ("succeeded", 200, r#"{"id":"pi_1","status":"succeeded"}"#, succeeded("pi_1")),
        ("processing", 200, r#"{"id":"pi_2","status":"processing"}"#, pending("pi_2")),
        ("card declined", 402, CARD_DECLINED, declined("pi_3", "insufficient_funds")),
    ];

    for (label, status, body, expected) in cases {
        let server = MockServer::start().await;
        let charge = subscription_charge();
        let idempotency_key = charge.idempotency_key.clone();
        authorised_post("/payment_intents")
            .and(header("idempotency-key", idempotency_key.as_str()))
            .respond_with(json_response(status, body))
            .mount(&server)
            .await;

        let outcome = client(&server).charge_off_session(charge).await.unwrap();
        assert_eq!(outcome, expected, "{label}");
    }
}

#[tokio::test]
async fn refund_posts_the_payment_intent_and_returns_the_refund_id() {
    let server = MockServer::start().await;
    authorised_post("/refunds")
        .and(header("idempotency-key", "refund_ord-1"))
        .and(body_string_contains("payment_intent=pi_1"))
        .respond_with(ok(r#"{"id":"re_1","object":"refund"}"#))
        .mount(&server)
        .await;

    let refund_id = client(&server).refund("pi_1", "refund_ord-1").await.unwrap();
    assert_eq!(refund_id, "re_1");
}

#[tokio::test]
async fn list_charges_follows_pagination_within_the_created_window() {
    let server = MockServer::start().await;
    let window = || {
        authorised_get("/charges")
            .and(query_param("created[gte]", "100"))
            .and(query_param("created[lte]", "200"))
            .and(query_param("limit", CHARGE_PAGE_SIZE))
    };
    window()
        .and(query_param_is_missing("starting_after"))
        .respond_with(ok(
            r#"{"object":"list","data":[{"id":"ch_1","status":"succeeded","created":150,"metadata":{"orderId":"ord-1"}}],"has_more":true}"#,
        ))
        .mount(&server)
        .await;
    window()
        .and(query_param("starting_after", "ch_1"))
        .respond_with(ok(
            r#"{"object":"list","data":[{"id":"ch_2","status":"succeeded","created":160,"refunded":true}],"has_more":false}"#,
        ))
        .mount(&server)
        .await;

    let charges = client(&server).list_charges(100, 200).await.unwrap();
    let [first, second] = charges.as_slice() else {
        panic!("expected one charge from each page, got {charges:?}");
    };
    assert_eq!((first.id.as_str(), second.id.as_str()), ("ch_1", "ch_2"), "the pages arrive in order");
    assert_eq!(first.metadata.order_id.as_deref(), Some("ord-1"));
    assert!(second.refunded, "the second page carries the refunded flag through");
}

#[tokio::test]
async fn an_empty_page_ends_the_listing_even_when_stripe_claims_more() {
    let server = MockServer::start().await;
    authorised_get("/charges")
        .respond_with(ok(r#"{"object":"list","data":[],"has_more":true}"#))
        .mount(&server)
        .await;

    let listing = timeout(Duration::from_secs(5), client(&server).list_charges(100, 200)).await;
    let charges = listing
        .expect("a page with no charges ends the only network loop in the crate, whatever has_more says")
        .unwrap();
    assert!(charges.is_empty());
}

#[tokio::test]
async fn api_errors_surface_the_status_and_message() {
    let server = MockServer::start().await;
    authorised_post("/customers")
        .respond_with(json_response(400, r#"{"error":{"type":"invalid_request_error","message":"No such customer"}}"#))
        .mount(&server)
        .await;

    let err = client(&server).create_customer("ada@example.com", "Ada Lovelace", "ent-1").await.unwrap_err();
    let StripeError::Api { status, message } = err else {
        panic!("expected an API error, got {err:?}");
    };
    assert_eq!((status, message.as_str()), (400, "No such customer"));
}

#[tokio::test]
async fn a_non_card_error_on_a_charge_is_a_retryable_failure() {
    let server = MockServer::start().await;
    authorised_post("/payment_intents")
        .respond_with(json_response(500, r#"{"error":{"type":"api_error","message":"boom"}}"#))
        .mount(&server)
        .await;

    let err = client(&server).charge_off_session(subscription_charge()).await.unwrap_err();
    let StripeError::Api { status, message } = err else {
        panic!("a 500 must reach the caller as an error to retry, never as a decline, got {err:?}");
    };
    assert_eq!((status, message.as_str()), (500, "boom"));
}
