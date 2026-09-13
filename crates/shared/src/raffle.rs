//! The raffle itself, and the prize tiers that hang off it.
//!
//! A raffle is a fixed selling window with published draw and results dates.
//! Its lifecycle status is never stored — it is a pure function of the dates and
//! the moment the draw was recorded, so nothing can drift out of sync and no
//! scheduler has to flip a flag. The running totals (`tickets_sold`,
//! `ticket_revenue_pence`, `donation_pence`) live on this row and are moved only
//! by the allocation transaction in [`crate::order`], never written by hand.
//!
//! Prizes are a handful of tiers rather than one row per prize: "400 prizes" is
//! `1 × £20k`, `1 × £5k`, `1 × £1k` and a tier of many smaller ones, so a tier
//! carries a `quantity` and the draw expands it into individual winner slots.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_dynamo::aws_sdk_dynamodb_1::to_attribute_value;

use crate::error::AppError;
use crate::table::{APP, DynamoRepo, GSI1, METADATA_SK, condition_failed_as_false, n, partition, s, sort_ts};

/// The GSI1 partition that lists every raffle in `opens_at` order, so the API
/// can find the current, previous and next raffle in one query.
fn raffles_gsi1pk() -> String {
    format!("{APP}#RAFFLES")
}

pub(crate) fn raffle_pk(raffle_id: &str) -> String {
    partition("RAFFLE", raffle_id)
}

fn prize_sk(rank: u32) -> String {
    format!("PRIZE#{rank:04}")
}

fn raffles_gsi1sk(raffle: &Raffle) -> String {
    format!("{}#{}", sort_ts(raffle.opens_at), raffle.raffle_id)
}

