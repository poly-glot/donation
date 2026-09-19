use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use shared::error::AppError;
use shared::order::{Entry, Order, OrderStatus};
use shared::raffle::{Raffle, RaffleStatus};
use shared::stripe::{Charge, PaymentGateway};
use shared::subscription::Subscription;
use shared::table::DynamoRepo;
use shared::telemetry;

const LOOKBACK_HOURS: i64 = 48;
const SETTLE_MINUTES: i64 = 60;
const SUBSCRIPTION_GRACE_HOURS: i64 = 24;
const RECENT_DRAW_DAYS: i64 = 30;
const ENTRY_PAGE_SIZE: i32 = 1_000;
const SUCCEEDED: &str = "succeeded";

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Violation {
    pub check: &'static str,
    pub subject: String,
    pub detail: String,
}

fn violation(check: &'static str, subject: impl Into<String>, detail: impl Into<String>) -> Violation {
    Violation {
        check,
        subject: subject.into(),
        detail: detail.into(),
    }
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub raffles_checked: u32,
    pub entries_checked: u64,
    pub charges_checked: u32,
    pub subscriptions_checked: u32,
    pub violations: Vec<Violation>,
}

pub fn is_live(raffle: &Raffle, now: DateTime<Utc>) -> bool {
    raffle.drawn_at.is_none_or(|drawn_at| now - drawn_at < Duration::days(RECENT_DRAW_DAYS))
}

pub fn subscriptions_due_by_now(raffle: &Raffle, now: DateTime<Utc>) -> bool {
    raffle.status_at(now) == RaffleStatus::Open && now - raffle.opens_at >= Duration::hours(SUBSCRIPTION_GRACE_HOURS)
}

pub fn raffle_violations(raffle: &Raffle) -> Vec<Violation> {
    let mut found = Vec::new();

    let expected_revenue = raffle.tickets_sold * raffle.ticket_price_pence;
    if raffle.ticket_revenue_pence != expected_revenue {
        let detail = format!(
            "ticketRevenuePence {} but {} tickets at {}p",
            raffle.ticket_revenue_pence, raffle.tickets_sold, raffle.ticket_price_pence
        );
        found.push(violation("revenue", &raffle.raffle_id, detail));
    }
    if raffle.tickets_sold > raffle.max_tickets {
        found.push(violation(
            "cap",
            &raffle.raffle_id,
            format!("ticketsSold {} exceeds maxTickets {}", raffle.tickets_sold, raffle.max_tickets),
        ));
    }
    found
}

#[derive(Debug)]
pub struct LedgerWalk {
    next_ticket: u64,
    last_ticket: u64,
}

impl Default for LedgerWalk {
    fn default() -> Self {
        Self {
            next_ticket: 1,
            last_ticket: 0,
        }
    }
}

impl LedgerWalk {
    pub fn step(&mut self, entry: &Entry) -> Option<Violation> {
        let contiguous = entry.ticket_from == self.next_ticket && entry.ticket_to >= entry.ticket_from;
        let expected = self.next_ticket;
        self.last_ticket = entry.ticket_to;
        self.next_ticket = entry.ticket_to + 1;

        if contiguous {
            return None;
        }
        let subject = format!("{}#{:08}", entry.raffle_id, entry.ticket_from);
        Some(violation(
            "ledger-gap",
            subject,
            format!("expected ticketFrom {expected}, found {}..{}", entry.ticket_from, entry.ticket_to),
        ))
    }

    pub fn finish(&self, raffle: &Raffle, run: Option<u32>, sold: u64, now: DateTime<Utc>) -> Option<Violation> {
        let subject = match run {
            None => raffle.raffle_id.clone(),
            Some(shard) => format!("{}#{shard:02}", raffle.raffle_id),
        };
        let detail = format!("entries reach {} but ticketsSold is {sold}", self.last_ticket);

        if self.last_ticket > sold {
            return Some(violation("ledger-overrun", subject, detail));
        }
        let short = self.last_ticket < sold && raffle.status_at(now) != RaffleStatus::Open;
        short.then(|| violation("ledger-short", subject, detail))
    }
}

