use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use shared::order::{Order, OrderStatus};
use shared::raffle::Raffle;
use shared::stripe::{ChargeOutcome, OffSessionCharge, PaymentGateway, StripeError};
use shared::subscription::{Subscription, SubscriptionStatus};
use shared::table::DynamoRepo;
use shared::testing::{local_repo, raffle, seed_raffle, subscription};
use subscription_charge::{RaffleRun, run};

const RAFFLE_ID: &str = "winter";

type Script = HashMap<&'static str, Result<ChargeOutcome, &'static str>>;

struct Scripted {
    script: Script,
    requests: Mutex<Vec<OffSessionCharge>>,
}

impl Scripted {
    fn new(script: Script) -> Self {
        Self {
            script,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn asked_for(&self) -> Vec<(u64, String, String)> {
        let requests = self.requests.lock().unwrap();
        requests
            .iter()
            .map(|charge| (charge.amount_pence, charge.customer_id.clone(), charge.idempotency_key.clone()))
            .collect()
    }
}

impl PaymentGateway for Scripted {
    async fn charge_off_session(&self, charge: OffSessionCharge) -> Result<ChargeOutcome, StripeError> {
        let outcome = match self.script.get(charge.customer_id.as_str()) {
            Some(Ok(outcome)) => Ok(outcome.clone()),
            Some(Err(message)) => Err(StripeError::Api {
                status: 500,
                message: (*message).into(),
            }),
            None => panic!("unexpected charge for {}", charge.customer_id),
        };

        self.requests.lock().unwrap().push(charge);
        outcome
    }
}

fn succeeds(payment_intent_id: &str) -> Result<ChargeOutcome, &'static str> {
    Ok(ChargeOutcome::Succeeded {
        payment_intent_id: payment_intent_id.into(),
    })
}

fn declines(payment_intent_id: &str, code: &str) -> Result<ChargeOutcome, &'static str> {
    Ok(ChargeOutcome::Declined {
        payment_intent_id: Some(payment_intent_id.into()),
        code: code.into(),
    })
}

fn tally(charged: u32, declined: u32, skipped: u32, errored: u32) -> RaffleRun {
    RaffleRun {
        raffle_id: RAFFLE_ID.into(),
        charged,
        declined,
        skipped,
        errored,
    }
}

async fn seed_opening(repo: &DynamoRepo, raffle_id: &str, opens_in_days: i64, now: DateTime<Utc>) -> Raffle {
    let raffle = raffle(raffle_id, opens_in_days, opens_in_days + 100, now);
    seed_raffle(repo, &raffle).await;
    raffle
}

async fn seed_subscriber(repo: &DynamoRepo, subscription_id: &str, now: DateTime<Utc>) -> Subscription {
    let subscriber = subscription(subscription_id, &format!("ent-{subscription_id}"), now);
    repo.put_subscription(&subscriber).await.unwrap();
    subscriber
}

async fn seed_subscriber_eligible_in(repo: &DynamoRepo, subscription_id: &str, days: i64, now: DateTime<Utc>) -> Subscription {
    let mut subscriber = subscription(subscription_id, &format!("ent-{subscription_id}"), now);
    subscriber.eligible_from = now + Duration::days(days);
    repo.put_subscription(&subscriber).await.unwrap();
    subscriber
}

async fn seed_already_charged(repo: &DynamoRepo, raffle: &Raffle, subscription_id: &str, now: DateTime<Utc>) -> Subscription {
    let subscriber = seed_subscriber(repo, subscription_id, now).await;

    let mut order = Order::from_subscription(raffle, &subscriber, now);
    order.stripe_payment_intent_id = Some(format!("pi_{subscription_id}"));
    assert!(repo.create_order(&order).await.unwrap());
    subscriber
}

async fn subscription_order(repo: &DynamoRepo, subscriber: &Subscription, raffle_id: &str) -> Option<Order> {
    repo.get_order(&subscriber.order_id_for(raffle_id)).await.unwrap()
}

async fn subscription_status(repo: &DynamoRepo, subscription_id: &str) -> SubscriptionStatus {
    repo.get_subscription(subscription_id).await.unwrap().unwrap().status
}

async fn subscriptions_charged(repo: &DynamoRepo, raffle_id: &str) -> bool {
    repo.get_raffle(raffle_id).await.unwrap().unwrap().subscriptions_charged_at.is_some()
}

