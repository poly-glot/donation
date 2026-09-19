//! The draw record and its winners.
//!
//! A draw is claimed atomically: the `#DRAW` row and the raffle's `drawn_at`
//! stamp are written in one transaction, both conditioned on absence, so it can
//! happen exactly once. The draw snapshots `tickets_sold` as the universe the
//! numbers are drawn from, which also fixes the status as `DRAWN` — allocations
//! that land afterwards can never enter this draw.
//!
//! Winners are written one per prize slot, each conditioned on its sequence
//! number, so a draw that is interrupted and re-run resumes from the next slot
//! rather than double-awarding. The winner-status lifecycle
//! (`PENDING → NOTIFIED → PAID`, or `UNCLAIMED`) is tracked here; the table
//! stream records when each transition happened. The picking policy — uniform
//! selection with rejection sampling, skipping refunded and repeat tickets —
//! lives in the `draw-run` Lambda, because it needs OS entropy and is not a
//! persistence concern.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_dynamo::aws_sdk_dynamodb_1::to_attribute_value;

use aws_sdk_dynamodb::types::{Put, TransactWriteItem, Update};

use crate::entrant::entrant_pk;
use crate::error::AppError;
use crate::raffle::raffle_pk;
use crate::table::{DynamoRepo, METADATA_SK, condition_failed_as_false, item, s};

const DRAW_SK: &str = "#DRAW";

fn winner_sk(sequence: u32) -> String {
    format!("WINNER#{sequence:04}")
}

fn entrant_winner_gsi1sk(raffle_id: &str, sequence: u32) -> String {
    format!("WINNER#{raffle_id}#{sequence:04}")
}

fn winner_keys(winner: &Winner) -> Vec<(&'static str, String)> {
    vec![
        ("PK", raffle_pk(&winner.raffle_id)),
        ("SK", winner_sk(winner.sequence)),
        ("GSI1PK", entrant_pk(&winner.entrant_id)),
        ("GSI1SK", entrant_winner_gsi1sk(&winner.raffle_id, winner.sequence)),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Draw {
    pub raffle_id: String,
    pub drawn_at: DateTime<Utc>,
    pub tickets_sold: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shard_counts: Vec<u64>,
    pub method: String,
    pub conducted_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub witnessed_by: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WinnerStatus {
    Pending,
    Notified,
    Paid,
    Unclaimed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Winner {
    pub raffle_id: String,
    pub sequence: u32,
    pub prize_rank: u32,
    pub prize_amount_pence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shard: Option<u32>,
    pub ticket_number: u64,
    pub order_id: String,
    pub entrant_id: String,
    pub status: WinnerStatus,
}

impl DynamoRepo {
    /// Claim the draw for a raffle: write the `#DRAW` record and set `drawn_at` in
    /// one transaction, both conditioned on absence. Returns `false` if another run
    /// already claimed it.
    pub async fn record_draw(&self, draw: &Draw) -> Result<bool, AppError> {
        let draw_put = Put::builder()
            .table_name(self.table())
            .set_item(Some(item(draw, &[("PK", raffle_pk(&draw.raffle_id)), ("SK", DRAW_SK.into())])?))
            .condition_expression("attribute_not_exists(PK)")
            .build()?;

        let raffle_update = Update::builder()
            .table_name(self.table())
            .key("PK", s(raffle_pk(&draw.raffle_id)))
            .key("SK", s(METADATA_SK))
            .update_expression("SET drawnAt = :now")
            .condition_expression("attribute_exists(PK) AND attribute_not_exists(drawnAt)")
            .expression_attribute_values(":now", to_attribute_value(draw.drawn_at)?)
            .build()?;

        let result = self
            .client()
            .transact_write_items()
            .transact_items(TransactWriteItem::builder().put(draw_put).build())
            .transact_items(TransactWriteItem::builder().update(raffle_update).build())
            .send()
            .await;

        condition_failed_as_false(result)
    }

    pub async fn get_draw(&self, raffle_id: &str) -> Result<Option<Draw>, AppError> {
        self.get(raffle_pk(raffle_id), DRAW_SK, true).await
    }

    /// Record a winner, conditioned on the sequence number being free, so a
    /// resumed or concurrent run never awards the same slot twice.
    pub async fn put_winner(&self, winner: &Winner) -> Result<bool, AppError> {
        condition_failed_as_false(self.put(winner, &winner_keys(winner), Some("attribute_not_exists(PK)")).await)
    }

    pub async fn list_winners(&self, raffle_id: &str) -> Result<Vec<Winner>, AppError> {
        self.query_prefix(raffle_pk(raffle_id), "WINNER#", false, None, None).await?.all()
    }

    pub async fn list_winners_for_entrant(&self, entrant_id: &str) -> Result<Vec<Winner>, AppError> {
        self.query_entrant_index(entrant_id, "WINNER#").await?.all()
    }

    pub async fn set_winner_status(&self, raffle_id: &str, sequence: u32, status: WinnerStatus) -> Result<bool, AppError> {
        let result = self
            .client()
            .update_item()
            .table_name(self.table())
            .key("PK", s(raffle_pk(raffle_id)))
            .key("SK", s(winner_sk(sequence)))
            .update_expression("SET #status = :status")
            .condition_expression("attribute_exists(PK)")
            .expression_attribute_names("#status", "status")
            .expression_attribute_values(":status", to_attribute_value(status)?)
            .send()
            .await;

        condition_failed_as_false(result)
    }
}
