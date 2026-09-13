use admin::{AdminRequest, run};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use shared::draw::{Draw, Winner, WinnerStatus};
use shared::error::AppError;
use shared::order::{Allocation, Entry, Order};
use shared::raffle::Raffle;
use shared::subscription::SubscriptionStatus;
use shared::table::DynamoRepo;
use shared::testing::{debit_payment, entrant, local_repo, prize, raffle, seed_raffle, subscription, winter};

const RAFFLE_ID: &str = "winter-2026";
const ENTRANT_ID: &str = "ent-1";
const SUBSCRIPTION_ID: &str = "sub_ent-1";
const FIRST_PRIZE_PENCE: u64 = 2_000_000;
const SEEDED_SUBSCRIBERS: usize = 51;
const WINNING_TICKET: u64 = 7;

fn request(value: Value) -> AdminRequest {
    serde_json::from_value(value).unwrap()
}

fn action(name: &str, mut body: Value) -> AdminRequest {
    body["action"] = json!(name);
    request(body)
}

fn raffle_input(name: &str, closes_in_days: i64, draw_in_days: i64, now: DateTime<Utc>) -> Value {
    json!({
        "raffleId": RAFFLE_ID,
        "name": name,
        "ticketPricePence": 100,
        "maxTicketsPerOrder": 20,
        "maxTickets": 5000000,
        "opensAt": now - Duration::days(1),
        "closesAt": now + Duration::days(closes_in_days),
        "drawAt": now + Duration::days(draw_in_days),
        "resultsAt": now + Duration::days(draw_in_days + 14)
    })
}

fn prize_tier(rank: u32, amount_pence: u64, quantity: u32) -> AdminRequest {
    action("putPrize", json!(prize(RAFFLE_ID, rank, amount_pence, quantity)))
}

fn winner(entrant_id: &str, status: WinnerStatus) -> Winner {
    Winner {
        raffle_id: RAFFLE_ID.into(),
        sequence: 1,
        prize_rank: 1,
        prize_amount_pence: FIRST_PRIZE_PENCE,
        ticket_number: WINNING_TICKET,
        order_id: "ord-1".into(),
        entrant_id: entrant_id.into(),
        status,
    }
}

async fn stored_raffle(repo: &DynamoRepo) -> Raffle {
    repo.get_raffle(RAFFLE_ID).await.unwrap().unwrap()
}

async fn subscription_status(repo: &DynamoRepo, subscription_id: &str) -> SubscriptionStatus {
    repo.get_subscription(subscription_id).await.unwrap().unwrap().status
}

async fn findable_by_email(repo: &DynamoRepo, email: &str) -> bool {
    repo.find_entrant_by_email(email).await.unwrap().is_some()
}

fn assert_bad_request(result: Result<Value, AppError>, label: &str) {
    assert!(
        matches!(&result, Err(AppError::BadRequest(_))),
        "expected a bad request for {label}, got {result:?}"
    );
}

fn assert_conflict(result: Result<Value, AppError>, label: &str) {
    assert!(matches!(&result, Err(AppError::Conflict(_))), "expected a conflict for {label}, got {result:?}");
}

fn assert_not_found(result: Result<Value, AppError>, label: &str) {
    assert!(
        matches!(&result, Err(AppError::NotFound(_))),
        "expected a not-found for {label}, got {result:?}"
    );
}