#[tokio::test]
async fn charges_due_subscriptions_once_and_marks_the_raffle() {
    let Some(repo) = local_repo("charge-test").await else {
        return;
    };
    let now = Utc::now();
    let open = seed_opening(&repo, RAFFLE_ID, -1, now).await;
    seed_opening(&repo, "spring", 30, now).await;

    let due = seed_subscriber(&repo, "due", now).await;
    let not_yet = seed_subscriber_eligible_in(&repo, "later", 10, now).await;
    let already = seed_already_charged(&repo, &open, "already", now).await;

    let gateway = Scripted::new(Script::from([("cus_due", succeeds("pi_due"))]));
    let runs = run(&repo, &gateway, now).await.unwrap();
    assert_eq!(
        runs,
        vec![tally(1, 0, 1, 0)],
        "one charge for due, one skip for the order that already has an intent"
    );

    let charged = subscription_order(&repo, &due, RAFFLE_ID).await.unwrap();
    assert_eq!(charged.status, OrderStatus::Pending, "the webhook allocates the tickets, not this run");
    assert_eq!(charged.stripe_payment_intent_id.as_deref(), Some("pi_due"));
    assert_eq!(repo.find_order_by_payment_intent("pi_due").await.unwrap().unwrap().order_id, charged.order_id);
    assert_eq!(
        gateway.asked_for(),
        vec![(charged.total_pence, due.stripe_customer_id.clone(), charged.order_id.clone())],
        "Stripe is asked for the order total under the order id, which is what makes a raffle charge exactly once"
    );

    assert!(
        subscription_order(&repo, &not_yet, RAFFLE_ID).await.is_none(),
        "due_subscriptions applies is_due_for to the ACTIVE page"
    );
    assert!(
        subscription_order(&repo, &already, "spring").await.is_none(),
        "the scheduled raffle is not charged until it opens"
    );
    assert!(subscriptions_charged(&repo, RAFFLE_ID).await);
    assert!(!subscriptions_charged(&repo, "spring").await);

    let rerun = run(&repo, &Scripted::new(Script::new()), now).await.unwrap();
    assert!(rerun.is_empty(), "a stamped raffle is never charged again");
}

#[tokio::test]
async fn a_declined_charge_fails_the_order_and_suspends_the_subscriber() {
    let Some(repo) = local_repo("charge-test").await else {
        return;
    };
    let now = Utc::now();
    seed_opening(&repo, RAFFLE_ID, -1, now).await;
    let declined = seed_subscriber(&repo, "declined", now).await;

    let gateway = Scripted::new(Script::from([("cus_declined", declines("pi_declined", "insufficient_funds"))]));
    let runs = run(&repo, &gateway, now).await.unwrap();
    assert_eq!(runs, vec![tally(0, 1, 0, 0)]);

    let failed = subscription_order(&repo, &declined, RAFFLE_ID).await.unwrap();
    assert_eq!(failed.status, OrderStatus::Failed);
    assert_eq!(
        failed.stripe_payment_intent_id.as_deref(),
        Some("pi_declined"),
        "the declined intent is kept so the charge can be traced"
    );
    assert_eq!(
        subscription_status(&repo, "declined").await,
        SubscriptionStatus::PastDue,
        "dunning starts here, and the subscriber is skipped until they pay"
    );
    assert!(
        subscriptions_charged(&repo, RAFFLE_ID).await,
        "a decline is a settled outcome, so the raffle is done and must not be charged again"
    );
}

#[tokio::test]
async fn a_closed_raffle_is_never_charged() {
    let Some(repo) = local_repo("charge-test").await else {
        return;
    };
    let now = Utc::now();
    seed_opening(&repo, "past", -200, now).await;
    seed_subscriber(&repo, "due", now).await;

    let runs = run(&repo, &Scripted::new(Script::new()), now).await.unwrap();
    assert!(
        runs.is_empty(),
        "a raffle whose tickets can no longer be bought is never charged, however long its subscribers have been eligible"
    );
}

#[tokio::test]
async fn stripe_errors_leave_the_raffle_open_for_the_next_run() {
    let Some(repo) = local_repo("charge-test").await else {
        return;
    };
    let now = Utc::now();
    seed_opening(&repo, RAFFLE_ID, -1, now).await;
    let flaky = seed_subscriber(&repo, "flaky", now).await;
    seed_subscriber(&repo, "fine", now).await;

    let stripe_down = Script::from([("cus_flaky", Err("stripe down")), ("cus_fine", succeeds("pi_fine"))]);
    let runs = run(&repo, &Scripted::new(stripe_down), now).await.unwrap();
    assert_eq!(runs, vec![tally(1, 0, 0, 1)]);
    assert!(
        !subscriptions_charged(&repo, RAFFLE_ID).await,
        "an errored subscriber leaves the raffle open so the next run retries it"
    );

    let stuck = subscription_order(&repo, &flaky, RAFFLE_ID).await.unwrap();
    assert_eq!(stuck.status, OrderStatus::Pending);
    assert!(stuck.stripe_payment_intent_id.is_none(), "no intent means the next run re-adopts this order");

    let recovered = Scripted::new(Script::from([("cus_flaky", succeeds("pi_flaky"))]));
    let runs = run(&repo, &recovered, now + Duration::hours(1)).await.unwrap();
    assert_eq!(runs, vec![tally(1, 0, 1, 0)], "the subscriber who already paid is skipped, not charged twice");
    assert!(subscriptions_charged(&repo, RAFFLE_ID).await);

    let retried = subscription_order(&repo, &flaky, RAFFLE_ID).await.unwrap();
    assert_eq!(retried.stripe_payment_intent_id.as_deref(), Some("pi_flaky"));
    assert_eq!(
        recovered.asked_for(),
        vec![(retried.total_pence, flaky.stripe_customer_id.clone(), retried.order_id.clone())],
        "the retry reuses the same idempotency key, so Stripe cannot double-charge"
    );
}
