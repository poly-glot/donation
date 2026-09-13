use chrono::{DateTime, Duration, Utc};
use draw_run::{DrawReport, DrawRequest, METHOD, run};
use shared::draw::{Draw, Winner, WinnerStatus};
use shared::error::AppError;
use shared::order::{Allocation, Order, OrderStatus};
use shared::raffle::{Prize, Raffle, RaffleStatus};
use shared::table::DynamoRepo;
use shared::testing::{debit_payment, local_repo, raffle, scripted, seed_raffle};

const RAFFLE_ID: &str = "winter-2026";
const FIRST_PRIZE_PENCE: u64 = 2_000_000;
const RUNNER_UP_PENCE: u64 = 10_000;

const UNCONTESTED_WINNERS: [&str; 4] = [
    "1 rank 1 ticket 1 ord-1",
    "2 rank 2 ticket 26 ord-3",
    "3 rank 2 ticket 30 ord-3",
    "4 rank 2 ticket 15 ord-1",
];

struct SoldOrder {
    order_id: String,
    first_ticket: u64,
    last_ticket: u64,
}

struct ClosedRaffle {
    tickets_sold: u64,
    first_paid: SoldOrder,
    refunded: SoldOrder,
    last_paid: SoldOrder,
}

impl ClosedRaffle {
    fn uncontested_draws(&self) -> [u64; 4] {
        [
            self.first_paid.first_ticket,
            self.last_paid.first_ticket,
            self.last_paid.last_ticket,
            self.first_paid.last_ticket,
        ]
    }
}

fn dated(raffle_id: &str, closes_in_days: i64, draw_in_days: i64, now: DateTime<Utc>) -> Raffle {
    let mut raffle = raffle(raffle_id, -120, closes_in_days, now);
    raffle.draw_at = now + Duration::days(draw_in_days);
    raffle.results_at = raffle.draw_at + Duration::days(14);
    raffle
}

fn prize_for(raffle_id: &str, rank: u32, amount_pence: u64, quantity: u32) -> Prize {
    shared::testing::prize(raffle_id, rank, amount_pence, quantity)
}

fn draw_of(raffle_id: &str) -> DrawRequest {
    DrawRequest {
        raffle_id: raffle_id.into(),
        conducted_by: "Rufus Cruft".into(),
        witnessed_by: Some("Auditor".into()),
    }
}

fn draws<const N: usize>(tickets: [u64; N]) -> impl FnMut() -> u64 {
    scripted(tickets.into_iter().map(|ticket| ticket - 1).collect())
}

fn never_called() -> u64 {
    panic!("random source must not be used")
}

fn assert_conflict(result: Result<DrawReport, AppError>, reason: &str) {
    match result {
        Err(AppError::Conflict(message)) => assert_eq!(message, reason),
        other => panic!("expected the conflict \"{reason}\", got {other:?}"),
    }
}

fn assert_not_found(result: Result<DrawReport, AppError>) {
    match result {
        Err(AppError::NotFound(_)) => {}
        other => panic!("expected a not-found error, got {other:?}"),
    }
}

fn summary(winners: &[Winner]) -> Vec<String> {
    winners
        .iter()
        .map(|winner| {
            format!(
                "{} rank {} ticket {} {}",
                winner.sequence, winner.prize_rank, winner.ticket_number, winner.order_id
            )
        })
        .collect()
}

async fn sell(repo: &DynamoRepo, raffle: &Raffle, order_id: &str, quantity: u32, now: DateTime<Utc>) -> SoldOrder {
    let order = Order::single(order_id, raffle, &format!("ent-{order_id}"), quantity, 0, false, now);
    assert!(repo.create_order(&order).await.unwrap(), "{order_id} is a new order");

    let allocation = repo
        .allocate_entry(order_id, &debit_payment(format!("pi_{order_id}"), None), now)
        .await
        .unwrap();
    let Allocation::Allocated(entry) = allocation else {
        panic!("expected tickets for {order_id}, got {allocation:?}");
    };

    SoldOrder {
        order_id: order_id.into(),
        first_ticket: entry.ticket_from,
        last_ticket: entry.ticket_to,
    }
}