#[tokio::test]
async fn a_raffle_is_created_once_and_updated_without_losing_the_sales_the_admin_never_saw() {
    let Some(repo) = local_repo("admin-test").await else {
        return;
    };
    let now = Utc::now();
    let opened = || action("createRaffle", raffle_input("Winter Poppy Raffle 2026", 100, 114, now));
    let renamed = || raffle_input("Renamed", 101, 115, now);

    let created = run(&repo, opened(), now).await.unwrap();
    assert_eq!(created["ticketsSold"], json!(0), "the response of a new raffle reports no sales");
    assert_conflict(run(&repo, opened(), now).await, "a raffle that already exists");

    let raffle = stored_raffle(&repo).await;
    let order = Order::single("ord-1", &raffle, ENTRANT_ID, 5, 0, false, now);
    assert!(repo.create_order(&order).await.unwrap(), "ord-1 is a new order");

    let allocation = repo.allocate_entry(&order.order_id, &debit_payment("pi_1", None), now).await.unwrap();
    let Allocation::Allocated(sale) = allocation else {
        panic!("the admin's raffle must have sold tickets, got {allocation:?}");
    };

    let updated = run(&repo, action("updateRaffle", renamed()), now).await.unwrap();
    assert_eq!(updated["name"], json!("Renamed"));

    let stored = stored_raffle(&repo).await;
    assert_eq!(
        (stored.name.as_str(), stored.tickets_sold, stored.ticket_revenue_pence),
        ("Renamed", sale.ticket_to, sale.ticket_to * raffle.ticket_price_pence),
        "the tickets sold between the admin's read and their write survive the update"
    );
    assert_eq!((stored.created_at, stored.closes_at), (raffle.created_at, now + Duration::days(101)));

    let mut drawn_before_close = renamed();
    drawn_before_close["drawAt"] = json!(now + Duration::days(50));
    assert_bad_request(
        run(&repo, action("updateRaffle", drawn_before_close), now).await,
        "a draw date before the close",
    );

    let mut repriced = renamed();
    repriced["ticketPricePence"] = json!(200);
    assert_bad_request(run(&repo, action("updateRaffle", repriced), now).await, "a ticket price change after sales");

    let mut unknown_raffle = renamed();
    unknown_raffle["raffleId"] = json!("nope");
    assert_not_found(run(&repo, action("updateRaffle", unknown_raffle), now).await, "an unknown raffle id");

    assert_eq!(stored_raffle(&repo).await, stored, "every refused update returned before writing the row");
}

#[tokio::test]
async fn a_prize_tier_is_stored_and_a_worthless_one_is_refused() {
    let Some(repo) = local_repo("admin-test").await else {
        return;
    };
    let now = Utc::now();
    seed_raffle(&repo, &winter()).await;

    let stored = run(&repo, prize_tier(1, FIRST_PRIZE_PENCE, 1), now).await.unwrap();
    assert_eq!(stored["amountPence"], json!(FIRST_PRIZE_PENCE));

    let prizes = repo.list_prizes(RAFFLE_ID).await.unwrap();
    let [first] = prizes.as_slice() else {
        panic!("expected one prize tier beside the raffle row, got {prizes:?}");
    };
    assert_eq!((first.rank, first.amount_pence, first.quantity), (1, FIRST_PRIZE_PENCE, 1));

    assert_bad_request(run(&repo, prize_tier(2, FIRST_PRIZE_PENCE, 0), now).await, "a tier nobody can win");
    assert_bad_request(run(&repo, prize_tier(2, 0, 1), now).await, "a tier worth nothing");
    assert_eq!(repo.list_prizes(RAFFLE_ID).await.unwrap().len(), 1, "a refused tier is never written");
}

#[tokio::test]
async fn cancelling_a_subscription_stops_it_being_charged_again() {
    let Some(repo) = local_repo("admin-test").await else {
        return;
    };
    let now = Utc::now();
    repo.put_subscription(&subscription(SUBSCRIPTION_ID, ENTRANT_ID, now)).await.unwrap();

    let cancel = |subscription_id: &str| request(json!({ "action": "cancelSubscription", "subscriptionId": subscription_id }));
    run(&repo, cancel(SUBSCRIPTION_ID), now).await.unwrap();
    assert_eq!(subscription_status(&repo, SUBSCRIPTION_ID).await, SubscriptionStatus::Cancelled);

    assert_not_found(run(&repo, cancel("sub_nope"), now).await, "an unknown subscription");
}

#[tokio::test]
async fn marking_a_winner_paid_closes_out_the_prize() {
    let Some(repo) = local_repo("admin-test").await else {
        return;
    };
    let now = Utc::now();
    repo.put_winner(&winner(ENTRANT_ID, WinnerStatus::Pending)).await.unwrap();

    let pay = |sequence: u32| request(json!({ "action": "setWinnerStatus", "raffleId": RAFFLE_ID, "sequence": sequence, "status": "PAID" }));
    run(&repo, pay(1), now).await.unwrap();

    let winners = repo.list_winners(RAFFLE_ID).await.unwrap();
    let [paid] = winners.as_slice() else {
        panic!("expected one winner, got {winners:?}");
    };
    assert_eq!(paid.status, WinnerStatus::Paid);

    assert_not_found(run(&repo, pay(9), now).await, "a sequence that was never drawn");
}

