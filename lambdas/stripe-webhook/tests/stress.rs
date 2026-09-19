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

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const TICKETS_DEADLINE: Duration = Duration::from_secs(120);
const STRIPE_RETRIES: u32 = 8;

struct Site {
    base: String,
    publishable_key: String,
    raffle_id: String,
    http: reqwest::Client,
}

struct Sale {
    checkout: Result<Duration, String>,
    confirm: Result<Duration, String>,
    tickets: Result<Duration, String>,
    poll_errors: u32,
}

struct Run {
    label: String,
    elapsed: Duration,
    sales: Vec<Sale>,
}

fn percentile(samples: &mut [Duration], hundredths: usize) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    samples.sort_unstable();
    samples[(samples.len() * hundredths / 100).min(samples.len() - 1)]
}

fn stage(label: &str, results: Vec<&Result<Duration, String>>) -> String {
    let mut ok: Vec<Duration> = results.iter().filter_map(|result| result.as_ref().ok().copied()).collect();
    let failures: Vec<&String> = results.iter().filter_map(|result| result.as_ref().err()).collect();
    let first_failure = failures.first().map(|failure| format!(" e.g. {failure}")).unwrap_or_default();

    format!(
        "{label}: {} ok, {} failed{first_failure}; p50 {:?} p95 {:?} max {:?}",
        ok.len(),
        failures.len(),
        percentile(&mut ok, 50),
        percentile(&mut ok, 95),
        ok.iter().max().copied().unwrap_or_default()
    )
}

impl Run {
    fn report(&self) {
        let offered = self.sales.len();
        let poll_errors: u32 = self.sales.iter().map(|sale| sale.poll_errors).sum();

        println!(
            "{}: {offered} sales offered in {:.1}s, {poll_errors} poll errors",
            self.label,
            self.elapsed.as_secs_f64()
        );
        println!("    {}", stage("checkout", self.sales.iter().map(|sale| &sale.checkout).collect()));
        println!("    {}", stage("stripe confirm", self.sales.iter().map(|sale| &sale.confirm).collect()));
        println!("    {}", stage("tickets visible", self.sales.iter().map(|sale| &sale.tickets).collect()));
    }
}

async fn json_of(response: reqwest::Response) -> Result<serde_json::Value, String> {
    let text = response.text().await.map_err(|err| err.to_string())?;
    serde_json::from_str(&text).map_err(|err| err.to_string())
}

