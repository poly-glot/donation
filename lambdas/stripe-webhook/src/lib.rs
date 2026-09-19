use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use lambda_http::{Body, Request, Response};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use shared::error::AppError;
use shared::order::{Allocation, Entry, Order, OrderStatus, PaidPayment};
use shared::stripe::{Charge, OrderMetadata, PaymentGateway};
use shared::subscription::{Subscription, SubscriptionStatus, TICKETS_PER_SUBSCRIPTION};
use shared::table::DynamoRepo;
use shared::telemetry;

const SIGNATURE_TOLERANCE_SECONDS: i64 = 300;
const DEBIT_FUNDING: &str = "debit";

pub fn verify_signature(payload: &[u8], header: &str, secret: &str, now: i64) -> bool {
    let mut timestamp = None;
    let mut signatures = Vec::new();

    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", value)) => timestamp = value.parse::<i64>().ok(),
            Some(("v1", value)) => signatures.push(value),
            _ => {}
        }
    }

    let Some(timestamp) = timestamp else {
        return false;
    };
    if (now - timestamp).abs() > SIGNATURE_TOLERANCE_SECONDS {
        return false;
    }

    signatures.into_iter().any(|signature| {
        let Ok(expected) = hex::decode(signature) else {
            return false;
        };
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
            return false;
        };
        mac.update(timestamp.to_string().as_bytes());
        mac.update(b".");
        mac.update(payload);
        mac.verify_slice(&expected).is_ok()
    })
}

#[derive(Debug, Deserialize)]
pub struct Event {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct PaymentIntent {
    id: String,
    #[serde(default)]
    metadata: OrderMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    PaymentFailed,
    CardNotDebit,
    SoldOut,
}

impl FailReason {
    fn suspends_subscription(self) -> bool {
        !matches!(self, Self::SoldOut)
    }

    fn refunds(self) -> bool {
        !matches!(self, Self::PaymentFailed)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Ignored,
    Allocated(Entry),
    AlreadyPaid,
    Refunded,
    Unchanged,
    Failed {
        order_id: String,
        reason: FailReason,
        refund_id: Option<String>,
    },
}

impl Outcome {
    fn metric(&self) -> Option<&'static str> {
        match self {
            Self::Allocated(_) => Some("WebhookAllocated"),
            Self::Failed { .. } => Some("WebhookFailed"),
            _ => None,
        }
    }
}

pub struct Webhook<G> {
    repo: DynamoRepo,
    gateway: G,
    secret: String,
}

impl<G: PaymentGateway> Webhook<G> {
    pub fn new(repo: DynamoRepo, gateway: G, secret: impl Into<String>) -> Self {
        Self {
            repo,
            gateway,
            secret: secret.into(),
        }
    }

    pub async fn handle(&self, request: Request) -> Result<Response<Body>, lambda_http::Error> {
        let signature = request
            .headers()
            .get("stripe-signature")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let payload: &[u8] = request.body().as_ref();
        let now = Utc::now();

        if !verify_signature(payload, signature, &self.secret, now.timestamp()) {
            let digest = hex::encode(Sha256::digest(payload));
            tracing::warn!(status = 400_u16, signature, payload_bytes = payload.len(), payload_sha256 = %digest, "webhook signature rejected");
            return respond(400, "invalid signature");
        }
        let event: Event = match serde_json::from_slice(payload) {
            Ok(event) => event,
            Err(err) => {
                tracing::warn!(status = 400_u16, error = %err, "webhook payload malformed");
                return respond(400, format!("malformed event: {err}"));
            }
        };

        let event_id = event.id.clone();
        let kind = event.kind.clone();
        let outcome = match self.process(event, now).await {
            Ok(outcome) => outcome,
            Err(err) => {
                tracing::error!(status = 500_u16, event_id = %event_id, kind = %kind, error = %err, "webhook errored, stripe will retry");
                return respond(500, "retry");
            }
        };

        if let Some(metric) = outcome.metric() {
            telemetry::emit(&[(metric, 1.0)], &[("eventId", &event_id)]);
        }
        tracing::info!(event_id = %event_id, kind = %kind, outcome = ?outcome, "webhook processed");
        respond(200, format!("{outcome:?}"))
    }

