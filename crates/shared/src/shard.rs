use std::hash::{DefaultHasher, Hash, Hasher};

use aws_sdk_dynamodb::types::{Put, TransactWriteItem, Update};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::entrant::entrant_pk;
use crate::error::AppError;
use crate::order::{
    ALLOCATION_ATTEMPTS, ALLOCATION_BACKOFF_STEP, AllocationConflict, Order, OrderStatus, PaidPayment, allocation_conflict, jittered, order_pk,
};
use crate::raffle::{Raffle, raffle_pk};
use crate::table::{DynamoRepo, METADATA_SK, item, n, s};

fn counter_sk(shard: u32) -> String {
    format!("COUNTER#{shard:02}")
}

fn entry_sk(shard: u32, offset: u64) -> String {
    format!("ENTRY#{shard:02}#{offset:08}")
}

fn entrant_entry_gsi1sk(raffle_id: &str, shard: u32, offset: u64) -> String {
    format!("ENTRY#{raffle_id}#{shard:02}#{offset:08}")
}

fn entry_keys(entry: &ShardedEntry) -> Vec<(&'static str, String)> {
    vec![
        ("PK", raffle_pk(&entry.raffle_id)),
        ("SK", entry_sk(entry.shard, entry.offset_from)),
        ("GSI1PK", entrant_pk(&entry.entrant_id)),
        ("GSI1SK", entrant_entry_gsi1sk(&entry.raffle_id, entry.shard, entry.offset_from)),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Counter {
    pub raffle_id: String,
    pub shard: u32,
    pub sold: u64,
    pub cap: u64,
}

impl Counter {
    fn has_room_for(&self, quantity: u32) -> bool {
        self.sold + u64::from(quantity) <= self.cap
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShardedEntry {
    pub raffle_id: String,
    pub order_id: String,
    pub entrant_id: String,
    pub shard: u32,
    pub offset_from: u64,
    pub offset_to: u64,
    pub allocated_at: DateTime<Utc>,
}

impl ShardedEntry {
    pub fn contains(&self, offset: u64) -> bool {
        (self.offset_from..=self.offset_to).contains(&offset)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardedAllocation {
    Allocated(ShardedEntry),
    AlreadyPaid,
    SoldOut,
}

pub fn shard_of(order_id: &str, shards: u32) -> u32 {
    let mut hasher = DefaultHasher::new();
    order_id.hash(&mut hasher);

    (hasher.finish() % u64::from(shards)) as u32
}

pub fn caps(max_tickets: u64, shards: u32) -> Vec<u64> {
    let shards = u64::from(shards);
    let base = max_tickets / shards;
    let remainder = max_tickets % shards;

    (0..shards).map(|shard| base + u64::from(shard < remainder)).collect()
}

pub fn locate(counts: &[u64], ticket: u64) -> Option<(u32, u64)> {
    let mut before = 0;

    for (shard, count) in counts.iter().enumerate() {
        if (before + 1..=before + count).contains(&ticket) {
            return Some((shard as u32, ticket - before));
        }
        before += count;
    }

    None
}

impl DynamoRepo {
    pub async fn create_counters(&self, raffle: &Raffle, shards: u32) -> Result<(), AppError> {
        for (shard, cap) in caps(raffle.max_tickets, shards).into_iter().enumerate() {
            let counter = Counter {
                raffle_id: raffle.raffle_id.clone(),
                shard: shard as u32,
                sold: 0,
                cap,
            };
            let keys = [("PK", raffle_pk(&raffle.raffle_id)), ("SK", counter_sk(counter.shard))];

            self.put(&counter, &keys, Some("attribute_not_exists(PK)")).await?;
        }

        Ok(())
    }

    pub async fn counters(&self, raffle_id: &str) -> Result<Vec<Counter>, AppError> {
        self.query_prefix(raffle_pk(raffle_id), "COUNTER#", false, None, None).await?.all()
    }

    pub async fn allocate_sharded(&self, order_id: &str, payment: &PaidPayment, now: DateTime<Utc>, shards: u32) -> Result<(ShardedAllocation, u32), AppError> {
        for attempt in 1..=ALLOCATION_ATTEMPTS {
            let Some(order): Option<Order> = self.get(order_pk(order_id), METADATA_SK, true).await? else {
                return Err(AppError::NotFound(format!("order {order_id}")));
            };
            match order.status {
                OrderStatus::Pending => {}
                OrderStatus::Paid => return Ok((ShardedAllocation::AlreadyPaid, attempt)),
                other => return Err(AppError::Conflict(format!("order {order_id} is {other:?}"))),
            }

            let Some(counter) = self.counter_with_room(&order, shards).await? else {
                return Ok((ShardedAllocation::SoldOut, attempt));
            };
            let entry = ShardedEntry {
                raffle_id: order.raffle_id.clone(),
                order_id: order.order_id.clone(),
                entrant_id: order.entrant_id.clone(),
                shard: counter.shard,
                offset_from: counter.sold + 1,
                offset_to: counter.sold + u64::from(order.ticket_quantity),
                allocated_at: now,
            };

            match self.commit_sharded(&order, counter.sold, &entry, payment, now).await {
                Ok(()) => return Ok((ShardedAllocation::Allocated(entry), attempt)),
                Err(err) => match allocation_conflict(&err) {
                    Some(AllocationConflict::OrderNotPending) => return Ok((ShardedAllocation::AlreadyPaid, attempt)),
                    Some(AllocationConflict::Retryable) => tokio::time::sleep(jittered(ALLOCATION_BACKOFF_STEP * attempt)).await,
                    None => return Err(err),
                },
            }
        }

        Err(AppError::Conflict(format!("ticket counter contention allocating order {order_id}")))
    }

    async fn counter_with_room(&self, order: &Order, shards: u32) -> Result<Option<Counter>, AppError> {
        let home = shard_of(&order.order_id, shards);

        for probe in 0..shards {
            let shard = (home + probe) % shards;
            let Some(counter): Option<Counter> = self.get(raffle_pk(&order.raffle_id), &counter_sk(shard), true).await? else {
                return Err(AppError::NotFound(format!("counter {shard} of raffle {}", order.raffle_id)));
            };

            if counter.has_room_for(order.ticket_quantity) {
                return Ok(Some(counter));
            }
        }

        Ok(None)
    }

    async fn commit_sharded(&self, order: &Order, sold_before: u64, entry: &ShardedEntry, payment: &PaidPayment, now: DateTime<Utc>) -> Result<(), AppError> {
        let counter_update = Update::builder()
            .table_name(self.table())
            .key("PK", s(raffle_pk(&entry.raffle_id)))
            .key("SK", s(counter_sk(entry.shard)))
            .update_expression("SET sold = :to")
            .condition_expression("sold = :read")
            .expression_attribute_values(":read", n(sold_before))
            .expression_attribute_values(":to", n(entry.offset_to))
            .build()?;

        let entry_put = Put::builder()
            .table_name(self.table())
            .set_item(Some(item(entry, &entry_keys(entry))?))
            .condition_expression("attribute_not_exists(PK)")
            .build()?;

        let order_update = self.paid_order_update(order, payment, now)?;

        self.client()
            .transact_write_items()
            .transact_items(TransactWriteItem::builder().update(counter_update).build())
            .transact_items(TransactWriteItem::builder().put(entry_put).build())
            .transact_items(TransactWriteItem::builder().update(order_update).build())
            .send()
            .await?;

        Ok(())
    }

    pub async fn find_sharded_entry(&self, raffle_id: &str, shard: u32, offset: u64) -> Result<Option<ShardedEntry>, AppError> {
        let candidate: Option<ShardedEntry> = self
            .query(
                None,
                "PK = :pk AND SK BETWEEN :first AND :last",
                vec![
                    (":pk", s(raffle_pk(raffle_id))),
                    (":first", s(entry_sk(shard, 1))),
                    (":last", s(entry_sk(shard, offset))),
                ],
                true,
                Some(1),
                None,
            )
            .await?
            .first()?;

        Ok(candidate.filter(|entry| entry.contains(offset)))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn locate_is_a_bijection_from_draw_numbers_onto_physical_tickets() {
        let counts = [4, 0, 7, 1, 3];
        let universe: u64 = counts.iter().sum();

        let reached: BTreeSet<(u32, u64)> = (1..=universe).map(|ticket| locate(&counts, ticket).expect("inside the universe")).collect();
        let held: BTreeSet<(u32, u64)> = counts
            .iter()
            .enumerate()
            .flat_map(|(shard, count)| (1..=*count).map(move |offset| (shard as u32, offset)))
            .collect();

        assert_eq!(
            reached, held,
            "each draw number reaches a distinct physical ticket and every sold ticket is reached"
        );
        assert_eq!(
            (locate(&counts, 0), locate(&counts, universe + 1)),
            (None, None),
            "the numbers either side of 1 to {universe} are not tickets"
        );
    }

    #[test]
    fn caps_split_the_licence_limit_exactly_across_shards() {
        let cases = [
            ("the remainder goes to the first shards", 10, 4, vec![3, 3, 2, 2]),
            ("one shard keeps the whole limit", 7, 1, vec![7]),
            ("an even split", 5_000_000, 8, vec![625_000; 8]),
        ];

        for (label, max_tickets, shards, expected) in cases {
            let split = caps(max_tickets, shards);

            assert_eq!(split, expected, "{label}");
            assert_eq!(split.iter().sum::<u64>(), max_tickets, "{label}: the caps add up to the limit");
        }
    }
}