fn raffle_keys(raffle: &Raffle) -> Vec<(&'static str, String)> {
    vec![
        ("PK", raffle_pk(&raffle.raffle_id)),
        ("SK", METADATA_SK.into()),
        ("GSI1PK", raffles_gsi1pk()),
        ("GSI1SK", raffles_gsi1sk(raffle)),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RaffleStatus {
    Scheduled,
    Open,
    Closed,
    Drawn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Raffle {
    pub raffle_id: String,
    pub name: String,
    pub ticket_price_pence: u64,
    pub max_tickets_per_order: u32,
    pub max_tickets: u64,
    pub opens_at: DateTime<Utc>,
    pub closes_at: DateTime<Utc>,
    pub draw_at: DateTime<Utc>,
    pub results_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drawn_at: Option<DateTime<Utc>>,
    pub tickets_sold: u64,
    pub ticket_revenue_pence: u64,
    pub donation_pence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscriptions_charged_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl Raffle {
    /// Status is derived, not stored: a draw wins over everything, otherwise the
    /// selling window decides between scheduled, open and closed.
    pub fn status_at(&self, now: DateTime<Utc>) -> RaffleStatus {
        if self.drawn_at.is_some() {
            RaffleStatus::Drawn
        } else if now < self.opens_at {
            RaffleStatus::Scheduled
        } else if now < self.closes_at {
            RaffleStatus::Open
        } else {
            RaffleStatus::Closed
        }
    }

    /// Subscribers are charged once, as soon as a raffle opens. The one-shot guard
    /// is the `subscriptions_charged_at` stamp, so the hourly run is a no-op after
    /// the first successful pass.
    pub fn needs_subscription_charge(&self, now: DateTime<Utc>) -> bool {
        self.status_at(now) == RaffleStatus::Open && self.subscriptions_charged_at.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prize {
    pub raffle_id: String,
    pub rank: u32,
    pub name: String,
    pub amount_pence: u64,
    pub quantity: u32,
}

impl DynamoRepo {
    /// Create a raffle, refusing to clobber one that already exists.
    pub async fn create_raffle(&self, raffle: &Raffle) -> Result<bool, AppError> {
        condition_failed_as_false(self.put(raffle, &raffle_keys(raffle), Some("attribute_not_exists(PK)")).await)
    }

    /// Overwrite a raffle, but only if its ticket count is still what the caller
    /// last read — so an admin edit can never silently discard a sale that landed
    /// in between.
    pub async fn replace_raffle(&self, raffle: &Raffle, expected_tickets_sold: u64) -> Result<bool, AppError> {
        let result = self
            .client()
            .put_item()
            .table_name(self.table())
            .set_item(Some(crate::table::item(raffle, &raffle_keys(raffle))?))
            .condition_expression("ticketsSold = :sold")
            .expression_attribute_values(":sold", n(expected_tickets_sold))
            .send()
            .await;

        condition_failed_as_false(result)
    }

    pub async fn get_raffle(&self, raffle_id: &str) -> Result<Option<Raffle>, AppError> {
        self.get(raffle_pk(raffle_id), METADATA_SK, false).await
    }

    pub async fn list_raffles(&self) -> Result<Vec<Raffle>, AppError> {
        self.query(Some(GSI1), "GSI1PK = :pk", vec![(":pk", s(raffles_gsi1pk()))], false, None, None)
            .await?
            .all()
    }

    /// Stamp the one-shot subscription-charge guard, returning `false` if it was
    /// already set so the caller knows another run got there first.
    pub async fn mark_subscriptions_charged(&self, raffle_id: &str, now: DateTime<Utc>) -> Result<bool, AppError> {
        let result = self
            .client()
            .update_item()
            .table_name(self.table())
            .key("PK", s(raffle_pk(raffle_id)))
            .key("SK", s(METADATA_SK))
            .update_expression("SET subscriptionsChargedAt = :now")
            .condition_expression("attribute_exists(PK) AND attribute_not_exists(subscriptionsChargedAt)")
            .expression_attribute_values(":now", to_attribute_value(now)?)
            .send()
            .await;

        condition_failed_as_false(result)
    }

    pub async fn put_prize(&self, prize: &Prize) -> Result<(), AppError> {
        let keys = [("PK", raffle_pk(&prize.raffle_id)), ("SK", prize_sk(prize.rank))];
        self.put(prize, &keys, None).await
    }

    pub async fn delete_prize(&self, raffle_id: &str, rank: u32) -> Result<bool, AppError> {
        let result = self
            .client()
            .delete_item()
            .table_name(self.table())
            .key("PK", s(raffle_pk(raffle_id)))
            .key("SK", s(prize_sk(rank)))
            .condition_expression("attribute_exists(PK)")
            .send()
            .await;

        condition_failed_as_false(result)
    }

    pub async fn list_prizes(&self, raffle_id: &str) -> Result<Vec<Prize>, AppError> {
        self.query_prefix(raffle_pk(raffle_id), "PRIZE#", false, None, None).await?.all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{at, winter};

    #[test]
    fn status_follows_the_dates_until_a_draw_overrides_them() {
        let drawn = Raffle {
            drawn_at: Some(at(2027, 1, 22)),
            ..winter()
        };

        let cases = [
            (winter(), at(2026, 9, 29), RaffleStatus::Scheduled),
            (winter(), at(2026, 9, 30), RaffleStatus::Open),
            (winter(), at(2027, 1, 7), RaffleStatus::Open),
            (winter(), at(2027, 1, 8), RaffleStatus::Closed),
            (drawn.clone(), at(2027, 1, 23), RaffleStatus::Drawn),
            // A draw wins even before the selling window would say "closed".
            (drawn, at(2026, 9, 29), RaffleStatus::Drawn),
        ];
        for (subject, now, expected) in cases {
            assert_eq!(subject.status_at(now), expected, "at {now}");
        }
    }

    #[test]
    fn subscription_charge_is_due_once_while_open_then_never_again() {
        let charged = Raffle {
            subscriptions_charged_at: Some(at(2026, 9, 30)),
            ..winter()
        };

        let cases = [
            (winter(), at(2026, 9, 29), false), // still scheduled
            (winter(), at(2026, 10, 1), true),  // open, not yet charged
            (charged, at(2026, 10, 1), false),  // open, already charged
            (winter(), at(2027, 1, 9), false),  // closed
        ];
        for (subject, now, expected) in cases {
            assert_eq!(subject.needs_subscription_charge(now), expected, "at {now}");
        }
    }
}
