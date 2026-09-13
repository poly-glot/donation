use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use lambda_http::{Body, http};
use serde_json::{Value, json};
use shared::error::AppError;
use shared::order::{Order, OrderStatus};
use shared::raffle::Raffle;
use shared::stripe::{PaymentGateway, StripeError};
use shared::subscription::SubscriptionStatus;
use shared::table::DynamoRepo;
use shared::testing::{local_repo, raffle, seed_raffle, subscription};
use stripe_webhook::{Event, FailReason, Outcome, Webhook};

const RAFFLE_ID: &str = "winter-2026";

type Refunds = Arc<Mutex<Vec<(String, String)>>>;

#[derive(Default)]
struct RefundLog(Refunds);

impl PaymentGateway for RefundLog {
    async fn refund(&self, payment_intent_id: &str, idempotency_key: &str) -> Result<String, StripeError> {
        self.0.lock().unwrap().push((payment_intent_id.into(), idempotency_key.into()));
        Ok(format!("re_{payment_intent_id}"))
    }
}

async fn webhook() -> Option<(Webhook<RefundLog>, DynamoRepo, Refunds)> {
    let repo = local_repo("webhook-test").await?;
    let log = RefundLog::default();
    let refunds = Arc::clone(&log.0);
    Some((Webhook::new(repo.clone(), log, "whsec_test"), repo, refunds))
}

fn event(id: &str, kind: &str, object: Value) -> Event {
    serde_json::from_value(json!({ "id": id, "type": kind, "data": { "object": object } })).unwrap()
}

fn charge(payment_intent: &str, order_id: Option<&str>, funding: &str) -> Value {
    let mut object = json!({
        "id": format!("ch_{payment_intent}"),
        "object": "charge",
        "payment_intent": payment_intent,
        "payment_method_details": { "card": { "funding": funding, "last4": "4242" } }
    });
    if let Some(order_id) = order_id {
        object["metadata"] = json!({ "orderId": order_id });
    }
    object
}

fn charge_of(order_id: &str, funding: &str) -> Value {
    charge(&format!("pi_{order_id}"), Some(order_id), funding)
}

fn paid_event(event_id: &str, order_id: &str, funding: &str) -> Event {
    event(event_id, "charge.succeeded", charge_of(order_id, funding))
}

fn refund_event(event_id: &str, order_id: &str, fully_refunded: bool) -> Event {
    let mut object = charge_of(order_id, "debit");
    object["refunded"] = json!(fully_refunded);
    event(event_id, "charge.refunded", object)
}

fn charge_saving_the_card(order_id: &str, customer_id: &str, payment_method_id: &str) -> Value {
    let mut object = charge_of(order_id, "debit");
    object["customer"] = json!(customer_id);
    object["payment_method"] = json!(payment_method_id);
    object
}

fn failed(order_id: &str, reason: FailReason, refund_id: Option<&str>) -> Outcome {
    Outcome::Failed {
        order_id: order_id.into(),
        reason,
        refund_id: refund_id.map(Into::into),
    }
}

async fn open_raffle(repo: &DynamoRepo, now: DateTime<Utc>) -> Raffle {
    let raffle = raffle(RAFFLE_ID, -1, 100, now);
    seed_raffle(repo, &raffle).await;
    raffle
}

async fn pending_order(repo: &DynamoRepo, raffle: &Raffle, order_id: &str, quantity: u32, now: DateTime<Utc>) -> Order {
    let mut order = Order::single(order_id, raffle, "ent-1", quantity, 0, false, now);
    order.stripe_payment_intent_id = Some(format!("pi_{order_id}"));
    assert!(repo.create_order(&order).await.unwrap());
    order
}

async fn order_status(repo: &DynamoRepo, order_id: &str) -> OrderStatus {
    repo.get_order(order_id).await.unwrap().unwrap().status
}

async fn tickets_sold(repo: &DynamoRepo) -> u64 {
    repo.get_raffle(RAFFLE_ID).await.unwrap().unwrap().tickets_sold
}

async fn subscription_status(repo: &DynamoRepo, subscription_id: &str) -> SubscriptionStatus {
    repo.get_subscription(subscription_id).await.unwrap().unwrap().status
}

fn refunded(refunds: &Refunds) -> Vec<(String, String)> {
    refunds.lock().unwrap().clone()
}

fn refund_of(order_id: &str) -> (String, String) {
    (format!("pi_{order_id}"), format!("refund_{order_id}"))
}

fn refund_id_of(order_id: &str) -> String {
    format!("re_pi_{order_id}")
}

