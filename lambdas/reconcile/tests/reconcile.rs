use chrono::{DateTime, Duration, Utc};
use reconcile::{Report, run};
use shared::order::{Allocation, Entry, Order, OrderStatus};
use shared::raffle::Raffle;
use shared::stripe::{Charge, OrderMetadata, PaymentGateway, StripeError};
use shared::subscription::Subscription;
use shared::table::DynamoRepo;
use shared::testing::{debit_payment, local_repo, raffle, seed_raffle, subscription};

struct Scripted(Vec<Charge>);

impl PaymentGateway for Scripted {
    async fn list_charges(&self, _created_from: i64, _created_to: i64) -> Result<Vec<Charge>, StripeError> {
        Ok(self.0.clone())
    }
}

fn no_charges() -> Scripted {
    Scripted(Vec::new())
}

fn charge(id: &str, order_id: Option<&str>) -> Charge {
    Charge {
        id: id.into(),
        status: "succeeded".into(),
        metadata: OrderMetadata {
            order_id: order_id.map(str::to_string),
        },
        ..Charge::default()
    }
}

fn counted(raffle_id: &str, opens_in_days: i64, closes_in_days: i64, tickets_sold: u64, now: DateTime<Utc>) -> Raffle {
    let mut raffle = raffle(raffle_id, opens_in_days, closes_in_days, now);
    raffle.tickets_sold = tickets_sold;
    raffle.ticket_revenue_pence = tickets_sold * raffle.ticket_price_pence;
    raffle
}

fn checks(report: &Report) -> Vec<(&str, &str)> {
    report.violations.iter().map(|found| (found.check, found.subject.as_str())).collect()
}

async fn pending_order(repo: &DynamoRepo, raffle: &Raffle, order_id: &str, quantity: u32, now: DateTime<Utc>) {
    let mut order = Order::single(order_id, raffle, &format!("ent-{order_id}"), quantity, 0, false, now);
    order.stripe_payment_intent_id = Some(format!("pi_{order_id}"));
    assert!(repo.create_order(&order).await.unwrap());
}

async fn paid_order(repo: &DynamoRepo, raffle: &Raffle, order_id: &str, quantity: u32, now: DateTime<Utc>) -> Entry {
    pending_order(repo, raffle, order_id, quantity, now).await;

    let allocation = repo
        .allocate_entry(order_id, &debit_payment(format!("pi_{order_id}"), None), now)
        .await
        .unwrap();
    let Allocation::Allocated(entry) = allocation else {
        panic!("expected tickets for {order_id}, got {allocation:?}");
    };
    entry
}

async fn failed_after_allocation(repo: &DynamoRepo, raffle: &Raffle, order_id: &str, allocated_at: DateTime<Utc>) -> Entry {
    let entry = paid_order(repo, raffle, order_id, 5, allocated_at).await;
    assert!(
        repo.set_order_status(order_id, OrderStatus::Paid, OrderStatus::Failed).await.unwrap(),
        "{order_id} was paid before it failed"
    );
    entry
}

async fn charged_subscriber(repo: &DynamoRepo, raffle: &Raffle, subscription_id: &str, now: DateTime<Utc>) {
    let subscriber = subscription(subscription_id, &format!("ent-{subscription_id}"), now);
    repo.put_subscription(&subscriber).await.unwrap();

    let mut order = Order::from_subscription(raffle, &subscriber, now);
    order.stripe_payment_intent_id = Some(format!("pi_sub_{subscription_id}"));
    assert!(repo.create_order(&order).await.unwrap());
}

async fn uncharged_subscriber(repo: &DynamoRepo, subscription_id: &str, now: DateTime<Utc>) -> Subscription {
    let subscriber = subscription(subscription_id, &format!("ent-{subscription_id}"), now);
    repo.put_subscription(&subscriber).await.unwrap();
    subscriber
}

#[tokio::test]
async fn a_healthy_raffle_reports_only_the_missed_webhook_and_the_uncharged_subscriber() {
    let Some(repo) = local_repo("reconcile-test").await else {
        return;
    };
    let now = Utc::now();
    let open = raffle("winter", -2, 100, now);
    seed_raffle(&repo, &open).await;

    paid_order(&repo, &open, "ord-1", 15, now).await;
    paid_order(&repo, &open, "ord-2", 10, now).await;
    pending_order(&repo, &open, "ord-missed", 5, now).await;

    charged_subscriber(&repo, &open, "charged", now).await;
    let late = uncharged_subscriber(&repo, "late", now).await;

    let charges = vec![
        charge("ch_1", Some("ord-1")),
        charge("ch_missed", Some("ord-missed")),
        charge("ch_dashboard", None),
    ];
    let report = run(&repo, &Scripted(charges), now).await.unwrap();

    assert_eq!(report.raffles_checked, 1);
    assert_eq!(report.entries_checked, 2, "only the two paid orders have ledger rows");
    assert_eq!(report.charges_checked, 3, "the dashboard charge carries no orderId and is still counted");
    assert_eq!(report.subscriptions_checked, 2, "both subscribers became eligible before this raffle opened");
    assert_eq!(
        checks(&report),
        vec![
            ("subscription-uncharged", late.order_id_for(&open.raffle_id).as_str()),
            ("charge-order", "ord-missed")
        ],
        "nothing else in a healthy raffle is flagged"
    );
}

#[tokio::test]
async fn a_closed_raffle_whose_ledger_falls_short_is_reported_and_the_stale_one_is_skipped() {
    let Some(repo) = local_repo("reconcile-test").await else {
        return;
    };
    let now = Utc::now();

    let short = counted("autumn", -200, -100, 5, now);
    seed_raffle(&repo, &short).await;

    let mut ancient = counted("spring", -400, -300, 5, now);
    ancient.drawn_at = Some(now - Duration::days(200));
    seed_raffle(&repo, &ancient).await;

    let report = run(&repo, &no_charges(), now).await.unwrap();

    assert_eq!(report.raffles_checked, 1, "of the two rows, only the one drawn 200 days ago is out of scope");
    assert_eq!(report.entries_checked, 0, "the counter claims five tickets the ledger never recorded");
    assert_eq!(checks(&report), vec![("ledger-short", short.raffle_id.as_str())]);
}

#[tokio::test]
async fn only_entries_inside_the_lookback_window_are_matched_to_their_order() {
    let Some(repo) = local_repo("reconcile-test").await else {
        return;
    };
    let now = Utc::now();
    let open = raffle("winter", -2, 100, now);
    seed_raffle(&repo, &open).await;

    let stale = failed_after_allocation(&repo, &open, "ord-stale", now - Duration::days(3)).await;
    let recent = failed_after_allocation(&repo, &open, "ord-recent", now).await;

    let report = run(&repo, &no_charges(), now).await.unwrap();

    assert_eq!(report.entries_checked, 2, "every entry is walked for gaps, however old");
    let found = checks(&report);
    let [(check, subject)] = found.as_slice() else {
        panic!("expected only the recent entry to be reported, got {found:?}");
    };
    assert_eq!(*check, "entry-unpaid", "the order behind the recent entry is FAILED");
    assert!(
        subject.ends_with(&format!("{:08}", recent.ticket_from)),
        "ticket {} is inside the window and ticket {} is not: {subject}",
        recent.ticket_from,
        stale.ticket_from
    );
}