async fn closed_raffle_with_sales(repo: &DynamoRepo, now: DateTime<Utc>) -> ClosedRaffle {
    let raffle = dated(RAFFLE_ID, -10, -1, now);
    seed_raffle(repo, &raffle).await;

    let first_paid = sell(repo, &raffle, "ord-1", 15, now).await;
    let refunded = sell(repo, &raffle, "ord-2", 10, now).await;
    let last_paid = sell(repo, &raffle, "ord-3", 5, now).await;
    assert!(
        repo.set_order_status(&refunded.order_id, OrderStatus::Paid, OrderStatus::Refunded)
            .await
            .unwrap()
    );

    repo.put_prize(&prize_for(RAFFLE_ID, 2, RUNNER_UP_PENCE, 3)).await.unwrap();
    repo.put_prize(&prize_for(RAFFLE_ID, 1, FIRST_PRIZE_PENCE, 1)).await.unwrap();

    ClosedRaffle {
        tickets_sold: last_paid.last_ticket,
        first_paid,
        refunded,
        last_paid,
    }
}

async fn interrupt_after_the_first_prize(repo: &DynamoRepo, sales: &ClosedRaffle, now: DateTime<Utc>) {
    let draw = Draw {
        raffle_id: RAFFLE_ID.into(),
        drawn_at: now,
        tickets_sold: sales.tickets_sold,
        method: METHOD.into(),
        conducted_by: "Rufus Cruft".into(),
        witnessed_by: None,
    };
    assert!(repo.record_draw(&draw).await.unwrap());

    let winner = Winner {
        raffle_id: RAFFLE_ID.into(),
        sequence: 1,
        prize_rank: 1,
        prize_amount_pence: FIRST_PRIZE_PENCE,
        ticket_number: sales.first_paid.first_ticket,
        order_id: sales.first_paid.order_id.clone(),
        entrant_id: format!("ent-{}", sales.first_paid.order_id),
        status: WinnerStatus::Pending,
    };
    assert!(repo.put_winner(&winner).await.unwrap());
}

#[tokio::test]
async fn draws_every_prize_slot_in_rank_order() {
    let Some(repo) = local_repo("draw-test").await else {
        return;
    };
    let now = Utc::now();
    let sales = closed_raffle_with_sales(&repo, now).await;

    let report = run(&repo, draw_of(RAFFLE_ID), draws(sales.uncontested_draws()), now).await.unwrap();

    assert_eq!(report.tickets_sold, sales.tickets_sold, "the draw is over every ticket the raffle sold");
    assert_eq!(summary(&report.winners), UNCONTESTED_WINNERS);

    let [first, second, third, fourth] = report.winners.as_slice() else {
        panic!("expected one winner per prize slot, got {:?}", report.winners);
    };
    assert_eq!(first.prize_amount_pence, FIRST_PRIZE_PENCE, "rank 1 is drawn first and pays the first prize");
    for runner_up in [second, third, fourth] {
        assert_eq!(
            runner_up.prize_amount_pence, RUNNER_UP_PENCE,
            "the three rank 2 slots each pay the runner-up prize"
        );
    }
    assert!(
        report.winners.iter().all(|winner| winner.status == WinnerStatus::Pending),
        "a drawn prize is owed until an admin marks it paid"
    );
}

#[tokio::test]
async fn redraws_a_ticket_that_was_refunded_or_has_already_won() {
    let Some(repo) = local_repo("draw-test").await else {
        return;
    };
    let now = Utc::now();
    let sales = closed_raffle_with_sales(&repo, now).await;

    let report = run(
        &repo,
        draw_of(RAFFLE_ID),
        draws([
            sales.refunded.first_ticket,
            sales.first_paid.first_ticket,
            sales.first_paid.first_ticket,
            sales.last_paid.first_ticket,
            sales.refunded.last_ticket,
            sales.last_paid.last_ticket,
            sales.first_paid.last_ticket,
        ]),
        now,
    )
    .await
    .unwrap();

    assert_eq!(
        report.winners.iter().map(|winner| winner.ticket_number).collect::<Vec<u64>>(),
        sales.uncontested_draws(),
        "the refunded and repeated draws are skipped, so the same four tickets win as an uncontested script"
    );
}