#[tokio::test]
async fn charge_succeeded_allocates_once_and_replays_are_harmless() {
    let Some((webhook, repo, refunds)) = webhook().await else {
        return;
    };
    let now = Utc::now();
    let raffle = open_raffle(&repo, now).await;
    let order = pending_order(&repo, &raffle, "ord-1", 15, now).await;

    let delivery = || paid_event("evt_paid", "ord-1", "debit");
    let first = webhook.process(delivery(), now).await.unwrap();
    let Outcome::Allocated(entry) = first else {
        panic!("expected an allocation, got {first:?}");
    };
    assert_eq!(
        (entry.ticket_from, entry.ticket_to),
        (1, u64::from(order.ticket_quantity)),
        "the first order on a fresh raffle takes as many tickets as it bought"
    );
    assert_eq!(
        webhook.process(delivery(), now).await.unwrap(),
        Outcome::AlreadyPaid,
        "a redelivery allocates nothing"
    );

    let without_metadata = event("evt_no_metadata", "charge.succeeded", charge("pi_ord-1", None, "debit"));
    let replayed = webhook.process(without_metadata, now).await.unwrap();
    assert_eq!(replayed, Outcome::AlreadyPaid, "the order is found by its PaymentIntent alone");

    let order = repo.get_order("ord-1").await.unwrap().unwrap();
    assert_eq!(order.status, OrderStatus::Paid);
    assert_eq!(order.card_last4.as_deref(), Some("4242"), "the card detail comes from the charge");
    assert_eq!(tickets_sold(&repo).await, 15, "three deliveries, one allocation");
    assert!(refunded(&refunds).is_empty(), "an allocated debit charge is never refunded");
}

#[tokio::test]
async fn a_credit_card_is_refunded_and_allocates_nothing() {
    let Some((webhook, repo, refunds)) = webhook().await else {
        return;
    };
    let now = Utc::now();
    let raffle = open_raffle(&repo, now).await;
    pending_order(&repo, &raffle, "ord-credit", 5, now).await;

    let delivery = || paid_event("evt_credit", "ord-credit", "credit");
    let refused = webhook.process(delivery(), now).await.unwrap();
    assert_eq!(refused, failed("ord-credit", FailReason::CardNotDebit, Some(&refund_id_of("ord-credit"))));
    assert_eq!(order_status(&repo, "ord-credit").await, OrderStatus::Failed);
    assert_eq!(tickets_sold(&repo).await, 0, "a licence breach takes no tickets");

    let redelivered = webhook.process(delivery(), now).await.unwrap();
    assert_eq!(redelivered, Outcome::Unchanged, "the order has already failed");
    assert_eq!(refunded(&refunds), vec![refund_of("ord-credit")], "one refund, keyed on the order");
}

#[tokio::test]
async fn a_charge_that_arrives_after_the_last_ticket_is_refunded() {
    let Some((webhook, repo, refunds)) = webhook().await else {
        return;
    };
    let now = Utc::now();
    let mut raffle = raffle(RAFFLE_ID, -1, 100, now);
    raffle.max_tickets = 20;
    seed_raffle(&repo, &raffle).await;
    pending_order(&repo, &raffle, "ord-big", 15, now).await;
    pending_order(&repo, &raffle, "ord-late", 10, now).await;

    let big = paid_event("evt_big", "ord-big", "debit");
    let allocated = webhook.process(big, now).await.unwrap();
    let Outcome::Allocated(filled) = allocated else {
        panic!("15 of the 20 tickets must allocate, got {allocated:?}");
    };
    assert_eq!(filled.ticket_to, 15, "the cap has five tickets left");

    let late = paid_event("evt_late", "ord-late", "debit");
    let refused = webhook.process(late, now).await.unwrap();
    assert_eq!(refused, failed("ord-late", FailReason::SoldOut, Some(&refund_id_of("ord-late"))));
    assert_eq!(order_status(&repo, "ord-late").await, OrderStatus::Failed);
    assert_eq!(tickets_sold(&repo).await, 15, "the cap refuses a partial allocation");
    assert_eq!(refunded(&refunds), vec![refund_of("ord-late")], "one refund, keyed on the order");
}

#[tokio::test]
async fn payment_failure_on_a_subscription_order_suspends_without_refund() {
    let Some((webhook, repo, refunds)) = webhook().await else {
        return;
    };
    let now = Utc::now();
    let raffle = open_raffle(&repo, now).await;
    let subscription = subscription("sub-1", "ent-2", now);
    repo.put_subscription(&subscription).await.unwrap();

    let mut order = Order::from_subscription(&raffle, &subscription, now);
    order.stripe_payment_intent_id = Some("pi_sub".into());
    assert!(repo.create_order(&order).await.unwrap());

    let delivery = || event("evt_fail", "payment_intent.payment_failed", json!({ "id": "pi_sub" }));
    let outcome = webhook.process(delivery(), now).await.unwrap();
    assert_eq!(outcome, failed(&order.order_id, FailReason::PaymentFailed, None));
    assert_eq!(subscription_status(&repo, "sub-1").await, SubscriptionStatus::PastDue, "dunning starts here");

    let redelivered = webhook.process(delivery(), now).await.unwrap();
    assert_eq!(redelivered, Outcome::Unchanged, "the order has already failed");
    assert!(refunded(&refunds).is_empty(), "a payment that never succeeded has nothing to refund");
}