    pub async fn process(&self, event: Event, now: DateTime<Utc>) -> Result<Outcome, AppError> {
        match event.kind.as_str() {
            "charge.succeeded" => self.charge_succeeded(parse(&event)?, now).await,
            "charge.refunded" => self.charge_refunded(parse(&event)?).await,
            "payment_intent.payment_failed" => self.payment_failed(parse(&event)?).await,
            _ => Ok(Outcome::Ignored),
        }
    }

    async fn charge_succeeded(&self, charge: Charge, now: DateTime<Utc>) -> Result<Outcome, AppError> {
        let Some(payment_intent_id) = charge.payment_intent else {
            return Ok(Outcome::Ignored);
        };
        let order = self.find_order(charge.metadata.order_id.as_deref(), &payment_intent_id).await?;

        let card = charge.payment_method_details.card;
        if card.funding.as_deref() != Some(DEBIT_FUNDING) {
            return self.fail_order(&order, FailReason::CardNotDebit, Some(&payment_intent_id)).await;
        }

        let payment = PaidPayment {
            payment_intent_id: payment_intent_id.clone(),
            card_funding: card.funding,
            card_last4: card.last4,
        };
        let outcome = match self.repo.allocate_entry(&order.order_id, &payment, now).await? {
            Allocation::Allocated(entry) => Outcome::Allocated(entry),
            Allocation::AlreadyPaid => Outcome::AlreadyPaid,
            Allocation::SoldOut => return self.fail_order(&order, FailReason::SoldOut, Some(&payment_intent_id)).await,
        };

        if order.subscribe {
            self.ensure_subscription(&order, charge.customer, charge.payment_method, now).await?;
        }
        Ok(outcome)
    }

