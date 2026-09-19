use std::hash::{DefaultHasher, Hash, Hasher};

use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::order::Order;
use crate::raffle::Raffle;
use crate::table::{DynamoRepo, condition_failed_as_false, partition};

pub const MAX_SHARDS: u32 = 99;
pub(crate) const COUNTER_SK: &str = "#COUNTER";

pub(crate) fn shard_pk(raffle_id: &str, shard: u32) -> String {
    partition("RAFFLE", format!("{raffle_id}#{shard:02}"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Counter {
    pub raffle_id: String,
    pub shard: u32,
    pub sold: u64,
    pub cap: u64,
    pub ticket_revenue_pence: u64,
    pub donation_pence: u64,
}

impl Counter {
    fn has_room_for(&self, quantity: u32) -> bool {
        self.sold + u64::from(quantity) <= self.cap
    }
}

impl Raffle {
    pub fn summed(mut self, counters: &[Counter]) -> Self {
        if self.shards.is_none() {
            return self;
        }

        self.tickets_sold = counters.iter().map(|counter| counter.sold).sum();
        self.ticket_revenue_pence = counters.iter().map(|counter| counter.ticket_revenue_pence).sum();
        self.donation_pence = counters.iter().map(|counter| counter.donation_pence).sum();
        self
    }
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

pub fn draw_number(counts: &[u64], shard: u32, number: u64) -> u64 {
    counts.iter().take(shard as usize).sum::<u64>() + number
}

impl DynamoRepo {
    pub async fn create_counters(&self, raffle: &Raffle) -> Result<(), AppError> {
        let Some(shards) = raffle.shards else {
            return Ok(());
        };

        for (shard, cap) in caps(raffle.max_tickets, shards).into_iter().enumerate() {
            let counter = Counter {
                raffle_id: raffle.raffle_id.clone(),
                shard: shard as u32,
                sold: 0,
                cap,
                ticket_revenue_pence: 0,
                donation_pence: 0,
            };
            let keys = [("PK", shard_pk(&raffle.raffle_id, counter.shard)), ("SK", COUNTER_SK.into())];

            condition_failed_as_false(self.put(&counter, &keys, Some("attribute_not_exists(PK)")).await)?;
        }

        Ok(())
    }

    pub async fn counters(&self, raffle: &Raffle) -> Result<Vec<Counter>, AppError> {
        let Some(shards) = raffle.shards else {
            return Ok(Vec::new());
        };
        let keys = (0..shards).map(|shard| (shard_pk(&raffle.raffle_id, shard), COUNTER_SK.to_string())).collect();

        let mut counters: Vec<Counter> = self.batch_get(keys, true).await?;
        if counters.len() != shards as usize {
            return Err(AppError::Internal(format!(
                "raffle {} has {} of {shards} counters",
                raffle.raffle_id,
                counters.len()
            )));
        }
        counters.sort_by_key(|counter| counter.shard);

        Ok(counters)
    }

    pub async fn with_totals(&self, raffle: Raffle) -> Result<Raffle, AppError> {
        let counters = self.counters(&raffle).await?;

        Ok(raffle.summed(&counters))
    }

    pub(crate) async fn counter_with_room(&self, raffle: &Raffle, order: &Order, shards: u32) -> Result<Option<Counter>, AppError> {
        let home = shard_of(&order.order_id, shards);

        for probe in 0..shards {
            let shard = (home + probe) % shards;
            let Some(counter): Option<Counter> = self.get(shard_pk(&raffle.raffle_id, shard), COUNTER_SK, true).await? else {
                return Err(AppError::NotFound(format!("counter {shard} of raffle {}", raffle.raffle_id)));
            };

            if counter.has_room_for(order.ticket_quantity) {
                return Ok(Some(counter));
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::testing::winter;

    fn counter(shard: u32, sold: u64, revenue: u64, donation: u64) -> Counter {
        Counter {
            raffle_id: "winter-2026".into(),
            shard,
            sold,
            cap: 625_000,
            ticket_revenue_pence: revenue,
            donation_pence: donation,
        }
    }

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
    fn draw_number_inverts_locate() {
        let counts = [4, 0, 7, 1, 3];
        let universe: u64 = counts.iter().sum();

        for ticket in 1..=universe {
            let (shard, number) = locate(&counts, ticket).expect("inside the universe");
            assert_eq!(
                draw_number(&counts, shard, number),
                ticket,
                "ticket {number} of shard {shard} is draw number {ticket}"
            );
        }
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

    #[test]
    fn totals_come_from_the_row_or_from_the_counters() {
        let mut sold_on_the_row = winter();
        sold_on_the_row.tickets_sold = 25;
        sold_on_the_row.ticket_revenue_pence = 2_500;
        sold_on_the_row.donation_pence = 500;
        let mut sharded = winter();
        sharded.shards = Some(2);
        let counters = [counter(0, 10, 1_000, 200), counter(1, 15, 1_500, 300)];

        let cases = [
            ("a raffle without shards keeps its own row", sold_on_the_row, &counters[..0], (25, 2_500, 500)),
            ("a sharded raffle sums its counters", sharded, &counters[..], (25, 2_500, 500)),
        ];

        for (label, raffle, counters, expected) in cases {
            let raffle = raffle.summed(counters);
            assert_eq!((raffle.tickets_sold, raffle.ticket_revenue_pence, raffle.donation_pence), expected, "{label}");
        }
    }
}
