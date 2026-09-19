use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use shared::error::AppError;
use shared::order::{ALLOCATION_ATTEMPTS, Order, OrderStatus};
use shared::raffle::Raffle;
use shared::shard::{ShardedAllocation, ShardedEntry, locate, shard_of};
use shared::table::DynamoRepo;
use shared::testing::{debit_payment, local_repo, raffle, seed_raffle};
use tokio::task::JoinSet;

const ENTRANT_ID: &str = "ent-1";
const SHARDS: u32 = 8;
const RACING_ORDERS: usize = 40;

type Outcome = Result<(ShardedAllocation, u32), AppError>;

fn intent_of(order_id: &str) -> String {
    format!("pi_{order_id}")
}

fn allocation(outcome: Outcome) -> ShardedAllocation {
    outcome.unwrap().0
}

fn allocated(outcome: Outcome) -> ShardedEntry {
    match allocation(outcome) {
        ShardedAllocation::Allocated(entry) => entry,
        other => panic!("expected an allocation, got {other:?}"),
    }
}

fn order_homed_on(raffle_id: &str, shard: u32, shards: u32, nth: usize) -> String {
    (1..)
        .map(|i| format!("{raffle_id}-h{shard}-{i}"))
        .filter(|order_id| shard_of(order_id, shards) == shard)
        .nth(nth)
        .expect("hashing spreads order ids over every shard")
}

fn runs_by_shard(entries: &[ShardedEntry]) -> BTreeMap<u32, BTreeSet<(u64, u64)>> {
    let mut runs: BTreeMap<u32, BTreeSet<(u64, u64)>> = BTreeMap::new();
    for entry in entries {
        runs.entry(entry.shard).or_default().insert((entry.offset_from, entry.offset_to));
    }
    runs
}

fn lost_rounds(outcomes: &[Outcome]) -> (u32, u32) {
    outcomes.iter().fold((0, 0), |(lost, exhausted), outcome| match outcome {
        Ok((_, attempts)) => (lost + attempts - 1, exhausted),
        Err(AppError::Conflict(_)) => (lost + ALLOCATION_ATTEMPTS, exhausted + 1),
        Err(other) => panic!("allocation failed: {other}"),
    })
}

async fn open_raffle(repo: &DynamoRepo, raffle_id: &str, max_tickets: u64, shards: u32, now: DateTime<Utc>) -> Raffle {
    let mut raffle = raffle(raffle_id, -1, 100, now);
    raffle.max_tickets = max_tickets;

    seed_raffle(repo, &raffle).await;
    repo.create_counters(&raffle, shards).await.unwrap();

    raffle
}

async fn pending_order(repo: &DynamoRepo, raffle: &Raffle, order_id: &str, quantity: u32, now: DateTime<Utc>) {
    let order = Order::single(order_id, raffle, ENTRANT_ID, quantity, 0, false, now);

    assert!(repo.create_order(&order).await.unwrap(), "{order_id} is new");
}

async fn allocate(repo: &DynamoRepo, order_id: &str, shards: u32, now: DateTime<Utc>) -> Outcome {
    repo.allocate_sharded(order_id, &debit_payment(intent_of(order_id), Some("4242")), now, shards)
        .await
}

async fn race(repo: &DynamoRepo, raffle: &Raffle, quantities: &[u32], shards: u32, now: DateTime<Utc>) -> Vec<Outcome> {
    let order_ids: Vec<String> = (1..=quantities.len()).map(|i| format!("{}-ord-{i}", raffle.raffle_id)).collect();
    for (order_id, quantity) in order_ids.iter().zip(quantities) {
        pending_order(repo, raffle, order_id, *quantity, now).await;
    }

    let mut races = JoinSet::new();
    for order_id in order_ids {
        let repo = repo.clone();
        races.spawn(async move { allocate(&repo, &order_id, shards, now).await });
    }

    races.join_all().await
}

async fn sold_by_shard(repo: &DynamoRepo, raffle_id: &str) -> Vec<u64> {
    repo.counters(raffle_id).await.unwrap().iter().map(|counter| counter.sold).collect()
}

async fn order_status(repo: &DynamoRepo, order_id: &str) -> OrderStatus {
    repo.get_order(order_id).await.unwrap().unwrap().status
}

#[tokio::test]
async fn concurrent_sharded_allocations_are_disjoint_and_gapless_within_each_shard() {
    let Some(repo) = local_repo("shard-test").await else {
        return;
    };
    let now = Utc::now();
    let raffle = open_raffle(&repo, "winter-2026", 5_000_000, SHARDS, now).await;
    let quantities: Vec<u32> = (1..=RACING_ORDERS as u32).map(|i| i % 20 + 1).collect();

    let entries: Vec<ShardedEntry> = race(&repo, &raffle, &quantities, SHARDS, now).await.into_iter().map(allocated).collect();

    let sold = sold_by_shard(&repo, &raffle.raffle_id).await;
    for (shard, runs) in runs_by_shard(&entries) {
        let mut next = 1;
        for (from, to) in &runs {
            assert_eq!(*from, next, "shard {shard}: run {from}..{to} butts against the one before it");
            next = to + 1;
        }
        assert_eq!(sold[shard as usize], next - 1, "shard {shard}: the counter is the last offset in its ledger");
    }

    let total: u64 = quantities.iter().copied().map(u64::from).sum();
    assert_eq!(
        sold.iter().sum::<u64>(),
        total,
        "{RACING_ORDERS} racing orders sold every ticket exactly once across the shards"
    );
}

