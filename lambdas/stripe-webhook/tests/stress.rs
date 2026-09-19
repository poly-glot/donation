use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::json;
use shared::order::Order;
use shared::raffle::Raffle;
use shared::stripe::PaymentGateway;
use shared::table::DynamoRepo;
use shared::testing::{local_repo, raffle, seed_raffle};
use stripe_webhook::{Event, Outcome, Webhook};
use tokio::task::JoinSet;

const TICKETS_PER_ORDER: u32 = 10;

struct NoGateway;

impl PaymentGateway for NoGateway {}

struct Burst {
    orders: usize,
    elapsed: Duration,
    allocated: usize,
    errored: usize,
    latencies: Vec<Duration>,
}

impl Burst {
    fn per_second(&self) -> f64 {
        self.orders as f64 / self.elapsed.as_secs_f64()
    }

    fn percentile(&self, hundredths: usize) -> Duration {
        let index = (self.latencies.len() * hundredths / 100).min(self.latencies.len() - 1);
        self.latencies[index]
    }

    fn report(&self, label: &str) {
        println!(
            "{label:>28}: {:>4} orders in {:>6.2}s = {:>6.1}/s, {:>4} allocated, {:>3} errored, latency p50 {:?} p99 {:?} max {:?}",
            self.orders,
            self.elapsed.as_secs_f64(),
            self.per_second(),
            self.allocated,
            self.errored,
            self.percentile(50),
            self.percentile(99),
            self.latencies.last().copied().unwrap_or_default()
        );
    }
}

fn paid_event(order_id: &str) -> Event {
    serde_json::from_value(json!({
        "id": format!("evt_{order_id}"),
        "type": "charge.succeeded",
        "data": { "object": {
            "id": format!("ch_{order_id}"),
            "object": "charge",
            "payment_intent": format!("pi_{order_id}"),
            "metadata": { "orderId": order_id },
            "payment_method_details": { "card": { "funding": "debit", "last4": "5556" } }
        } }
    }))
    .unwrap()
}

async fn open_raffle(repo: &DynamoRepo, raffle_id: &str, now: DateTime<Utc>) -> Raffle {
    let raffle = raffle(raffle_id, -1, 100, now);
    seed_raffle(repo, &raffle).await;
    raffle
}

async fn pending_orders(repo: &DynamoRepo, raffle: &Raffle, count: usize, now: DateTime<Utc>) -> Vec<String> {
    let mut order_ids = Vec::with_capacity(count);
    for i in 1..=count {
        let order_id = format!("{}-ord-{i}", raffle.raffle_id);
        let mut order = Order::single(&order_id, raffle, "ent-1", TICKETS_PER_ORDER, 0, false, now);
        order.stripe_payment_intent_id = Some(format!("pi_{order_id}"));

        assert!(repo.create_order(&order).await.unwrap(), "{order_id} is new");
        order_ids.push(order_id);
    }
    order_ids
}

async fn deliver(webhook: Arc<Webhook<NoGateway>>, order_ids: Vec<String>, pace: Option<Duration>, now: DateTime<Utc>) -> Burst {
    let orders = order_ids.len();
    let started = Instant::now();

    let mut deliveries = JoinSet::new();
    for order_id in order_ids {
        let webhook = Arc::clone(&webhook);
        deliveries.spawn(async move {
            let began = Instant::now();
            let outcome = webhook.process(paid_event(&order_id), now).await;
            (outcome, began.elapsed())
        });
        if let Some(pace) = pace {
            tokio::time::sleep(pace).await;
        }
    }
    let outcomes = deliveries.join_all().await;
    let elapsed = started.elapsed();

    let allocated = outcomes.iter().filter(|(outcome, _)| matches!(outcome, Ok(Outcome::Allocated(_)))).count();
    let errored = outcomes.iter().filter(|(outcome, _)| outcome.is_err()).count();
    let mut latencies: Vec<Duration> = outcomes.into_iter().map(|(_, latency)| latency).collect();
    latencies.sort_unstable();

    Burst {
        orders,
        elapsed,
        allocated,
        errored,
        latencies,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "stress test; run with --ignored --nocapture, against DynamoDB Local or a real region"]
async fn webhook_throughput_on_one_raffle() {
    let Some(repo) = local_repo("stress").await else {
        return;
    };
    let webhook = Arc::new(Webhook::new(repo.clone(), NoGateway, "whsec_test"));
    let now = Utc::now();

    for orders in [40, 80, 160, 320] {
        let raffle = open_raffle(&repo, &format!("burst-{orders}"), now).await;
        let order_ids = pending_orders(&repo, &raffle, orders, now).await;

        deliver(Arc::clone(&webhook), order_ids, None, now).await.report(&format!("burst of {orders}"));
    }

    for per_second in [10, 25, 50, 100] {
        let raffle = open_raffle(&repo, &format!("paced-{per_second}"), now).await;
        let order_ids = pending_orders(&repo, &raffle, per_second * 5, now).await;
        let pace = Duration::from_secs_f64(1.0 / per_second as f64);

        deliver(Arc::clone(&webhook), order_ids, Some(pace), now)
            .await
            .report(&format!("paced at {per_second}/s for 5s"));
    }
}
