//! Orders, the ticket ledger, and the transaction that turns a payment into
//! gapless ticket numbers.
//!
//! An order is what Stripe charges; an [`Entry`] is the range of ticket numbers a
//! paid order was awarded. Tickets are not scarce inventory — the only ceiling is
//! the licence proceeds cap — so nothing is ever reserved or held. Numbers are
//! handed out only on payment, and they must be contiguous and gap-free across a
//! raffle, which is the whole difficulty this module exists to solve.
//!
//! [`DynamoRepo::allocate_entry`] does it with one `TransactWriteItems` that moves
//! the raffle counter, writes the ledger row and flips the order to `PAID`, all
//! conditioned so the write is safe under races and idempotent under Stripe's
//! at-least-once webhooks: the counter update is a compare-and-set on the value
//! just read (a lost race retries with backoff), and a replayed webhook trips the
//! order's `PENDING` condition and returns [`Allocation::AlreadyPaid`] without
//! burning numbers. Ticket money and donation money are kept in separate columns
//! throughout, because only the donation is Gift Aid eligible and the two report
//! to different regulators.

use std::time::Duration;

use aws_sdk_dynamodb::types::{Put, TransactWriteItem, Update};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_dynamo::aws_sdk_dynamodb_1::to_attribute_value;

use crate::entrant::{Entrant, entrant_pk};
use crate::error::{AppError, CONDITIONAL_CHECK_FAILED};
use crate::raffle::{Raffle, RaffleStatus, raffle_pk};
use crate::subscription::Subscription;
use crate::table::{DynamoRepo, GSI2, METADATA_SK, PageKey, condition_failed_as_false, item, n, partition, s, sort_ts};

const ALLOCATION_ATTEMPTS: u32 = 10;
const ALLOCATION_BACKOFF_STEP: Duration = Duration::from_millis(25);
const RETRYABLE_CANCELLATIONS: [&str; 3] = ["TransactionConflict", "ThrottlingError", "ProvisionedThroughputExceeded"];

pub(crate) fn order_pk(order_id: &str) -> String {
    partition("ORDER", order_id)
}

fn entry_sk(ticket_from: u64) -> String {
    format!("ENTRY#{ticket_from:08}")
}

fn entrant_order_gsi1sk(created_at: DateTime<Utc>) -> String {
    format!("ORDER#{}", sort_ts(created_at))
}

fn entrant_entry_gsi1sk(raffle_id: &str, ticket_from: u64) -> String {
    format!("ENTRY#{raffle_id}#{ticket_from:08}")
}

/// The order's `GSI2PK`, present once a PaymentIntent exists, so the webhook can
/// find the order Stripe is telling us about by its payment id.
pub(crate) fn payment_intent_gsi2pk(payment_intent_id: &str) -> String {
    partition("PI", payment_intent_id)
}

fn order_keys(order: &Order) -> Vec<(&'static str, String)> {
    let mut keys = vec![
        ("PK", order_pk(&order.order_id)),
        ("SK", METADATA_SK.into()),
        ("GSI1PK", entrant_pk(&order.entrant_id)),
        ("GSI1SK", entrant_order_gsi1sk(order.created_at)),
    ];
    if let Some(payment_intent_id) = &order.stripe_payment_intent_id {
        keys.push(("GSI2PK", payment_intent_gsi2pk(payment_intent_id)));
    }
    keys
}