#[tokio::test]
async fn erasure_refuses_unpaid_prizes_then_cancels_subscriptions_and_redacts() {
    let Some(repo) = local_repo("admin-test").await else {
        return;
    };
    let now = Utc::now();
    let mut entrant = entrant(ENTRANT_ID, now);
    entrant.stripe_customer_id = Some("cus_1".into());
    repo.put_entrant(&entrant).await.unwrap();
    repo.put_subscription(&subscription(SUBSCRIPTION_ID, ENTRANT_ID, now)).await.unwrap();
    repo.put_winner(&winner(ENTRANT_ID, WinnerStatus::Notified)).await.unwrap();

    let erase = |entrant_id: &str| request(json!({ "action": "eraseEntrant", "entrantId": entrant_id }));
    assert_conflict(run(&repo, erase(ENTRANT_ID), now).await, "an entrant still owed a prize");
    assert!(findable_by_email(&repo, &entrant.email).await, "a refused erasure redacts nothing");

    repo.set_winner_status(RAFFLE_ID, 1, WinnerStatus::Paid).await.unwrap();
    let result = run(&repo, erase(ENTRANT_ID), now).await.unwrap();
    assert_eq!(result["erased"], json!(true));
    assert_eq!(result["subscriptionsCancelled"], json!(1), "the one live subscription the arrangement seeded");

    let erased = repo.get_entrant(ENTRANT_ID).await.unwrap().unwrap();
    assert_eq!(
        (erased.email.as_str(), erased.stripe_customer_id.as_deref()),
        (shared::entrant::ERASED, None),
        "the redacted row is what was persisted, Stripe handle included"
    );
    assert!(erased.erased_at.is_some());
    assert!(!findable_by_email(&repo, &entrant.email).await, "the email index entry goes with the address");
    assert_eq!(subscription_status(&repo, SUBSCRIPTION_ID).await, SubscriptionStatus::Cancelled);

    let again = run(&repo, erase(ENTRANT_ID), now).await.unwrap();
    assert_eq!(
        again["subscriptionsCancelled"],
        json!(0),
        "an already-cancelled subscription is not cancelled twice"
    );

    let ghost = run(&repo, erase("ghost"), now).await.unwrap();
    assert_eq!(ghost["erased"], json!(false), "erasing an unknown entrant is not an error");
}

fn remove_prize(rank: u32) -> AdminRequest {
    request(json!({ "action": "removePrize", "raffleId": RAFFLE_ID, "rank": rank }))
}

fn draw_record(now: DateTime<Utc>, tickets_sold: u64) -> Draw {
    Draw {
        raffle_id: RAFFLE_ID.into(),
        drawn_at: now,
        tickets_sold,
        method: "os-csprng-uniform-rejection".into(),
        conducted_by: "Responsible Person".into(),
        witnessed_by: Some("Auditor".into()),
    }
}

async fn paid_tickets(repo: &DynamoRepo, order_id: &str, ticket_quantity: u32, now: DateTime<Utc>) -> Entry {
    let raffle = stored_raffle(repo).await;
    let order = Order::single(order_id, &raffle, ENTRANT_ID, ticket_quantity, 500, true, now);
    assert!(repo.create_order(&order).await.unwrap(), "{order_id} is a new order");

    let allocation = repo
        .allocate_entry(order_id, &debit_payment(format!("pi_{order_id}"), Some("5556")), now)
        .await
        .unwrap();
    let Allocation::Allocated(entry) = allocation else {
        panic!("{order_id} must have been allocated tickets, got {allocation:?}");
    };
    entry
}

#[tokio::test]
async fn a_prize_tier_needs_a_raffle_to_hang_off() {
    let Some(repo) = local_repo("admin-test").await else {
        return;
    };
    let now = Utc::now();

    assert_not_found(
        run(&repo, prize_tier(1, FIRST_PRIZE_PENCE, 1), now).await,
        "a tier for a raffle that was never created",
    );
}