#[tokio::test]
async fn records_the_witnessed_draw_and_repeats_it_without_drawing_again() {
    let Some(repo) = local_repo("draw-test").await else {
        return;
    };
    let now = Utc::now();
    let sales = closed_raffle_with_sales(&repo, now).await;

    let report = run(&repo, draw_of(RAFFLE_ID), draws(sales.uncontested_draws()), now).await.unwrap();

    let raffle = repo.get_raffle(RAFFLE_ID).await.unwrap().unwrap();
    assert_eq!(raffle.status_at(now), RaffleStatus::Drawn);

    let draw = repo.get_draw(RAFFLE_ID).await.unwrap().unwrap();
    assert_eq!(draw.conducted_by, "Rufus Cruft");
    assert_eq!(
        draw.witnessed_by.as_deref(),
        Some("Auditor"),
        "the witness is on the record the licence requires"
    );
    assert_eq!(draw.method, METHOD);
    assert_eq!(draw.tickets_sold, sales.tickets_sold);

    let again = run(&repo, draw_of(RAFFLE_ID), never_called, now + Duration::hours(1)).await.unwrap();

    assert_eq!(again.winners, report.winners, "a repeat run reports the draw that was conducted");
    assert_eq!(
        summary(&repo.list_winners(RAFFLE_ID).await.unwrap()),
        UNCONTESTED_WINNERS,
        "and leaves the recorded winners exactly as they were"
    );
}

#[tokio::test]
async fn interrupted_draw_resumes_from_the_next_prize() {
    let Some(repo) = local_repo("draw-test").await else {
        return;
    };
    let now = Utc::now();
    let sales = closed_raffle_with_sales(&repo, now).await;
    interrupt_after_the_first_prize(&repo, &sales, now).await;

    let report = run(&repo, draw_of(RAFFLE_ID), draws(sales.uncontested_draws()), now + Duration::hours(1))
        .await
        .unwrap();

    assert_eq!(summary(&report.winners), UNCONTESTED_WINNERS);
    assert_eq!(report.drawn_at, now, "the resumed run keeps the moment the draw was opened");
}

#[tokio::test]
async fn refuses_to_draw_when_a_precondition_fails_and_records_nothing() {
    let Some(repo) = local_repo("draw-test").await else {
        return;
    };
    let now = Utc::now();

    assert_not_found(run(&repo, draw_of("never-created"), never_called, now).await);

    let still_open = dated("still-open", 10, 24, now);
    let before_draw_date = dated("before-draw-date", -1, 13, now);
    let nothing_sold = dated("nothing-sold", -10, -1, now);
    let no_prizes = dated("no-prizes", -10, -1, now);

    for raffle in [&still_open, &before_draw_date, &nothing_sold, &no_prizes] {
        seed_raffle(&repo, raffle).await;
    }
    for raffle in [&still_open, &before_draw_date, &no_prizes] {
        sell(&repo, raffle, &format!("ord-{}", raffle.raffle_id), 5, now).await;
    }
    for raffle in [&still_open, &before_draw_date, &nothing_sold] {
        repo.put_prize(&prize_for(&raffle.raffle_id, 1, FIRST_PRIZE_PENCE, 1)).await.unwrap();
    }

    let cases = [
        ("a raffle still selling tickets", &still_open, "raffle is Open, not closed"),
        ("a closed raffle before its draw date", &before_draw_date, "draw date not reached"),
        ("a raffle nobody entered", &nothing_sold, "no tickets sold"),
        ("a raffle with no prize tiers", &no_prizes, "no prizes configured"),
    ];
    for (label, raffle, refusal) in cases {
        let raffle_id = raffle.raffle_id.as_str();
        assert_conflict(run(&repo, draw_of(raffle_id), never_called, now).await, refusal);
        assert!(repo.get_draw(raffle_id).await.unwrap().is_none(), "{label}: a refused draw records nothing");
        assert!(
            repo.get_raffle(raffle_id).await.unwrap().unwrap().drawn_at.is_none(),
            "{label}: a refused draw leaves the raffle undrawn"
        );
    }
}