fn entry_keys(entry: &Entry) -> Vec<(&'static str, String)> {
    vec![
        ("PK", raffle_pk(&entry.raffle_id)),
        ("SK", entry_sk(entry.ticket_from)),
        ("GSI1PK", entrant_pk(&entry.entrant_id)),
        ("GSI1SK", entrant_entry_gsi1sk(&entry.raffle_id, entry.ticket_from)),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderStatus {
    Pending,
    Paid,
    Failed,
    Refunded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Order {
    pub order_id: String,
    pub raffle_id: String,
    pub entrant_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_id: Option<String>,
    pub ticket_quantity: u32,
    pub ticket_amount_pence: u64,
    pub donation_pence: u64,
    pub total_pence: u64,
    pub gift_aid: bool,
    #[serde(default)]
    pub subscribe: bool,
    pub status: OrderStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stripe_payment_intent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card_funding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card_last4: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paid_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl Order {
    /// A single online purchase: tickets at the raffle's price, plus an optional
    /// donation. Ticket money and donation money are computed apart and only summed
    /// into `total_pence` — the amount Stripe charges.
    pub fn single(
        order_id: impl Into<String>,
        raffle: &Raffle,
        entrant_id: &str,
        ticket_quantity: u32,
        donation_pence: u64,
        gift_aid: bool,
        now: DateTime<Utc>,
    ) -> Self {
        let ticket_amount_pence = raffle.ticket_price_pence * u64::from(ticket_quantity);

        Self {
            order_id: order_id.into(),
            raffle_id: raffle.raffle_id.clone(),
            entrant_id: entrant_id.into(),
            subscription_id: None,
            ticket_quantity,
            ticket_amount_pence,
            donation_pence,
            total_pence: ticket_amount_pence + donation_pence,
            gift_aid,
            subscribe: false,
            status: OrderStatus::Pending,
            stripe_payment_intent_id: None,
            card_funding: None,
            card_last4: None,
            paid_at: None,
            created_at: now,
        }
    }

    /// A subscriber's per-raffle order. Its id is deterministic in
    /// `(subscription, raffle)` so the DynamoDB conditional put and the Stripe
    /// idempotency key together make the charge exactly-once.
    pub fn from_subscription(raffle: &Raffle, subscription: &Subscription, now: DateTime<Utc>) -> Self {
        let order_id = subscription.order_id_for(&raffle.raffle_id);
        let mut order = Self::single(order_id, raffle, &subscription.entrant_id, subscription.tickets_per_raffle, 0, false, now);
        order.subscription_id = Some(subscription.subscription_id.clone());
        order
    }
}

/// The licence rules that must hold before we take a penny: the raffle is open,
/// the quantity is within the per-order cap, and the entrant is an adult resident
/// of Great Britain who has not self-excluded. Checked before anything is written.
pub fn validate_purchase(raffle: &Raffle, entrant: &Entrant, ticket_quantity: u32, now: DateTime<Utc>) -> Result<(), AppError> {
    let today = now.date_naive();

    if raffle.status_at(now) != RaffleStatus::Open {
        return Err(AppError::Conflict("raffle is not open".into()));
    }
    if ticket_quantity == 0 || ticket_quantity > raffle.max_tickets_per_order {
        return Err(AppError::BadRequest(format!(
            "ticketQuantity must be between 1 and {}",
            raffle.max_tickets_per_order
        )));
    }
    if !entrant.is_adult_on(today) {
        return Err(AppError::Forbidden("entrants must be 18 or over".into()));
    }
    if !entrant.address.is_great_britain() {
        return Err(AppError::Forbidden("entrants must be resident in Great Britain".into()));
    }
    if entrant.is_self_excluded_on(today) {
        return Err(AppError::Forbidden("entrant is self-excluded".into()));
    }
    Ok(())
}

/// The card facts the webhook lifts from a successful charge and the allocation
/// stamps onto the order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaidPayment {
    pub payment_intent_id: String,
    pub card_funding: Option<String>,
    pub card_last4: Option<String>,
}

/// One ledger row per paid order: the contiguous ticket range it was awarded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub raffle_id: String,
    pub order_id: String,
    pub entrant_id: String,
    pub ticket_from: u64,
    pub ticket_to: u64,
    pub allocated_at: DateTime<Utc>,
}

impl Entry {
    pub fn contains(&self, ticket_number: u64) -> bool {
        (self.ticket_from..=self.ticket_to).contains(&ticket_number)
    }
}

/// The next range for a purchase of `ticket_quantity`, given how many tickets the
/// raffle had already sold. Ranges are 1-based and butt exactly against each other.
pub(crate) fn ticket_range(tickets_sold_before: u64, ticket_quantity: u32) -> (u64, u64) {
    (tickets_sold_before + 1, tickets_sold_before + u64::from(ticket_quantity))
}

/// The three ways an allocation attempt can end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Allocation {
    Allocated(Entry),
    AlreadyPaid,
    SoldOut,
}

/// Why the allocation transaction was cancelled tells us how to react: a lost
/// order condition means someone else already paid it (idempotent replay); a lost
/// counter or ledger condition, a concurrent transaction or a throttle means the
/// attempt made no progress and should back off, re-read and retry.
#[derive(Debug, PartialEq, Eq)]
enum AllocationConflict {
    OrderNotPending,
    Retryable,
}

fn jittered(cap: Duration) -> Duration {
    cap.mul_f64(crate::random::u64() as f64 / u64::MAX as f64)
}

fn allocation_conflict(err: &AppError) -> Option<AllocationConflict> {
    let AppError::Dynamo(dynamo_err) = err else {
        return None;
    };
    let aws_sdk_dynamodb::Error::TransactionCanceledException(cancelled) = &**dynamo_err else {
        return None;
    };
    let reasons = cancelled.cancellation_reasons();

    if reasons
        .iter()
        .any(|reason| reason.code().is_some_and(|code| RETRYABLE_CANCELLATIONS.contains(&code)))
    {
        return Some(AllocationConflict::Retryable);
    }

    // The reasons come back positionally, one per transaction item, in the order
    // we submitted them: [raffle counter, entry ledger, order status].
    let failed: Vec<bool> = reasons.iter().map(|reason| reason.code() == Some(CONDITIONAL_CHECK_FAILED)).collect();

    match failed.as_slice() {
        [_, _, true] => Some(AllocationConflict::OrderNotPending),
        [true, _, _] | [_, true, _] => Some(AllocationConflict::Retryable),
        _ => None,
    }
}

impl DynamoRepo {
    pub async fn create_order(&self, order: &Order) -> Result<bool, AppError> {
        condition_failed_as_false(self.put(order, &order_keys(order), Some("attribute_not_exists(PK)")).await)
    }

    pub async fn get_order(&self, order_id: &str) -> Result<Option<Order>, AppError> {
        self.get(order_pk(order_id), METADATA_SK, false).await
    }

    pub async fn find_order_by_payment_intent(&self, payment_intent_id: &str) -> Result<Option<Order>, AppError> {
        self.query(
            Some(GSI2),
            "GSI2PK = :pk",
            vec![(":pk", s(payment_intent_gsi2pk(payment_intent_id)))],
            false,
            Some(1),
            None,
        )
        .await?
        .first()
    }

    pub async fn list_orders_for_entrant(&self, entrant_id: &str) -> Result<Vec<Order>, AppError> {
        self.query_entrant_index(entrant_id, "ORDER#").await?.all()
    }

    /// Attach a PaymentIntent to an existing order (and its GSI2 lookup key). Used
    /// by the subscription-charge run, which creates the order before it charges.
    pub async fn set_order_payment_intent(&self, order_id: &str, payment_intent_id: &str) -> Result<(), AppError> {
        self.client()
            .update_item()
            .table_name(self.table())
            .key("PK", s(order_pk(order_id)))
            .key("SK", s(METADATA_SK))
            .update_expression("SET stripePaymentIntentId = :pi, GSI2PK = :gsi2pk")
            .condition_expression("attribute_exists(PK)")
            .expression_attribute_values(":pi", s(payment_intent_id))
            .expression_attribute_values(":gsi2pk", s(payment_intent_gsi2pk(payment_intent_id)))
            .send()
            .await?;
        Ok(())
    }

    /// Move an order between statuses as a compare-and-set, returning `false` if it
    /// was not in the `from` status — the guard behind every idempotent webhook.
    pub async fn set_order_status(&self, order_id: &str, from: OrderStatus, to: OrderStatus) -> Result<bool, AppError> {
        let result = self
            .client()
            .update_item()
            .table_name(self.table())
            .key("PK", s(order_pk(order_id)))
            .key("SK", s(METADATA_SK))
            .update_expression("SET #status = :to")
            .condition_expression("#status = :from")
            .expression_attribute_names("#status", "status")
            .expression_attribute_values(":from", to_attribute_value(from)?)
            .expression_attribute_values(":to", to_attribute_value(to)?)
            .send()
            .await;

        condition_failed_as_false(result)
    }

    /// Allocate ticket numbers for a paid order. Reads the order and raffle with
    /// consistent reads, computes the next range, and commits the three-item
    /// transaction; a counter race re-reads and retries after a jittered backoff, while
    /// a replay or an over-cap order returns without burning numbers.
    pub async fn allocate_entry(&self, order_id: &str, payment: &PaidPayment, now: DateTime<Utc>) -> Result<Allocation, AppError> {
        for attempt in 0..ALLOCATION_ATTEMPTS {
            let Some(order): Option<Order> = self.get(order_pk(order_id), METADATA_SK, true).await? else {
                return Err(AppError::NotFound(format!("order {order_id}")));
            };
            match order.status {
                OrderStatus::Pending => {}
                OrderStatus::Paid => return Ok(Allocation::AlreadyPaid),
                other => return Err(AppError::Conflict(format!("order {order_id} is {other:?}"))),
            }

            let Some(raffle): Option<Raffle> = self.get(raffle_pk(&order.raffle_id), METADATA_SK, true).await? else {
                return Err(AppError::NotFound(format!("raffle {}", order.raffle_id)));
            };
            let (ticket_from, ticket_to) = ticket_range(raffle.tickets_sold, order.ticket_quantity);
            if ticket_to > raffle.max_tickets {
                return Ok(Allocation::SoldOut);
            }

            let entry = Entry {
                raffle_id: order.raffle_id.clone(),
                order_id: order.order_id.clone(),
                entrant_id: order.entrant_id.clone(),
                ticket_from,
                ticket_to,
                allocated_at: now,
            };

            match self.commit_allocation(&order, raffle.tickets_sold, &entry, payment, now).await {
                Ok(()) => return Ok(Allocation::Allocated(entry)),
                Err(err) => match allocation_conflict(&err) {
                    Some(AllocationConflict::OrderNotPending) => return Ok(Allocation::AlreadyPaid),
                    Some(AllocationConflict::Retryable) => {
                        tokio::time::sleep(jittered(ALLOCATION_BACKOFF_STEP * (attempt + 1))).await;
                    }
                    None => return Err(err),
                },
            }
        }

        Err(AppError::Conflict(format!("ticket counter contention allocating order {order_id}")))
    }

    async fn commit_allocation(
        &self,
        order: &Order,
        tickets_sold_before: u64,
        entry: &Entry,
        payment: &PaidPayment,
        now: DateTime<Utc>,
    ) -> Result<(), AppError> {
        let raffle_update = Update::builder()
            .table_name(self.table())
            .key("PK", s(raffle_pk(&order.raffle_id)))
            .key("SK", s(METADATA_SK))
            .update_expression(
                "SET ticketsSold = :to, ticketRevenuePence = ticketRevenuePence + :revenue, \
                 donationPence = donationPence + :donation",
            )
            .condition_expression("ticketsSold = :sold")
            .expression_attribute_values(":sold", n(tickets_sold_before))
            .expression_attribute_values(":to", n(entry.ticket_to))
            .expression_attribute_values(":revenue", n(order.ticket_amount_pence))
            .expression_attribute_values(":donation", n(order.donation_pence))
            .build()?;

        let entry_put = Put::builder()
            .table_name(self.table())
            .set_item(Some(item(entry, &entry_keys(entry))?))
            .condition_expression("attribute_not_exists(PK)")
            .build()?;

        let order_update = Update::builder()
            .table_name(self.table())
            .key("PK", s(order_pk(&order.order_id)))
            .key("SK", s(METADATA_SK))
            .update_expression(
                "SET #status = :paid, paidAt = :now, stripePaymentIntentId = :pi, \
                 cardFunding = :funding, cardLast4 = :last4, GSI2PK = :gsi2pk",
            )
            .condition_expression("#status = :pending")
            .expression_attribute_names("#status", "status")
            .expression_attribute_values(":paid", to_attribute_value(OrderStatus::Paid)?)
            .expression_attribute_values(":pending", to_attribute_value(OrderStatus::Pending)?)
            .expression_attribute_values(":now", to_attribute_value(now)?)
            .expression_attribute_values(":pi", s(payment.payment_intent_id.clone()))
            .expression_attribute_values(":funding", to_attribute_value(&payment.card_funding)?)
            .expression_attribute_values(":last4", to_attribute_value(&payment.card_last4)?)
            .expression_attribute_values(":gsi2pk", s(payment_intent_gsi2pk(&payment.payment_intent_id)))
            .build()?;

        self.client()
            .transact_write_items()
            .transact_items(TransactWriteItem::builder().update(raffle_update).build())
            .transact_items(TransactWriteItem::builder().put(entry_put).build())
            .transact_items(TransactWriteItem::builder().update(order_update).build())
            .send()
            .await?;

        Ok(())
    }

    /// Who holds ticket `N`: the last entry whose range starts at or before `N`,
    /// confirmed to actually contain it. One descending, limit-1 range query.
    pub async fn find_entry_by_ticket(&self, raffle_id: &str, ticket_number: u64) -> Result<Option<Entry>, AppError> {
        if ticket_number == 0 {
            return Ok(None);
        }

        let candidate: Option<Entry> = self
            .query(
                None,
                "PK = :pk AND SK BETWEEN :first AND :last",
                vec![
                    (":pk", s(raffle_pk(raffle_id))),
                    (":first", s(entry_sk(1))),
                    (":last", s(entry_sk(ticket_number))),
                ],
                true,
                Some(1),
                None,
            )
            .await?
            .first()?;

        Ok(candidate.filter(|entry| entry.contains(ticket_number)))
    }

    pub async fn list_entries_for_entrant(&self, entrant_id: &str) -> Result<Vec<Entry>, AppError> {
        self.query_entrant_index(entrant_id, "ENTRY#").await?.all()
    }

    pub async fn list_entries(&self, raffle_id: &str, limit: i32, start: Option<PageKey>) -> Result<(Vec<Entry>, Option<PageKey>), AppError> {
        self.query_prefix(raffle_pk(raffle_id), "ENTRY#", false, Some(limit), start).await?.paged()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entrant::Address;
    use crate::subscription::{Subscription, SubscriptionStatus};
    use crate::testing::{at, winter};
    use aws_sdk_dynamodb::types::CancellationReason;
    use aws_sdk_dynamodb::types::error::TransactionCanceledException;

    type PurchaseCase = (&'static str, Entrant, u32, DateTime<Utc>, Option<u16>);

    fn cancelled(codes: [&str; 3]) -> AppError {
        let mut builder = TransactionCanceledException::builder();
        for code in codes {
            builder = builder.cancellation_reasons(CancellationReason::builder().code(code).build());
        }
        AppError::from(aws_sdk_dynamodb::Error::TransactionCanceledException(builder.build()))
    }

    fn entrant(postcode: &str, born: (i32, u32, u32), excluded_until: Option<chrono::NaiveDate>) -> Entrant {
        Entrant {
            entrant_id: "ent-1".into(),
            title: "Ms".into(),
            first_name: "Ada".into(),
            last_name: "Lovelace".into(),
            email: "ada@example.com".into(),
            telephone: None,
            date_of_birth: chrono::NaiveDate::from_ymd_opt(born.0, born.1, born.2).unwrap(),
            address: Address {
                line1: "1 Road".into(),
                line2: None,
                town: "London".into(),
                postcode: postcode.into(),
                country: "GB".into(),
            },
            stripe_customer_id: Some("cus_1".into()),
            self_excluded_until: excluded_until,
            erased_at: None,
            created_at: at(2026, 9, 1),
        }
    }

    #[test]
    fn totals_split_ticket_and_donation() {
        // (quantity, donation, gift_aid) -> (ticket_amount, total)
        let cases = [(15, 1_000, true, 1_500, 2_500), (5, 500, false, 500, 1_000), (10, 0, true, 1_000, 1_000)];
        for (quantity, donation, gift_aid, ticket_amount, total) in cases {
            let order = Order::single("ord", &winter(), "ent-1", quantity, donation, gift_aid, at(2026, 10, 1));
            assert_eq!(order.ticket_amount_pence, ticket_amount);
            assert_eq!(order.total_pence, total);
            assert_eq!(order.status, OrderStatus::Pending);
        }
    }

    #[test]
    fn purchase_validation_enforces_every_licence_rule() {
        let open = at(2026, 10, 1);
        let adult = entrant("SW1A 1AA", (1990, 1, 1), None);

        // (label, entrant, quantity, now) -> expected HTTP status, None meaning Ok
        let cases: [PurchaseCase; 7] = [
            ("adult buys the maximum while open", adult.clone(), 20, open, None),
            ("over the per-order cap", adult.clone(), 21, open, Some(400)),
            ("zero tickets", adult.clone(), 0, open, Some(400)),
            ("raffle not yet open", adult.clone(), 1, at(2026, 9, 1), Some(409)),
            ("under 18", entrant("SW1A 1AA", (2010, 1, 1), None), 1, open, Some(403)),
            ("outside Great Britain", entrant("BT1 5GS", (1990, 1, 1), None), 1, open, Some(403)),
            (
                "self-excluded",
                entrant("SW1A 1AA", (1990, 1, 1), chrono::NaiveDate::from_ymd_opt(2027, 1, 1)),
                1,
                open,
                Some(403),
            ),
        ];

        for (label, entrant, quantity, now, expected) in cases {
            let status = validate_purchase(&winter(), &entrant, quantity, now).err().map(|err| err.status_code());
            assert_eq!(status, expected, "{label}");
        }
    }

    #[test]
    fn cancellation_reasons_decide_between_replay_retry_and_failure() {
        let cases = [
            (
                "order already paid",
                ["None", "None", "ConditionalCheckFailed"],
                Some(AllocationConflict::OrderNotPending),
            ),
            ("counter moved", ["ConditionalCheckFailed", "None", "None"], Some(AllocationConflict::Retryable)),
            (
                "entry row taken",
                ["None", "ConditionalCheckFailed", "None"],
                Some(AllocationConflict::Retryable),
            ),
            (
                "concurrent transaction on the raffle",
                ["TransactionConflict", "None", "None"],
                Some(AllocationConflict::Retryable),
            ),
            (
                "concurrent transaction on the order",
                ["None", "None", "TransactionConflict"],
                Some(AllocationConflict::Retryable),
            ),
            (
                "conflict beside a paid order still retries",
                ["TransactionConflict", "None", "ConditionalCheckFailed"],
                Some(AllocationConflict::Retryable),
            ),
            (
                "the raffle counter item was throttled",
                ["ThrottlingError", "None", "None"],
                Some(AllocationConflict::Retryable),
            ),
            (
                "the entry write exceeded provisioned capacity",
                ["None", "ProvisionedThroughputExceeded", "None"],
                Some(AllocationConflict::Retryable),
            ),
            (
                "a throttle beside a paid order still retries",
                ["ThrottlingError", "None", "ConditionalCheckFailed"],
                Some(AllocationConflict::Retryable),
            ),
            ("validation error", ["ValidationError", "None", "None"], None),
        ];
        for (label, codes, expected) in cases {
            assert_eq!(allocation_conflict(&cancelled(codes)), expected, "{label}");
        }
        assert_eq!(allocation_conflict(&AppError::BadRequest("x".into())), None);
    }

    #[test]
    fn backoff_jitter_stays_within_the_cap_and_varies() {
        let cap = Duration::from_millis(250);
        let samples: Vec<Duration> = (0..32).map(|_| jittered(cap)).collect();
        assert!(samples.iter().all(|sample| *sample <= cap));
        assert!(samples.iter().any(|sample| *sample != samples[0]));
    }

    #[test]
    fn ticket_ranges_are_contiguous_and_membership_is_inclusive() {
        assert_eq!(ticket_range(0, 15), (1, 15));
        assert_eq!(ticket_range(15, 10), (16, 25));

        let entry = Entry {
            raffle_id: "winter-2026".into(),
            order_id: "ord-2".into(),
            entrant_id: "ent-1".into(),
            ticket_from: 16,
            ticket_to: 25,
            allocated_at: at(2026, 10, 1),
        };
        for (ticket, inside) in [(15, false), (16, true), (25, true), (26, false)] {
            assert_eq!(entry.contains(ticket), inside, "ticket {ticket}");
        }
    }

    #[test]
    fn a_subscription_order_is_deterministic_and_free_of_gift_aid() {
        let subscription = Subscription {
            subscription_id: "sub-1".into(),
            entrant_id: "ent-1".into(),
            stripe_customer_id: "cus_1".into(),
            stripe_payment_method_id: "pm_1".into(),
            tickets_per_raffle: 10,
            eligible_from: winter().closes_at,
            status: SubscriptionStatus::Active,
            created_at: at(2026, 10, 1),
        };
        let mut spring = winter();
        spring.raffle_id = "spring-2027".into();

        let order = Order::from_subscription(&spring, &subscription, at(2027, 1, 9));
        assert_eq!(order.order_id, "sub_sub-1_spring-2027");
        assert_eq!(order.subscription_id.as_deref(), Some("sub-1"));
        assert_eq!(order.total_pence, 1_000);
        assert!(!order.gift_aid);
    }
}