#[tokio::test]
async fn a_prize_tier_is_removed_by_its_rank() {
    let Some(repo) = local_repo("admin-test").await else {
        return;
    };
    let now = Utc::now();
    seed_raffle(&repo, &winter()).await;
    run(&repo, prize_tier(1, FIRST_PRIZE_PENCE, 1), now).await.unwrap();
    run(&repo, prize_tier(2, 500_000, 1), now).await.unwrap();

    run(&repo, remove_prize(1), now).await.unwrap();

    let prizes = repo.list_prizes(RAFFLE_ID).await.unwrap();
    let [kept] = prizes.as_slice() else {
        panic!("expected the untouched tier to be the only one left, got {prizes:?}");
    };
    assert_eq!(kept.rank, 2, "removing rank 1 leaves rank 2 untouched");

    assert_not_found(run(&repo, remove_prize(1), now).await, "a rank that was already removed");
}

#[tokio::test]
async fn prize_tiers_are_frozen_once_the_raffle_is_drawn() {
    let Some(repo) = local_repo("admin-test").await else {
        return;
    };
    let now = Utc::now();
    seed_raffle(&repo, &winter()).await;
    run(&repo, prize_tier(1, FIRST_PRIZE_PENCE, 1), now).await.unwrap();

    let mut drawn = winter();
    drawn.drawn_at = Some(now);
    seed_raffle(&repo, &drawn).await;

    assert_conflict(run(&repo, prize_tier(2, 500_000, 1), now).await, "a tier added after the draw");
    assert_conflict(run(&repo, remove_prize(1), now).await, "a tier removed after the draw");
    assert_eq!(
        repo.list_prizes(RAFFLE_ID).await.unwrap().len(),
        1,
        "the raffle keeps the tiers it was drawn with"
    );
}

#[tokio::test]
async fn every_raffle_is_listed_with_the_status_its_dates_imply() {
    let Some(repo) = local_repo("admin-test").await else {
        return;
    };
    let now = Utc::now();
    seed_raffle(&repo, &raffle(RAFFLE_ID, -1, 100, now)).await;
    seed_raffle(&repo, &raffle("spring-2027", 200, 300, now)).await;

    let listed = run(&repo, request(json!({ "action": "listRaffles" })), now).await.unwrap();

    let rows = listed.as_array().expect("the list is an array");
    let statuses: Vec<(&str, &str)> = rows
        .iter()
        .map(|row| (row["raffleId"].as_str().unwrap_or_default(), row["status"].as_str().unwrap_or_default()))
        .collect();
    assert_eq!(
        statuses,
        vec![("winter-2026", "OPEN"), ("spring-2027", "SCHEDULED")],
        "the index orders the raffles by the day they open, newest last"
    );
}

#[tokio::test]
async fn one_read_answers_a_raffle_with_its_prizes_draw_and_winners() {
    let Some(repo) = local_repo("admin-test").await else {
        return;
    };
    let now = Utc::now();
    seed_raffle(&repo, &winter()).await;
    run(&repo, prize_tier(1, FIRST_PRIZE_PENCE, 1), now).await.unwrap();
    repo.put_winner(&winner(ENTRANT_ID, WinnerStatus::Pending)).await.unwrap();
    assert!(repo.record_draw(&draw_record(now, 15)).await.unwrap(), "the draw is claimed once");

    let detail = run(&repo, request(json!({ "action": "getRaffle", "raffleId": RAFFLE_ID })), now).await.unwrap();

    assert_eq!(detail["status"], json!("DRAWN"), "the recorded draw is what makes the raffle drawn");
    assert_eq!(
        detail["prizes"][0]["amountPence"],
        json!(FIRST_PRIZE_PENCE),
        "the tier the admin stored is the tier the detail reports"
    );
    assert_eq!(
        detail["draw"]["conductedBy"],
        json!("Responsible Person"),
        "the draw record travels with the raffle"
    );
    assert_eq!(
        detail["winners"][0]["ticketNumber"],
        json!(WINNING_TICKET),
        "the winner seeded beside the draw comes back with it"
    );

    assert_not_found(
        run(&repo, request(json!({ "action": "getRaffle", "raffleId": "nope" })), now).await,
        "a raffle that was never created",
    );
}