#[tokio::test]
async fn every_draw_number_resolves_to_a_held_physical_ticket() {
    let Some(repo) = local_repo("shard-test").await else {
        return;
    };
    let now = Utc::now();
    let raffle = open_raffle(&repo, "winter-2026", 5_000_000, SHARDS, now).await;
    let quantities: Vec<u32> = (1..=12).collect();
    race(&repo, &raffle, &quantities, SHARDS, now).await;

    let counts = sold_by_shard(&repo, &raffle.raffle_id).await;
    let universe: u64 = counts.iter().sum();
    for ticket in 1..=universe {
        let (shard, offset) = locate(&counts, ticket).expect("a draw number inside the universe lands on a shard");
        let holder = repo.find_sharded_entry(&raffle.raffle_id, shard, offset).await.unwrap();

        assert!(
            holder.is_some_and(|entry| entry.contains(offset)),
            "draw number {ticket} is ticket {offset} of shard {shard}, and one ledger query finds who holds it"
        );
    }
}

#[tokio::test]
async fn an_order_whose_home_shard_is_full_takes_the_next_shard_with_room() {
    let Some(repo) = local_repo("shard-test").await else {
        return;
    };
    let now = Utc::now();
    let shards = 2;
    let raffle = open_raffle(&repo, "small-2026", 6, shards, now).await;
    let filler = order_homed_on(&raffle.raffle_id, 0, shards, 0);
    let overflow = order_homed_on(&raffle.raffle_id, 0, shards, 1);
    pending_order(&repo, &raffle, &filler, 3, now).await;
    pending_order(&repo, &raffle, &overflow, 3, now).await;

    let filled = allocated(allocate(&repo, &filler, shards, now).await);
    let probed = allocated(allocate(&repo, &overflow, shards, now).await);

    assert_eq!(
        (filled.shard, filled.offset_from, filled.offset_to),
        (0, 1, 3),
        "the first order fills its home shard"
    );
    assert_eq!(
        (probed.shard, probed.offset_from, probed.offset_to),
        (1, 1, 3),
        "the second order is homed on the full shard and is placed on the next one"
    );
}

#[tokio::test]
async fn a_raffle_is_sold_out_only_when_no_shard_can_hold_the_whole_run() {
    let Some(repo) = local_repo("shard-test").await else {
        return;
    };
    let now = Utc::now();
    let shards = 2;
    let raffle = open_raffle(&repo, "small-2026", 6, shards, now).await;
    let left = order_homed_on(&raffle.raffle_id, 0, shards, 0);
    let right = order_homed_on(&raffle.raffle_id, 1, shards, 0);
    for order_id in [&left, &right] {
        pending_order(&repo, &raffle, order_id, 2, now).await;
        allocated(allocate(&repo, order_id, shards, now).await);
    }
    pending_order(&repo, &raffle, "pair", 2, now).await;
    pending_order(&repo, &raffle, "single", 1, now).await;

    assert_eq!(
        allocation(allocate(&repo, "pair", shards, now).await),
        ShardedAllocation::SoldOut,
        "a run never straddles shards, so one free ticket in each of two shards cannot hold a run of two"
    );
    let single = allocated(allocate(&repo, "single", shards, now).await);
    assert_eq!((single.offset_from, single.offset_to), (3, 3), "a run that fits a shard's tail is still sold");
}

#[tokio::test]
async fn a_replayed_sharded_allocation_returns_already_paid_and_burns_no_offsets() {
    let Some(repo) = local_repo("shard-test").await else {
        return;
    };
    let now = Utc::now();
    let raffle = open_raffle(&repo, "winter-2026", 5_000_000, SHARDS, now).await;
    pending_order(&repo, &raffle, "ord-once", 5, now).await;
    let first = allocated(allocate(&repo, "ord-once", SHARDS, now).await);

    assert_eq!(
        allocation(allocate(&repo, "ord-once", SHARDS, now).await),
        ShardedAllocation::AlreadyPaid,
        "a redelivered webhook allocates nothing"
    );
    assert_eq!(
        (
            sold_by_shard(&repo, &raffle.raffle_id).await.iter().sum::<u64>(),
            order_status(&repo, "ord-once").await
        ),
        (first.offset_to, OrderStatus::Paid),
        "the counters still hold only the first allocation and the order stays paid"
    );
}

#[tokio::test]
#[ignore = "measures contention against DynamoDB Local; run with --ignored --nocapture"]
async fn lost_rounds_fall_when_the_counter_is_sharded() {
    let Some(repo) = local_repo("shard-bench").await else {
        return;
    };
    let now = Utc::now();
    let rounds = 5;
    let sizes = [40, 80, 160];
    let mut table: BTreeMap<(usize, u32), Vec<(u32, u32)>> = BTreeMap::new();

    for contenders in sizes {
        for round in 0..rounds {
            for shards in [1, SHARDS] {
                let raffle = open_raffle(&repo, &format!("bench-{contenders}-{shards}-{round}"), 5_000_000, shards, now).await;
                let outcomes = race(&repo, &raffle, &vec![10; contenders], shards, now).await;

                table.entry((contenders, shards)).or_default().push(lost_rounds(&outcomes));
            }
        }
    }

    for ((contenders, shards), runs) in &table {
        println!("{contenders:>4} racing orders on {shards} shard(s): (lost rounds, exhausted orders) per run {runs:?}");
    }
    for contenders in sizes {
        let total = |shards: u32| table[&(contenders, shards)].iter().map(|(lost, _)| lost).sum::<u32>();

        assert!(
            total(SHARDS) < total(1),
            "{contenders} contenders: {SHARDS} shards lose fewer rounds than one counter: {table:?}"
        );
    }
}