impl Site {
    async fn discover(base: String) -> Self {
        let http = reqwest::Client::new();
        let config = http.get(format!("{base}/config.js")).send().await.unwrap().text().await.unwrap();
        let publishable_key = config
            .split("stripePublishableKey: \"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("config.js names the publishable key")
            .to_string();
        let current = json_of(http.get(format!("{base}/api/raffles/current")).send().await.unwrap()).await.unwrap();
        let raffle_id = current["current"]["raffleId"].as_str().expect("an open raffle").to_string();

        Self {
            base,
            publishable_key,
            raffle_id,
            http,
        }
    }

    async fn checkout(&self, email: &str) -> Result<(String, String), String> {
        let body = json!({
            "ticketQuantity": 1, "donationPence": 0, "giftAid": false, "subscribe": false,
            "entrant": {
                "title": "Mx", "firstName": "Stress", "lastName": "Test", "email": email, "dateOfBirth": "1980-06-23",
                "address": { "line1": "1 Load Lane", "town": "Milton Keynes", "postcode": "MK3 6EB", "country": "GB" }
            },
            "marketing": { "email": false, "post": false }
        });
        let response = self
            .http
            .post(format!("{}/api/raffles/{}/orders", self.base, self.raffle_id))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|err| err.to_string())?;
        let status = response.status();
        let created = json_of(response).await.map_err(|err| format!("checkout {status}: {err}"))?;
        if !status.is_success() {
            return Err(format!("checkout {status}: {created}"));
        }

        let field = |name: &str| {
            created[name]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("checkout answered without {name}"))
        };
        Ok((field("orderId")?, field("clientSecret")?))
    }

    async fn confirm(&self, client_secret: &str) -> Result<(), String> {
        let intent_id = client_secret.split("_secret_").next().unwrap_or_default();

        for attempt in 1..=STRIPE_RETRIES {
            let response = self
                .http
                .post(format!("https://api.stripe.com/v1/payment_intents/{intent_id}/confirm"))
                .basic_auth(&self.publishable_key, None::<&str>)
                .form(&[
                    ("client_secret", client_secret),
                    ("payment_method", "pm_card_visa_debit"),
                    ("return_url", &self.base),
                ])
                .send()
                .await
                .map_err(|err| err.to_string())?;
            let status = response.status();
            if status.as_u16() == 429 {
                tokio::time::sleep(Duration::from_millis(250 * u64::from(attempt))).await;
                continue;
            }

            let body = json_of(response).await.map_err(|err| format!("confirm {status}: {err}"))?;
            if !status.is_success() {
                return Err(format!("confirm {status}: {}", body["error"]["message"]));
            }
            return match body["status"].as_str() {
                Some("succeeded") => Ok(()),
                other => Err(format!("intent status {other:?}")),
            };
        }
        Err(format!("stripe rate limited {STRIPE_RETRIES} times"))
    }

    async fn await_tickets(&self, order_id: &str) -> (Result<Duration, String>, u32) {
        let started = Instant::now();
        let mut poll_errors = 0;

        while started.elapsed() < TICKETS_DEADLINE {
            match self.http.get(format!("{}/api/orders/{order_id}", self.base)).send().await {
                Ok(response) if response.status().is_success() => {
                    let order = json_of(response).await.unwrap_or_default();
                    if order["tickets"].is_object() {
                        return (Ok(started.elapsed()), poll_errors);
                    }
                    if order["status"] == "FAILED" {
                        return (Err("order failed".into()), poll_errors);
                    }
                }
                _ => poll_errors += 1,
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        (Err(format!("no tickets within {TICKETS_DEADLINE:?}")), poll_errors)
    }

    async fn sale(&self, email: &str) -> Sale {
        let began = Instant::now();
        let (order_id, client_secret) = match self.checkout(email).await {
            Ok(created) => created,
            Err(err) => {
                return Sale {
                    checkout: Err(err),
                    confirm: Err("not attempted".into()),
                    tickets: Err("not attempted".into()),
                    poll_errors: 0,
                };
            }
        };
        let checkout = Ok(began.elapsed());

        let began = Instant::now();
        if let Err(err) = self.confirm(&client_secret).await {
            return Sale {
                checkout,
                confirm: Err(err),
                tickets: Err("not attempted".into()),
                poll_errors: 0,
            };
        }
        let confirm = Ok(began.elapsed());

        let (tickets, poll_errors) = self.await_tickets(&order_id).await;
        Sale {
            checkout,
            confirm,
            tickets,
            poll_errors,
        }
    }
}

async fn offer(site: &Arc<Site>, label: &str, sales: usize, per_second: u64) -> Run {
    let started = Instant::now();
    let run_id = shared::random::id("stress");

    let mut deliveries = JoinSet::new();
    for i in 1..=sales {
        let site = Arc::clone(site);
        let email = format!("{run_id}-{i}@example.com");
        deliveries.spawn(async move { site.sale(&email).await });
        tokio::time::sleep(Duration::from_millis(1_000 / per_second)).await;
    }
    let sales = deliveries.join_all().await;

    Run {
        label: label.into(),
        elapsed: started.elapsed(),
        sales,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "drives a deployment's checkout through Stripe test mode; set STRESS_BASE_URL"]
async fn checkout_throughput_against_a_deployment() {
    let Ok(base) = std::env::var("STRESS_BASE_URL") else {
        eprintln!("skipping: STRESS_BASE_URL not set");
        return;
    };
    let site = Arc::new(Site::discover(base.trim_end_matches('/').to_string()).await);
    println!("driving {} raffle {} through Stripe test mode", site.base, site.raffle_id);

    for (sales, per_second) in [(20, 2), (40, 5), (80, 10)] {
        offer(&site, &format!("{sales} sales offered at {per_second}/s"), sales, per_second)
            .await
            .report();
    }
}