#[tokio::test]
async fn the_ledger_lists_the_ticket_runs_a_raffle_has_sold() {
    let Some(repo) = local_repo("admin-test").await else {
        return;
    };
    let now = Utc::now();
    seed_raffle(&repo, &winter()).await;
    let first = paid_tickets(&repo, "ord-1", 5, now).await;
    let later = paid_tickets(&repo, "ord-2", 3, now).await;

    let ledger = run(&repo, request(json!({ "action": "listEntries", "raffleId": RAFFLE_ID })), now)
        .await
        .unwrap();

    let runs: Vec<(u64, u64)> = ledger["entries"]
        .as_array()
        .expect("the ledger is an array")
        .iter()
        .map(|entry| (entry["ticketFrom"].as_u64().unwrap_or_default(), entry["ticketTo"].as_u64().unwrap_or_default()))
        .collect();
    assert_eq!(
        runs,
        vec![(first.ticket_from, first.ticket_to), (later.ticket_from, later.ticket_to)],
        "the ledger comes back in ticket order"
    );
    assert_eq!(ledger["cursor"], Value::Null, "a page with room to spare carries no cursor");
}

#[tokio::test]
async fn an_order_comes_back_with_its_tickets_and_the_person_who_bought_them() {
    let Some(repo) = local_repo("admin-test").await else {
        return;
    };
    let now = Utc::now();
    seed_raffle(&repo, &winter()).await;
    repo.put_entrant(&entrant(ENTRANT_ID, now)).await.unwrap();
    let sale = paid_tickets(&repo, "ord-1", 5, now).await;

    let detail = run(&repo, request(json!({ "action": "getOrder", "orderId": "ord-1" })), now).await.unwrap();

    assert_eq!(detail["status"], json!("PAID"), "the allocation flipped the order the view reports");
    assert_eq!(
        (detail["entry"]["ticketFrom"].as_u64(), detail["entry"]["ticketTo"].as_u64()),
        (Some(sale.ticket_from), Some(sale.ticket_to)),
        "the view carries the range the allocation handed out"
    );
    assert_eq!(
        detail["entrant"]["email"],
        json!(entrant(ENTRANT_ID, now).email),
        "the buyer is resolved from the order, not restated by the caller"
    );

    assert_not_found(
        run(&repo, request(json!({ "action": "getOrder", "orderId": "ord-nope" })), now).await,
        "an order id nobody was given",
    );
}

#[tokio::test]
async fn a_ticket_number_resolves_to_the_order_holding_it() {
    let Some(repo) = local_repo("admin-test").await else {
        return;
    };
    let now = Utc::now();
    seed_raffle(&repo, &winter()).await;
    let sale = paid_tickets(&repo, "ord-1", 5, now).await;

    let inside = |ticket: u64| request(json!({ "action": "findTicket", "raffleId": RAFFLE_ID, "ticketNumber": ticket }));
    let found = run(&repo, inside(sale.ticket_to), now).await.unwrap();
    assert_eq!(found["orderId"], json!("ord-1"), "the last ticket of the run belongs to the run");

    assert_not_found(run(&repo, inside(sale.ticket_to + 1), now).await, "a ticket number nobody bought");
}