    async fn ensure_subscription(
        &self,
        order: &Order,
        customer_id: Option<String>,
        payment_method_id: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<(), AppError> {
        let (Some(customer_id), Some(payment_method_id)) = (customer_id, payment_method_id) else {
            tracing::warn!(order_id = %order.order_id, "subscribe requested but charge carries no saved card");
            return Ok(());
        };

        let subscription_id = Subscription::id_for_entrant(&order.entrant_id);
        if self.repo.get_subscription(&subscription_id).await?.is_some() {
            return Ok(());
        }

        let raffle = self.repo.get_raffle(&order.raffle_id).await?;
        let subscription = Subscription {
            subscription_id,
            entrant_id: order.entrant_id.clone(),
            stripe_customer_id: customer_id,
            stripe_payment_method_id: payment_method_id,
            tickets_per_raffle: TICKETS_PER_SUBSCRIPTION,
            eligible_from: Subscription::eligible_from(raffle.as_ref(), now),
            status: SubscriptionStatus::Active,
            created_at: now,
        };
        self.repo.put_subscription(&subscription).await
    }

    async fn charge_refunded(&self, charge: Charge) -> Result<Outcome, AppError> {
        let Some(payment_intent_id) = charge.payment_intent.filter(|_| charge.refunded) else {
            return Ok(Outcome::Ignored);
        };
        let order = self.find_order(charge.metadata.order_id.as_deref(), &payment_intent_id).await?;

        let changed = self.repo.set_order_status(&order.order_id, OrderStatus::Paid, OrderStatus::Refunded).await?;
        Ok(if changed { Outcome::Refunded } else { Outcome::Unchanged })
    }

    async fn payment_failed(&self, intent: PaymentIntent) -> Result<Outcome, AppError> {
        let order = self.find_order(intent.metadata.order_id.as_deref(), &intent.id).await?;
        self.fail_order(&order, FailReason::PaymentFailed, None).await
    }

    async fn fail_order(&self, order: &Order, reason: FailReason, payment_intent_id: Option<&str>) -> Result<Outcome, AppError> {
        if order.status != OrderStatus::Pending {
            return Ok(Outcome::Unchanged);
        }

        let refund_id = if let Some(payment_intent_id) = payment_intent_id
            && reason.refunds()
        {
            Some(self.gateway.refund(payment_intent_id, &format!("refund_{}", order.order_id)).await?)
        } else {
            None
        };

        let changed = self.repo.set_order_status(&order.order_id, OrderStatus::Pending, OrderStatus::Failed).await?;
        if !changed {
            return Ok(Outcome::Unchanged);
        }

        if let Some(subscription_id) = order.subscription_id.as_deref()
            && reason.suspends_subscription()
        {
            self.repo.set_subscription_status(subscription_id, SubscriptionStatus::PastDue).await?;
        }
        Ok(Outcome::Failed {
            order_id: order.order_id.clone(),
            reason,
            refund_id,
        })
    }

    async fn find_order(&self, order_id: Option<&str>, payment_intent_id: &str) -> Result<Order, AppError> {
        if let Some(order_id) = order_id
            && let Some(order) = self.repo.get_order(order_id).await?
        {
            return Ok(order);
        }

        let by_payment_intent = self.repo.find_order_by_payment_intent(payment_intent_id).await?;
        by_payment_intent.ok_or_else(|| AppError::NotFound(format!("order for {payment_intent_id}")))
    }
}

fn parse<T: DeserializeOwned>(event: &Event) -> Result<T, AppError> {
    serde_json::from_value(event.data["object"].clone()).map_err(|err| AppError::BadRequest(format!("{} payload: {err}", event.kind)))
}

fn respond(status: u16, body: impl Into<Body>) -> Result<Response<Body>, lambda_http::Error> {
    Ok(Response::builder().status(status).body(body.into())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(payload: &[u8], secret: &str, timestamp: i64) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("{timestamp}.").as_bytes());
        mac.update(payload);
        format!("t={timestamp},v1={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn accepts_a_fresh_valid_signature() {
        let payload = br#"{"id":"evt_1","type":"charge.succeeded"}"#;
        let header = sign(payload, "whsec_test", 1_700_000_000);
        assert!(verify_signature(payload, &header, "whsec_test", 1_700_000_100));
    }

    #[test]
    fn accepts_when_any_v1_signature_matches() {
        let payload = b"{}";
        let header = format!("{},v1=deadbeef", sign(payload, "whsec_test", 1_700_000_000));
        assert!(verify_signature(payload, &header, "whsec_test", 1_700_000_000));
    }

    #[test]
    fn rejects_wrong_secret_tampering_staleness_and_garbage() {
        let payload = br#"{"amount":100}"#;
        let header = sign(payload, "whsec_test", 1_700_000_000);

        assert!(!verify_signature(payload, &header, "whsec_other", 1_700_000_000));
        assert!(!verify_signature(br#"{"amount":999}"#, &header, "whsec_test", 1_700_000_000));
        assert!(!verify_signature(payload, &header, "whsec_test", 1_700_000_000 + 301));
        assert!(!verify_signature(payload, &header, "whsec_test", 1_700_000_000 - 301));
        assert!(!verify_signature(payload, "garbage", "whsec_test", 1_700_000_000));
        assert!(!verify_signature(payload, "t=abc,v1=00", "whsec_test", 1_700_000_000));
        assert!(!verify_signature(payload, "v1=00", "whsec_test", 1_700_000_000));
        assert!(!verify_signature(payload, "", "whsec_test", 1_700_000_000));
    }

    #[test]
    fn charge_payload_parses_with_missing_optional_parts() {
        let event: Event = serde_json::from_value(serde_json::json!({
            "id": "evt_1",
            "type": "charge.succeeded",
            "data": { "object": { "id": "ch_1", "payment_intent": "pi_1" } }
        }))
        .unwrap();
        let charge: Charge = parse(&event).unwrap();
        assert_eq!(charge.payment_intent.as_deref(), Some("pi_1"));
        assert!(charge.metadata.order_id.is_none());
        assert!(charge.payment_method_details.card.funding.is_none());
        assert!(charge.customer.is_none());
        assert!(!charge.refunded);
    }

    #[test]
    fn only_card_and_capacity_failures_refund_and_sold_out_keeps_subscription() {
        assert!(FailReason::CardNotDebit.refunds());
        assert!(FailReason::SoldOut.refunds());
        assert!(!FailReason::PaymentFailed.refunds());
        assert!(!FailReason::SoldOut.suspends_subscription());
        assert!(FailReason::PaymentFailed.suspends_subscription());
    }
}