fn entry_order_violation(entry: &Entry, order: Option<&Order>) -> Option<Violation> {
    let subject = format!("{}#{:08}", entry.raffle_id, entry.ticket_from);
    match order {
        None => Some(violation("entry-orphan", subject, format!("order {} does not exist", entry.order_id))),
        Some(order) if matches!(order.status, OrderStatus::Paid | OrderStatus::Refunded) => None,
        Some(order) => Some(violation("entry-unpaid", subject, format!("order {} is {:?}", order.order_id, order.status))),
    }
}

pub fn charge_violation(charge: &Charge, order: Option<&Order>) -> Option<Violation> {
    if charge.status != SUCCEEDED {
        return None;
    }
    let order_id = charge.metadata.order_id.as_deref()?;
    let Some(order) = order else {
        return Some(violation("charge-orphan", order_id, format!("charge {} has no order", charge.id)));
    };

    let settled = if charge.refunded {
        matches!(order.status, OrderStatus::Failed | OrderStatus::Refunded)
    } else {
        order.status == OrderStatus::Paid
    };
    if settled {
        return None;
    }

    let detail = match (charge.refunded, order.status) {
        (false, OrderStatus::Pending) => format!("charge {} succeeded but order is still PENDING: missed webhook", charge.id),
        (false, status) => format!("charge {} succeeded but order is {status:?}", charge.id),
        (true, status) => format!("charge {} was refunded but order is {status:?}", charge.id),
    };
    Some(violation("charge-order", order_id, detail))
}

fn subscription_violation(subscription: &Subscription, raffle: &Raffle, order: Option<&Order>) -> Option<Violation> {
    let subject = subscription.order_id_for(&raffle.raffle_id);
    match order {
        None => Some(violation("subscription-uncharged", subject, "no order for this raffle")),
        Some(order) if order.stripe_payment_intent_id.is_none() => Some(violation("subscription-uncharged", subject, "order has no PaymentIntent")),
        Some(_) => None,
    }
}

pub async fn run<G: PaymentGateway>(repo: &DynamoRepo, gateway: &G, now: DateTime<Utc>) -> Result<Report, AppError> {
    let since = now - Duration::hours(LOOKBACK_HOURS);
    let mut report = Report::default();

    let raffles = repo.list_raffles().await?;
    for raffle in raffles.into_iter().filter(|raffle| is_live(raffle, now)) {
        let raffle = repo.with_totals(raffle).await?;
        report.raffles_checked += 1;
        report.violations.extend(raffle_violations(&raffle));
        check_ledger(repo, &raffle, since, now, &mut report).await?;
        if subscriptions_due_by_now(&raffle, now) {
            check_subscriptions(repo, &raffle, &mut report).await?;
        }
    }
    check_charges(repo, gateway, since, now - Duration::minutes(SETTLE_MINUTES), &mut report).await?;

    for found in &report.violations {
        tracing::error!(check = found.check, subject = %found.subject, detail = %found.detail, "integrity violation");
    }
    telemetry::emit(&[("IntegrityViolations", report.violations.len() as f64)], &[], &[]);
    tracing::info!(
        report.raffles_checked,
        report.entries_checked,
        report.charges_checked,
        report.subscriptions_checked,
        violations = report.violations.len(),
        "reconciliation finished"
    );
    Ok(report)
}

fn runs_of(raffle: &Raffle) -> Vec<Option<u32>> {
    match raffle.shards {
        None => vec![None],
        Some(shards) => (0..shards).map(Some).collect(),
    }
}

async fn walk_run(repo: &DynamoRepo, raffle: &Raffle, run: Option<u32>, since: DateTime<Utc>, report: &mut Report) -> Result<LedgerWalk, AppError> {
    let mut walk = LedgerWalk::default();
    let mut start = None;

    loop {
        let (entries, next) = repo.list_entries(&raffle.raffle_id, run, ENTRY_PAGE_SIZE, start).await?;
        for entry in &entries {
            report.entries_checked += 1;
            report.violations.extend(walk.step(entry));
            if entry.allocated_at >= since {
                let order = repo.get_order(&entry.order_id).await?;
                report.violations.extend(entry_order_violation(entry, order.as_ref()));
            }
        }
        let Some(key) = next else { break };
        start = Some(key);
    }

    Ok(walk)
}

