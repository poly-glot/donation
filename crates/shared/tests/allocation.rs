use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use shared::order::{Allocation, Entry, Order, OrderStatus};
use shared::raffle::Raffle;
use shared::subscription::SubscriptionStatus;
use shared::table::DynamoRepo;
use shared::testing::{debit_payment, entrant, local_repo, raffle, seed_raffle, subscription};

const RAFFLE_ID: &str = "winter-2026";
const ENTRANT_ID: &str = "ent-1";
const TICKET_PENCE: u64 = 100;

struct Sales {
    first: Order,
    first_tickets: Entry,
    later_tickets: Entry,
}

fn intent_of(order_id: &str) -> String {
    format!("pi_{order_id}")
}

fn allocated(allocation: Allocation) -> Entry {
    match allocation {
        Allocation::Allocated(entry) => entry,
        other => panic!("expected an allocation, got {other:?}"),
    }
}

async fn open_raffle(repo: &DynamoRepo, now: DateTime<Utc>) -> Raffle {
    let raffle = raffle(RAFFLE_ID, -1, 100, now);
    seed_raffle(repo, &raffle).await;
    raffle
}

async fn allocate(repo: &DynamoRepo, order_id: &str, now: DateTime<Utc>) -> Allocation {
    repo.allocate_entry(order_id, &debit_payment(intent_of(order_id), Some("4242")), now)
        .await
        .unwrap()
}

async fn two_paid_orders(repo: &DynamoRepo, now: DateTime<Utc>) -> Sales {
    let raffle = open_raffle(repo, now).await;

    let first = Order::single("ord-1", &raffle, ENTRANT_ID, 15, 500, true, now);
    let later = Order::single("ord-2", &raffle, ENTRANT_ID, 10, 0, false, now);
    assert!(repo.create_order(&first).await.unwrap());
    assert!(repo.create_order(&later).await.unwrap());

    let first_tickets = allocated(allocate(repo, &first.order_id, now).await);
    let later_tickets = allocated(allocate(repo, &later.order_id, now).await);

    Sales {
        first,
        first_tickets,
        later_tickets,
    }
}

async fn stored_raffle(repo: &DynamoRepo) -> Raffle {
    repo.get_raffle(RAFFLE_ID).await.unwrap().unwrap()
}

async fn order_status(repo: &DynamoRepo, order_id: &str) -> OrderStatus {
    repo.get_order(order_id).await.unwrap().unwrap().status
}

async fn owner_of_ticket(repo: &DynamoRepo, ticket: u64) -> Option<String> {
    let entry = repo.find_entry_by_ticket(RAFFLE_ID, None, ticket).await.unwrap();
    entry.map(|entry| entry.order_id)
}

async fn subscriptions_in(repo: &DynamoRepo, status: SubscriptionStatus) -> Vec<String> {
    let (page, _) = repo.list_subscriptions(status, 100, None).await.unwrap();
    page.into_iter().map(|subscription| subscription.subscription_id).collect()
}

#[tokio::test]
async fn allocates_gapless_ranges_idempotently() {
    let Some(repo) = local_repo("allocation-test").await else {
        return;
    };
    let now = Utc::now();
    let sales = two_paid_orders(&repo, now).await;

    assert_eq!(sales.first_tickets.ticket_from, 1, "a raffle with no sales hands out ticket 1 first");
    assert_eq!(
        sales.later_tickets.ticket_from,
        sales.first_tickets.ticket_to + 1,
        "the counter the first transaction wrote is the counter the second one read"
    );

    assert_eq!(
        allocate(&repo, "ord-1", now).await,
        Allocation::AlreadyPaid,
        "a redelivered webhook allocates nothing"
    );
    assert!(
        !repo.create_order(&sales.first).await.unwrap(),
        "a repeated order id loses the condition rather than overwriting the row"
    );

    let sold = sales.later_tickets.ticket_to;
    let after = stored_raffle(&repo).await;
    assert_eq!(
        (after.tickets_sold, after.ticket_revenue_pence, after.donation_pence),
        (sold, sold * TICKET_PENCE, 500),
        "the counter, the ticket money and the donation money all move in the one transaction"
    );

    let paid = repo.get_order(&sales.first.order_id).await.unwrap().unwrap();
    assert_eq!(paid.status, OrderStatus::Paid);
    assert_eq!(paid.card_funding.as_deref(), Some("debit"), "the card detail comes from the payment");
    assert_eq!(paid.paid_at, Some(now), "the order is stamped with the time the allocation was given");
}

#[tokio::test]
async fn finds_the_owner_of_a_ticket_and_everything_an_entrant_bought() {
    let Some(repo) = local_repo("allocation-test").await else {
        return;
    };
    let now = Utc::now();
    let sales = two_paid_orders(&repo, now).await;
    let first = sales.first_tickets;
    let later = sales.later_tickets;

    let cases = [
        (first.ticket_from, Some("ord-1"), "the first ticket of the first order"),
        (first.ticket_to, Some("ord-1"), "the last ticket before the boundary"),
        (later.ticket_from, Some("ord-2"), "the first ticket after the boundary"),
        (later.ticket_to + 1, None, "one past every ticket sold"),
        (0, None, "there is no ticket zero"),
    ];
    for (ticket, expected_owner, label) in cases {
        assert_eq!(owner_of_ticket(&repo, ticket).await.as_deref(), expected_owner, "{label}: ticket {ticket}");
    }

    let found = repo.find_order_by_payment_intent(&intent_of("ord-2")).await.unwrap().unwrap();
    assert_eq!(found.order_id, "ord-2", "the PaymentIntent index answers the webhook's only other lookup");

    let entries = repo.list_entries_for_entrant(ENTRANT_ID).await.unwrap();
    let [one, other] = entries.as_slice() else {
        panic!("expected one entry per order, got {entries:?}");
    };
    let mut ranges = [one.ticket_from, other.ticket_from];
    ranges.sort_unstable();
    assert_eq!(
        ranges,
        [first.ticket_from, later.ticket_from],
        "one query returns every range this entrant holds"
    );

    let orders = repo.list_orders_for_entrant(ENTRANT_ID).await.unwrap();
    assert_eq!(orders.len(), 2, "one query answers everything this entrant bought");

    let profile = entrant(ENTRANT_ID, now);
    repo.put_entrant(&profile).await.unwrap();
    let found_by_email = repo.find_entrant_by_email(&profile.email.to_uppercase()).await.unwrap().unwrap();
    assert_eq!(
        found_by_email.entrant_id, ENTRANT_ID,
        "the email index lowercases the address on the way in and on the way out"
    );
}

