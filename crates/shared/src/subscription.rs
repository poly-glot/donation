//! The VIP subscription: a saved debit card that auto-enters the supporter in
//! every future raffle.
//!
//! Stripe holds the card; we hold the calendar. The subscription is our entity —
//! it stores the Stripe customer and the payment method saved at sign-up — and a
//! per-raffle off-session charge is created when a raffle opens, so charges land
//! "ahead of each draw" no matter how irregular the raffle calendar is. A
//! subscription becomes eligible from the *next* raffle, never the draw the
//! supporter just entered, which is what `eligible_from` encodes.
//!
//! `status` is mirrored into `GSI2PK = SUBS#{status}` so the charge run can page
//! straight through the active subscribers without scanning.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_dynamo::aws_sdk_dynamodb_1::to_attribute_value;

use crate::entrant::entrant_pk;
use crate::error::AppError;
use crate::raffle::{Raffle, RaffleStatus};
use crate::table::{DynamoRepo, GSI2, METADATA_SK, PageKey, condition_failed_as_false, partition, s, sort_ts};

/// Every subscription buys the same number of tickets per raffle.
pub const TICKETS_PER_SUBSCRIPTION: u32 = 10;

pub(crate) fn subscription_pk(subscription_id: &str) -> String {
    partition("SUB", subscription_id)
}

fn entrant_subscription_gsi1sk(created_at: DateTime<Utc>) -> String {
    format!("SUB#{}", sort_ts(created_at))
}

fn subscriptions_gsi2pk(status: SubscriptionStatus) -> String {
    partition("SUBS", status.as_str())
}

fn subscription_keys(subscription: &Subscription) -> Vec<(&'static str, String)> {
    vec![
        ("PK", subscription_pk(&subscription.subscription_id)),
        ("SK", METADATA_SK.into()),
        ("GSI1PK", entrant_pk(&subscription.entrant_id)),
        ("GSI1SK", entrant_subscription_gsi1sk(subscription.created_at)),
        ("GSI2PK", subscriptions_gsi2pk(subscription.status)),
    ]
}

const PAGE_SIZE: i32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SubscriptionStatus {
    Active,
    PastDue,
    Cancelled,
}

impl SubscriptionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "ACTIVE",
            Self::PastDue => "PAST_DUE",
            Self::Cancelled => "CANCELLED",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    pub subscription_id: String,
    pub entrant_id: String,
    pub stripe_customer_id: String,
    pub stripe_payment_method_id: String,
    pub tickets_per_raffle: u32,
    pub eligible_from: DateTime<Utc>,
    pub status: SubscriptionStatus,
    pub created_at: DateTime<Utc>,
}

impl Subscription {
    /// When a new subscription starts counting. Signing up during an open raffle
    /// enters the *next* one (from this raffle's close), because the current draw's
    /// tickets were already bought outright; signing up at any other time is
    /// eligible immediately.
    pub fn eligible_from(current_raffle: Option<&Raffle>, now: DateTime<Utc>) -> DateTime<Utc> {
        current_raffle
            .filter(|raffle| raffle.status_at(now) == RaffleStatus::Open)
            .map_or(now, |raffle| raffle.closes_at)
    }

    /// A subscription is charged for a raffle when it is active and became eligible
    /// on or before that raffle opened.
    pub fn is_due_for(&self, raffle: &Raffle) -> bool {
        self.status == SubscriptionStatus::Active && self.eligible_from <= raffle.opens_at
    }

    pub fn order_id_for(&self, raffle_id: &str) -> String {
        format!("sub_{}_{raffle_id}", self.subscription_id)
    }

    /// A supporter has at most one subscription, keyed off their entrant id, so
    /// re-subscribing after a cancellation reuses the same row.
    pub fn id_for_entrant(entrant_id: &str) -> String {
        format!("sub_{entrant_id}")
    }
}

impl DynamoRepo {
    pub async fn put_subscription(&self, subscription: &Subscription) -> Result<(), AppError> {
        self.put(subscription, &subscription_keys(subscription), None).await
    }

    pub async fn get_subscription(&self, subscription_id: &str) -> Result<Option<Subscription>, AppError> {
        self.get(subscription_pk(subscription_id), METADATA_SK, false).await
    }

    pub async fn list_subscriptions_for_entrant(&self, entrant_id: &str) -> Result<Vec<Subscription>, AppError> {
        self.query_entrant_index(entrant_id, "SUB#").await?.all()
    }