#[tokio::test]
async fn subscribe_orders_create_the_subscription_from_the_saved_card() {
    let Some((webhook, repo, _)) = webhook().await else {
        return;
    };
    let now = Utc::now();
    let raffle = open_raffle(&repo, now).await;
    let mut order = Order::single("ord-sub", &raffle, "ent-9", 10, 0, false, now);
    order.subscribe = true;
    order.stripe_payment_intent_id = Some("pi_ord-sub".into());
    assert!(repo.create_order(&order).await.unwrap());

    let saved_card = charge_saving_the_card("ord-sub", "cus_9", "pm_9");
    let delivery = || event("evt_sub", "charge.succeeded", saved_card.clone());
    let allocated = webhook.process(delivery(), now).await.unwrap();
    let Outcome::Allocated(entry) = allocated else {
        panic!("a saved-card charge must allocate, got {allocated:?}");
    };
    assert_eq!(entry.order_id, "ord-sub");

    let subscription = repo.get_subscription("sub_ent-9").await.unwrap().unwrap();
    assert_eq!(subscription.status, SubscriptionStatus::Active);
    assert_eq!(
        (subscription.stripe_customer_id.as_str(), subscription.stripe_payment_method_id.as_str()),
        ("cus_9", "pm_9"),
        "the card Stripe saved is the one to charge next time"
    );
    assert_eq!(subscription.tickets_per_raffle, 10, "as many tickets as this order bought");
    assert_eq!(subscription.eligible_from, raffle.closes_at, "the next raffle is the first to charge for");

    let redelivered = webhook.process(delivery(), now + Duration::hours(1)).await.unwrap();
    assert_eq!(redelivered, Outcome::AlreadyPaid);
    assert_eq!(
        repo.get_subscription("sub_ent-9").await.unwrap().unwrap().created_at,
        subscription.created_at,
        "a redelivery must not recreate the subscription"
    );
}

#[tokio::test]
async fn full_refund_flips_paid_orders_and_partial_refunds_are_ignored() {
    let Some((webhook, repo, _)) = webhook().await else {
        return;
    };
    let now = Utc::now();
    let raffle = open_raffle(&repo, now).await;
    pending_order(&repo, &raffle, "ord-1", 5, now).await;
    webhook.process(paid_event("evt_1", "ord-1", "debit"), now).await.unwrap();

    let partial = refund_event("evt_partial", "ord-1", false);
    assert_eq!(webhook.process(partial, now).await.unwrap(), Outcome::Ignored, "the entrant keeps the tickets");
    assert_eq!(order_status(&repo, "ord-1").await, OrderStatus::Paid);

    let full = refund_event("evt_full", "ord-1", true);
    assert_eq!(webhook.process(full, now).await.unwrap(), Outcome::Refunded);
    assert_eq!(order_status(&repo, "ord-1").await, OrderStatus::Refunded);

    let unhandled_kind = event("evt_other", "invoice.paid", json!({}));
    assert_eq!(
        webhook.process(unhandled_kind, now).await.unwrap(),
        Outcome::Ignored,
        "an event type we subscribe to but do not act on"
    );
}

#[tokio::test]
async fn a_failed_delivery_succeeds_when_retried() {
    let Some((webhook, repo, _)) = webhook().await else {
        return;
    };
    let now = Utc::now();
    let raffle = open_raffle(&repo, now).await;

    let delivery = || paid_event("evt_early", "ord-1", "debit");
    let too_early = webhook.process(delivery(), now).await;
    assert!(
        matches!(too_early, Err(AppError::NotFound(_))),
        "handle answers 500 on any error, which is what makes Stripe redeliver, got {too_early:?}"
    );

    let order = pending_order(&repo, &raffle, "ord-1", 5, now).await;
    let redelivered = webhook.process(delivery(), now).await.unwrap();
    let Outcome::Allocated(entry) = redelivered else {
        panic!("the redelivery must allocate once the order exists, got {redelivered:?}");
    };
    assert_eq!(entry.order_id, order.order_id);
}

#[tokio::test]
async fn http_handler_rejects_unsigned_requests() {
    let Some((webhook, _, _)) = webhook().await else {
        return;
    };
    let unsigned = http::Request::builder().body(Body::from("{}")).unwrap();
    assert_eq!(webhook.handle(unsigned).await.unwrap().status(), 400, "no stripe-signature header");

    let forged = http::Request::builder().header("stripe-signature", "t=1,v1=00").body(Body::from("{}")).unwrap();
    assert_eq!(webhook.handle(forged).await.unwrap().status(), 400, "a signature that does not verify");
}