#[tokio::test]
async fn an_entrant_dossier_gathers_the_orders_tickets_subscriptions_and_wins_of_one_person() {
    let Some(repo) = local_repo("admin-test").await else {
        return;
    };
    let now = Utc::now();
    seed_raffle(&repo, &winter()).await;
    repo.put_entrant(&entrant(ENTRANT_ID, now)).await.unwrap();
    repo.put_subscription(&subscription(SUBSCRIPTION_ID, ENTRANT_ID, now)).await.unwrap();
    repo.put_winner(&winner(ENTRANT_ID, WinnerStatus::Notified)).await.unwrap();
    let sale = paid_tickets(&repo, "ord-1", 5, now).await;

    let dossier = run(&repo, request(json!({ "action": "getEntrant", "entrantId": ENTRANT_ID })), now)
        .await
        .unwrap();

    assert_eq!(
        dossier["orders"][0]["orderId"],
        json!("ord-1"),
        "the order partition is reached through the entrant index"
    );
    assert_eq!(
        dossier["entries"][0]["ticketFrom"],
        json!(sale.ticket_from),
        "the ticket run the allocation wrote"
    );
    assert_eq!(
        dossier["subscriptions"][0]["subscriptionId"],
        json!(SUBSCRIPTION_ID),
        "the subscription hangs off the same partition"
    );
    assert_eq!(
        dossier["winners"][0]["ticketNumber"],
        json!(WINNING_TICKET),
        "the win is gathered with everything else"
    );

    assert_not_found(
        run(&repo, request(json!({ "action": "getEntrant", "entrantId": "ghost" })), now).await,
        "an entrant id nobody was given",
    );
}

#[tokio::test]
async fn an_entrant_is_found_by_the_email_they_checked_out_with() {
    let Some(repo) = local_repo("admin-test").await else {
        return;
    };
    let now = Utc::now();
    let supporter = entrant(ENTRANT_ID, now);
    repo.put_entrant(&supporter).await.unwrap();

    let by_email = |email: &str| request(json!({ "action": "findEntrant", "email": email }));
    let dossier = run(&repo, by_email(&supporter.email), now).await.unwrap();
    assert_eq!(
        dossier["entrantId"],
        json!(ENTRANT_ID),
        "the email index finds the profile the dossier is built from"
    );

    assert_not_found(run(&repo, by_email("stranger@example.com"), now).await, "an address nobody signed up with");
}

#[tokio::test]
async fn the_subscription_page_carries_only_the_status_that_was_asked_for() {
    let Some(repo) = local_repo("admin-test").await else {
        return;
    };
    let now = Utc::now();
    repo.put_subscription(&subscription(SUBSCRIPTION_ID, ENTRANT_ID, now)).await.unwrap();
    repo.put_subscription(&subscription("sub_ent-2", "ent-2", now)).await.unwrap();
    run(&repo, request(json!({ "action": "cancelSubscription", "subscriptionId": "sub_ent-2" })), now)
        .await
        .unwrap();

    let listed = |status: &str| request(json!({ "action": "listSubscriptions", "status": status }));
    let active = run(&repo, listed("ACTIVE"), now).await.unwrap();
    let cancelled = run(&repo, listed("CANCELLED"), now).await.unwrap();

    assert_eq!(
        active["subscriptions"][0]["subscriptionId"],
        json!(SUBSCRIPTION_ID),
        "the untouched subscriber is still on the active page"
    );
    assert_eq!(active["subscriptions"][1], Value::Null, "the cancelled subscriber left the active page");
    assert_eq!(
        cancelled["subscriptions"][0]["subscriptionId"],
        json!("sub_ent-2"),
        "cancelling moved it to the cancelled page rather than dropping it"
    );
}

#[tokio::test]
async fn a_subscriber_list_longer_than_one_page_is_walked_by_the_cursor_it_returns() {
    let Some(repo) = local_repo("admin-test").await else {
        return;
    };
    let now = Utc::now();
    for index in 0..SEEDED_SUBSCRIBERS {
        repo.put_subscription(&subscription(&format!("sub_ent-{index}"), &format!("ent-{index}"), now))
            .await
            .unwrap();
    }

    let listed = |cursor: Option<&str>| request(json!({ "action": "listSubscriptions", "status": "ACTIVE", "cursor": cursor }));
    let first = run(&repo, listed(None), now).await.unwrap();
    let cursor = first["cursor"].as_str().expect("a page that cannot hold every subscriber carries a cursor");

    let second = run(&repo, listed(Some(cursor)), now).await.unwrap();

    let counted = |page: &Value| page["subscriptions"].as_array().map_or(0, Vec::len);
    assert_eq!(
        counted(&first) + counted(&second),
        SEEDED_SUBSCRIBERS,
        "the two pages together are every subscriber the arrangement seeded"
    );
    assert_eq!(second["cursor"], Value::Null, "the page that finishes the list carries no cursor");
}