    pub async fn due_subscriptions(&self, raffle: &Raffle) -> Result<Vec<Subscription>, AppError> {
        let mut due = Vec::new();
        let mut start = None;

        loop {
            let (page, next) = self.list_subscriptions(SubscriptionStatus::Active, PAGE_SIZE, start).await?;
            due.extend(page.into_iter().filter(|subscription| subscription.is_due_for(raffle)));
            let Some(key) = next else { break };
            start = Some(key);
        }
        Ok(due)
    }

    /// One page of subscriptions in a given status, via GSI2 — the charge run
    /// pages through `ACTIVE`.
    pub async fn list_subscriptions(
        &self,
        status: SubscriptionStatus,
        limit: i32,
        start: Option<PageKey>,
    ) -> Result<(Vec<Subscription>, Option<PageKey>), AppError> {
        self.query(
            Some(GSI2),
            "GSI2PK = :pk",
            vec![(":pk", s(subscriptions_gsi2pk(status)))],
            false,
            Some(limit),
            start,
        )
        .await?
        .paged()
    }

    /// Change status and keep the GSI2 mirror in step, so a cancelled or past-due
    /// subscription drops out of the active page immediately. Returns `false` if
    /// there is no such subscription.
    pub async fn set_subscription_status(&self, subscription_id: &str, status: SubscriptionStatus) -> Result<bool, AppError> {
        let result = self
            .client()
            .update_item()
            .table_name(self.table())
            .key("PK", s(subscription_pk(subscription_id)))
            .key("SK", s(METADATA_SK))
            .update_expression("SET #status = :status, GSI2PK = :gsi2pk")
            .condition_expression("attribute_exists(PK)")
            .expression_attribute_names("#status", "status")
            .expression_attribute_values(":status", to_attribute_value(status)?)
            .expression_attribute_values(":gsi2pk", s(subscriptions_gsi2pk(status)))
            .send()
            .await;

        condition_failed_as_false(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{at, winter};

    fn subscription(eligible_from: DateTime<Utc>, status: SubscriptionStatus) -> Subscription {
        Subscription {
            subscription_id: "sub-1".into(),
            entrant_id: "ent-1".into(),
            stripe_customer_id: "cus_1".into(),
            stripe_payment_method_id: "pm_1".into(),
            tickets_per_raffle: 10,
            eligible_from,
            status,
            created_at: at(2026, 10, 1),
        }
    }

    #[test]
    fn eligibility_starts_from_next_raffle_when_signing_up_during_an_open_one() {
        let raffle = winter();
        let during_open = at(2026, 10, 1);
        let while_closed = at(2027, 1, 10);

        // Signing up mid-raffle waits for the close; otherwise eligibility is now.
        assert_eq!(Subscription::eligible_from(Some(&raffle), during_open), raffle.closes_at);
        assert_eq!(Subscription::eligible_from(Some(&raffle), while_closed), while_closed);
        assert_eq!(Subscription::eligible_from(None, during_open), during_open);
    }

    #[test]
    fn a_subscription_is_due_only_when_active_and_eligible_before_the_raffle_opens() {
        let mut spring = winter();
        spring.raffle_id = "spring-2027".into();
        spring.opens_at = at(2027, 1, 9);

        // (label, eligible_from, status, raffle, due?)
        let cases = [
            (
                "eligible from this raffle's close, current raffle",
                winter().closes_at,
                SubscriptionStatus::Active,
                winter(),
                false,
            ),
            (
                "eligible from the winter close, next raffle",
                winter().closes_at,
                SubscriptionStatus::Active,
                spring.clone(),
                true,
            ),
            (
                "cancelled never charges",
                winter().closes_at,
                SubscriptionStatus::Cancelled,
                spring.clone(),
                false,
            ),
            ("past due never charges", winter().closes_at, SubscriptionStatus::PastDue, spring, false),
        ];
        for (label, eligible_from, status, raffle, due) in cases {
            assert_eq!(subscription(eligible_from, status).is_due_for(&raffle), due, "{label}");
        }
    }

    #[test]
    fn order_ids_are_deterministic_per_pair_and_entrant_scoped() {
        let subscription = subscription(winter().closes_at, SubscriptionStatus::Active);
        assert_eq!(subscription.order_id_for("spring-2027"), "sub_sub-1_spring-2027");
        assert_eq!(Subscription::id_for_entrant("ent-9"), "sub_ent-9");
    }
}
