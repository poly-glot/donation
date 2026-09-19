use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use shared::error::AppError;
use shared::order::{ALLOCATION_ATTEMPTS, Allocation, Entry, Order, OrderStatus};
use shared::raffle::Raffle;
use shared::shard::{locate, shard_of};
use shared::table::DynamoRepo;
use shared::testing::{debit_payment, local_repo, raffle, seed_raffle};
use tokio::task::JoinSet;

const ENTRANT_ID: &str = "ent-1";
const SHARDS: u32 = 8;
const RACING_ORDERS: usize = 40;
const TICKET_PENCE: u64 = 100;

type Outcome = Result<(Allocation, u32), AppError>;

fn intent_of(order_id: &str) -> String {
    format!("pi_{order_id}")
}

fn allocation(outcome: Outcome) -> Allocation {
    outcome.unwrap().0
}

fn allocated(outcome: Outcome) -> Entry {
    match allocation(outcome) {
        Allocation::Allocated(entry) => entry,
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

fn runs_by_shard(entries: &[Entry]) -> BTreeMap<u32, BTreeSet<(u64, u64)>> {
    let mut runs: BTreeMap<u32, BTreeSet<(u64, u64)>> = BTreeMap::new();
    for entry in entries {
        let shard = entry.shard.expect("an entry on a sharded raffle names its shard");
        runs.entry(shard).or_default().insert((entry.ticket_from, entry.ticket_to));
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

async fn open_raffle(repo: &DynamoRepo, raffle_id: &str, max_tickets: u64, shards: Option<u32>, now: DateTime<Utc>) -> Raffle {
    let mut raffle = raffle(raffle_id, -1, 100, now);
    raffle.max_tickets = max_tickets;
    raffle.shards = shards;

    seed_raffle(repo, &raffle).await;

    raffle
}

async fn pending_order(repo: &DynamoRepo, raffle: &Raffle, order_id: &str, quantity: u32, now: DateTime<Utc>) {
    let order = Order::single(order_id, raffle, ENTRANT_ID, quantity, 0, false, now);

    assert!(repo.create_order(&order).await.unwrap(), "{order_id} is new");
}

async fn allocate(repo: &DynamoRepo, order_id: &str, now: DateTime<Utc>) -> Outcome {
    repo.allocate_entry_counting(order_id, &debit_payment(intent_of(order_id), Some("4242")), now)
        .await
}

async fn race(repo: &DynamoRepo, raffle: &Raffle, quantities: &[u32], now: DateTime<Utc>) -> Vec<Outcome> {
    let order_ids: Vec<String> = (1..=quantities.len()).map(|i| format!("{}-ord-{i}", raffle.raffle_id)).collect();
    for (order_id, quantity) in order_ids.iter().zip(quantities) {
        pending_order(repo, raffle, order_id, *quantity, now).await;
    }

    let mut races = JoinSet::new();
    for order_id in order_ids {
        let repo = repo.clone();
        races.spawn(async move { allocate(&repo, &order_id, now).await });
    }

    races.join_all().await
}

async fn sold_by_shard(repo: &DynamoRepo, raffle: &Raffle) -> Vec<u64> {
    repo.counters(raffle).await.unwrap().iter().map(|counter| counter.sold).collect()
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
    let raffle = open_raffle(&repo, "winter-2026", 5_000_000, Some(SHARDS), now).await;
    let quantities: Vec<u32> = (1..=RACING_ORDERS as u32).map(|i| i % 20 + 1).collect();

    let entries: Vec<Entry> = race(&repo, &raffle, &quantities, now).await.into_iter().map(allocated).collect();

    let sold = sold_by_shard(&repo, &raffle).await;
    for (shard, runs) in runs_by_shard(&entries) {
        let mut next = 1;
        for (from, to) in &runs {
            assert_eq!(*from, next, "shard {shard}: run {from}..{to} butts against the one before it");
            next = to + 1;
        }
        assert_eq!(sold[shard as usize], next - 1, "shard {shard}: the counter is the last ticket in its ledger");
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
    let raffle = open_raffle(&repo, "winter-2026", 5_000_000, Some(SHARDS), now).await;
    let quantities: Vec<u32> = (1..=12).collect();
    race(&repo, &raffle, &quantities, now).await;

    let counts = sold_by_shard(&repo, &raffle).await;
    let universe: u64 = counts.iter().sum();
    for ticket in 1..=universe {
        let (shard, number) = locate(&counts, ticket).expect("a draw number inside the universe lands on a shard");
        let holder = repo.find_entry_by_ticket(&raffle.raffle_id, Some(shard), number).await.unwrap();

        assert!(
            holder.is_some_and(|entry| entry.contains(number)),
            "draw number {ticket} is ticket {number} of shard {shard}, and one ledger query finds who holds it"
        );
    }
}

#[tokio::test]
async fn a_sharded_raffle_reports_its_totals_from_the_counters() {
    let Some(repo) = local_repo("shard-test").await else {
        return;
    };
    let now = Utc::now();
    let raffle = open_raffle(&repo, "winter-2026", 5_000_000, Some(SHARDS), now).await;
    let quantities = [5, 10, 15];
    race(&repo, &raffle, &quantities, now).await;

    let totals = repo.with_totals(raffle.clone()).await.unwrap();
    let row = repo.get_raffle(&raffle.raffle_id).await.unwrap().unwrap();

    assert_eq!(
        (totals.tickets_sold, totals.ticket_revenue_pence, row.tickets_sold),
        (30, 30 * TICKET_PENCE, 0),
        "the totals are the sum of the counters and the raffle row itself never moves"
    );
}

#[tokio::test]
async fn an_order_whose_home_shard_is_full_takes_the_next_shard_with_room() {
    let Some(repo) = local_repo("shard-test").await else {
        return;
    };
    let now = Utc::now();
    let shards = 2;
    let raffle = open_raffle(&repo, "small-2026", 6, Some(shards), now).await;
    let filler = order_homed_on(&raffle.raffle_id, 0, shards, 0);
    let overflow = order_homed_on(&raffle.raffle_id, 0, shards, 1);
    pending_order(&repo, &raffle, &filler, 3, now).await;
    pending_order(&repo, &raffle, &overflow, 3, now).await;

    let filled = allocated(allocate(&repo, &filler, now).await);
    let probed = allocated(allocate(&repo, &overflow, now).await);

    assert_eq!(
        (filled.shard, filled.ticket_from, filled.ticket_to),
        (Some(0), 1, 3),
        "the first order fills its home shard"
    );
    assert_eq!(
        (probed.shard, probed.ticket_from, probed.ticket_to),
        (Some(1), 1, 3),
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
    let raffle = open_raffle(&repo, "small-2026", 6, Some(shards), now).await;
    let left = order_homed_on(&raffle.raffle_id, 0, shards, 0);
    let right = order_homed_on(&raffle.raffle_id, 1, shards, 0);
    for order_id in [&left, &right] {
        pending_order(&repo, &raffle, order_id, 2, now).await;
        allocated(allocate(&repo, order_id, now).await);
    }
    pending_order(&repo, &raffle, "pair", 2, now).await;
    pending_order(&repo, &raffle, "single", 1, now).await;

    assert_eq!(
        allocation(allocate(&repo, "pair", now).await),
        Allocation::SoldOut,
        "a run never straddles shards, so one free ticket in each of two shards cannot hold a run of two"
    );
    let single = allocated(allocate(&repo, "single", now).await);
    assert_eq!((single.ticket_from, single.ticket_to), (3, 3), "a run that fits a shard's tail is still sold");
}

#[tokio::test]
async fn a_replayed_sharded_allocation_returns_already_paid_and_burns_no_tickets() {
    let Some(repo) = local_repo("shard-test").await else {
        return;
    };
    let now = Utc::now();
    let raffle = open_raffle(&repo, "winter-2026", 5_000_000, Some(SHARDS), now).await;
    pending_order(&repo, &raffle, "ord-once", 5, now).await;
    let first = allocated(allocate(&repo, "ord-once", now).await);

    assert_eq!(
        allocation(allocate(&repo, "ord-once", now).await),
        Allocation::AlreadyPaid,
        "a redelivered webhook allocates nothing"
    );
    assert_eq!(
        (sold_by_shard(&repo, &raffle).await.iter().sum::<u64>(), order_status(&repo, "ord-once").await),
        (first.ticket_to, OrderStatus::Paid),
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
            for shards in [None, Some(SHARDS)] {
                let counters = shards.unwrap_or(1);
                let raffle = open_raffle(&repo, &format!("bench-{contenders}-{counters}-{round}"), 5_000_000, shards, now).await;
                let outcomes = race(&repo, &raffle, &vec![10; contenders], now).await;

                table.entry((contenders, counters)).or_default().push(lost_rounds(&outcomes));
            }
        }
    }

    for ((contenders, counters), runs) in &table {
        println!("{contenders:>4} racing orders on {counters} counter(s): (lost rounds, exhausted orders) per run {runs:?}");
    }
    for contenders in sizes {
        let total = |counters: u32| table[&(contenders, counters)].iter().map(|(lost, _)| lost).sum::<u32>();

        assert!(
            total(SHARDS) < total(1),
            "{contenders} contenders: {SHARDS} counters lose fewer rounds than one: {table:?}"
        );
    }
}