async fn check_ledger(repo: &DynamoRepo, raffle: &Raffle, since: DateTime<Utc>, now: DateTime<Utc>, report: &mut Report) -> Result<(), AppError> {
    let runs = runs_of(raffle);
    let mut walks = Vec::with_capacity(runs.len());
    for run in &runs {
        walks.push(walk_run(repo, raffle, *run, since, report).await?);
    }

    let current = repo.get_raffle(&raffle.raffle_id).await?.unwrap_or_else(|| raffle.clone());
    let counters = repo.counters(&current).await?;
    for (run, walk) in runs.iter().zip(&walks) {
        let sold = match run {
            None => current.tickets_sold,
            Some(shard) => counters[*shard as usize].sold,
        };
        report.violations.extend(walk.finish(&current, *run, sold, now));
    }

    Ok(())
}

async fn check_subscriptions(repo: &DynamoRepo, raffle: &Raffle, report: &mut Report) -> Result<(), AppError> {
    for subscription in &repo.due_subscriptions(raffle).await? {
        report.subscriptions_checked += 1;
        let order = repo.get_order(&subscription.order_id_for(&raffle.raffle_id)).await?;
        report.violations.extend(subscription_violation(subscription, raffle, order.as_ref()));
    }
    Ok(())
}

async fn check_charges<G: PaymentGateway>(repo: &DynamoRepo, gateway: &G, from: DateTime<Utc>, to: DateTime<Utc>, report: &mut Report) -> Result<(), AppError> {
    let charges = gateway.list_charges(from.timestamp(), to.timestamp()).await?;

    for charge in &charges {
        report.charges_checked += 1;
        let order = match charge.metadata.order_id.as_deref() {
            Some(order_id) => repo.get_order(order_id).await?,
            None => None,
        };
        report.violations.extend(charge_violation(charge, order.as_ref()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::stripe::OrderMetadata;
    use shared::testing::{at, subscription, winter};

    fn raffle(tickets_sold: u64) -> Raffle {
        let mut raffle = winter();
        raffle.tickets_sold = tickets_sold;
        raffle.ticket_revenue_pence = tickets_sold * 100;
        raffle
    }

    fn entry(from: u64, to: u64) -> Entry {
        Entry {
            raffle_id: "winter-2026".into(),
            order_id: format!("ord-{from}"),
            entrant_id: "ent-1".into(),
            shard: None,
            ticket_from: from,
            ticket_to: to,
            allocated_at: at(2026, 10, 1),
        }
    }

    fn order(status: OrderStatus) -> Order {
        let mut order = Order::single("ord-1", &raffle(0), "ent-1", 5, 0, false, at(2026, 10, 1));
        order.status = status;
        order
    }

    fn charge(order_id: Option<&str>, status: &str, refunded: bool) -> Charge {
        Charge {
            id: "ch_1".into(),
            status: status.into(),
            metadata: OrderMetadata {
                order_id: order_id.map(str::to_string),
            },
            refunded,
            ..Charge::default()
        }
    }

    #[test]
    fn ledger_walk_accepts_contiguous_ranges_and_reports_gaps_overruns_and_shortfalls() {
        let mut walk = LedgerWalk::default();
        assert!(walk.step(&entry(1, 15)).is_none());
        assert!(walk.step(&entry(16, 25)).is_none());
        assert_eq!(walk.step(&entry(30, 34)).map(|found| found.check), Some("ledger-gap"));
        assert_eq!(walk.finish(&raffle(34), None, 34, at(2026, 10, 1)), None);
        assert_eq!(
            walk.finish(&raffle(30), None, 30, at(2026, 10, 1)).map(|found| found.check),
            Some("ledger-overrun")
        );

        let open = at(2026, 10, 1);
        let closed = at(2027, 1, 9);
        assert_eq!(walk.finish(&raffle(40), None, 40, open), None);
        assert_eq!(walk.finish(&raffle(40), None, 40, closed).map(|found| found.check), Some("ledger-short"));
    }

    #[test]
    fn charges_must_match_the_order_state_stripe_implies() {
        let cases = [
            (
                "no metadata is skipped",
                charge(None, "succeeded", false),
                Some(order(OrderStatus::Pending)),
                None,
            ),
            (
                "unsettled charges are skipped",
                charge(Some("ord-1"), "pending", false),
                Some(order(OrderStatus::Pending)),
                None,
            ),
            (
                "paid order matches",
                charge(Some("ord-1"), "succeeded", false),
                Some(order(OrderStatus::Paid)),
                None,
            ),
            (
                "pending order is a missed webhook",
                charge(Some("ord-1"), "succeeded", false),
                Some(order(OrderStatus::Pending)),
                Some("charge-order"),
            ),
            (
                "refunded charge with failed order matches",
                charge(Some("ord-1"), "succeeded", true),
                Some(order(OrderStatus::Failed)),
                None,
            ),
            (
                "refunded charge with paid order is wrong",
                charge(Some("ord-1"), "succeeded", true),
                Some(order(OrderStatus::Paid)),
                Some("charge-order"),
            ),
            ("missing order", charge(Some("ord-1"), "succeeded", false), None, Some("charge-orphan")),
        ];
        for (label, charge, order, expected) in cases {
            assert_eq!(charge_violation(&charge, order.as_ref()).map(|found| found.check), expected, "{label}");
        }
    }

    #[test]
    fn an_entry_must_point_at_an_order_that_kept_its_tickets() {
        let cases = [
            ("no order behind the tickets", None, Some("entry-orphan")),
            ("a paid order", Some(order(OrderStatus::Paid)), None),
            (
                "a refunded order keeps the ledger row it was allocated",
                Some(order(OrderStatus::Refunded)),
                None,
            ),
            (
                "tickets allocated against a pending order",
                Some(order(OrderStatus::Pending)),
                Some("entry-unpaid"),
            ),
            (
                "tickets allocated against a failed order",
                Some(order(OrderStatus::Failed)),
                Some("entry-unpaid"),
            ),
        ];
        for (label, order, expected) in cases {
            let found = entry_order_violation(&entry(1, 15), order.as_ref());
            assert_eq!(found.map(|found| found.check), expected, "{label}");
        }
    }

    #[test]
    fn a_due_subscriber_must_have_an_order_stripe_was_asked_to_charge() {
        let spring = raffle(0);
        let subscriber = subscription("sub-1", "ent-1", at(2026, 10, 1));
        let uncharged = Order::from_subscription(&spring, &subscriber, at(2026, 10, 1));
        let mut charged = uncharged.clone();
        charged.stripe_payment_intent_id = Some("pi_1".into());

        let cases = [
            ("no order for this raffle at all", None, Some("subscription-uncharged")),
            (
                "an order the charge run never got a PaymentIntent for",
                Some(uncharged),
                Some("subscription-uncharged"),
            ),
            ("an order carrying the intent Stripe returned", Some(charged), None),
        ];
        for (label, order, expected) in cases {
            let found = subscription_violation(&subscriber, &spring, order.as_ref());
            assert_eq!(found.map(|found| found.check), expected, "{label}");
        }
    }

    #[test]
    fn raffle_totals_and_liveness_follow_the_row() {
        let mut drifted = raffle(10);
        drifted.ticket_revenue_pence = 900;
        drifted.max_tickets = 5;
        let checks: Vec<&str> = raffle_violations(&drifted).iter().map(|found| found.check).collect();
        assert_eq!(checks, vec!["revenue", "cap"]);
        assert!(raffle_violations(&raffle(10)).is_empty());

        let mut old = raffle(10);
        old.drawn_at = Some(at(2027, 1, 22));
        assert!(is_live(&old, at(2027, 2, 1)));
        assert!(!is_live(&old, at(2027, 4, 1)));
        assert!(subscriptions_due_by_now(&raffle(0), at(2026, 10, 2)));
        assert!(!subscriptions_due_by_now(&raffle(0), at(2026, 9, 30)));
    }
}