#[tokio::test]
async fn refuses_to_sell_past_the_licence_cap() {
    let Some(repo) = local_repo("allocation-test").await else {
        return;
    };
    let now = Utc::now();
    let mut capped = raffle(RAFFLE_ID, -1, 100, now);
    capped.max_tickets = 20;
    seed_raffle(&repo, &capped).await;

    let within = Order::single("ord-1", &capped, ENTRANT_ID, 15, 0, false, now);
    let over = Order::single("ord-2", &capped, ENTRANT_ID, 10, 0, false, now);
    repo.create_order(&within).await.unwrap();
    repo.create_order(&over).await.unwrap();

    let sold = allocated(allocate(&repo, "ord-1", now).await);
    assert_eq!((sold.ticket_from, sold.ticket_to), (1, 15));
    assert_eq!(
        allocate(&repo, "ord-2", now).await,
        Allocation::SoldOut,
        "ten more would pass the twenty-ticket licence"
    );
    assert_eq!(
        order_status(&repo, "ord-2").await,
        OrderStatus::Pending,
        "a sold-out order is failed by the webhook, not here"
    );
    assert_eq!(
        stored_raffle(&repo).await.tickets_sold,
        15,
        "the cap refuses the whole order rather than part of it"
    );
}

#[tokio::test]
async fn a_status_change_only_fires_from_the_status_it_expects() {
    let Some(repo) = local_repo("allocation-test").await else {
        return;
    };
    let now = Utc::now();
    let raffle = open_raffle(&repo, now).await;
    repo.create_order(&Order::single("ord-1", &raffle, ENTRANT_ID, 5, 0, false, now)).await.unwrap();

    assert!(repo.set_order_status("ord-1", OrderStatus::Pending, OrderStatus::Failed).await.unwrap());
    assert_eq!(order_status(&repo, "ord-1").await, OrderStatus::Failed);
    assert!(
        !repo.set_order_status("ord-1", OrderStatus::Pending, OrderStatus::Failed).await.unwrap(),
        "the row is no longer pending, so a replayed failure is a lost condition and not an error"
    );
}

#[tokio::test]
async fn concurrent_allocations_never_overlap_or_leave_gaps() {
    let Some(repo) = local_repo("allocation-test").await else {
        return;
    };
    let now = Utc::now();
    let raffle = open_raffle(&repo, now).await;

    let quantities: Vec<u32> = (1..=12).collect();
    for quantity in &quantities {
        let order = Order::single(format!("ord-{quantity}"), &raffle, ENTRANT_ID, *quantity, 0, false, now);
        repo.create_order(&order).await.unwrap();
    }

    let races: Vec<_> = quantities
        .iter()
        .map(|quantity| {
            let repo = repo.clone();
            let order_id = format!("ord-{quantity}");
            tokio::spawn(async move { allocate(&repo, &order_id, now).await })
        })
        .collect();

    let mut tickets = BTreeSet::new();
    for race in races {
        let entry = allocated(race.await.unwrap());
        for ticket in entry.ticket_from..=entry.ticket_to {
            assert!(tickets.insert(ticket), "ticket {ticket} was allocated twice");
        }
    }

    let total: u64 = quantities.iter().copied().map(u64::from).sum();
    assert_eq!(
        (
            tickets.len() as u64,
            tickets.iter().next_back().copied(),
            stored_raffle(&repo).await.tickets_sold
        ),
        (total, Some(total), total),
        "twelve concurrent transactions hand out every ticket from 1 to {total} exactly once"
    );
}

#[tokio::test]
async fn subscription_status_moves_between_active_lists() {
    let Some(repo) = local_repo("allocation-test").await else {
        return;
    };
    let now = Utc::now();
    repo.put_subscription(&subscription("sub-1", ENTRANT_ID, now)).await.unwrap();
    assert_eq!(subscriptions_in(&repo, SubscriptionStatus::Active).await, vec!["sub-1"]);

    assert!(repo.set_subscription_status("sub-1", SubscriptionStatus::Cancelled).await.unwrap());
    assert!(
        subscriptions_in(&repo, SubscriptionStatus::Active).await.is_empty(),
        "the row left the ACTIVE partition"
    );
    assert_eq!(
        subscriptions_in(&repo, SubscriptionStatus::Cancelled).await,
        vec!["sub-1"],
        "and arrived in the other one"
    );

    assert!(
        !repo.set_subscription_status("missing", SubscriptionStatus::Cancelled).await.unwrap(),
        "a subscription that does not exist cannot change status"
    );
}
