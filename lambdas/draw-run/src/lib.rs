use std::collections::HashSet;

use chrono::{DateTime, Utc};
use lambda_http::{Body, Request, Response};
use serde::{Deserialize, Serialize};
use shared::auth::Cognito;
use shared::draw::{Draw, Winner, WinnerStatus};
use shared::error::AppError;
use shared::http::{answered, authorise, body};
use shared::order::{Entry, OrderStatus};
use shared::raffle::{Prize, Raffle, RaffleStatus};
use shared::shard::{Counter, draw_number, locate};
use shared::table::DynamoRepo;

pub const METHOD: &str = "os-csprng-uniform-rejection";
const MAX_REDRAWS_PER_PRIZE: u32 = 1_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DrawRequest {
    pub raffle_id: String,
    pub conducted_by: String,
    #[serde(default)]
    pub witnessed_by: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DrawReport {
    pub raffle_id: String,
    pub drawn_at: DateTime<Utc>,
    pub tickets_sold: u64,
    pub winners: Vec<Winner>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DrawState {
    Fresh,
    Resume,
}

pub fn uniform(mut random: impl FnMut() -> u64, upper: u64) -> u64 {
    let zone = u64::MAX - u64::MAX % upper;
    loop {
        let sample = random();
        if sample < zone {
            return sample % upper + 1;
        }
    }
}

pub fn prize_slots(prizes: &[Prize]) -> Vec<&Prize> {
    let mut by_rank: Vec<&Prize> = prizes.iter().collect();
    by_rank.sort_by_key(|prize| prize.rank);
    by_rank
        .into_iter()
        .flat_map(|prize| std::iter::repeat_n(prize, prize.quantity as usize))
        .collect()
}

pub fn draw_state(raffle: &Raffle, now: DateTime<Utc>) -> Result<DrawState, AppError> {
    match raffle.status_at(now) {
        RaffleStatus::Drawn => Ok(DrawState::Resume),
        RaffleStatus::Closed if now < raffle.draw_at => Err(AppError::Conflict("draw date not reached".into())),
        RaffleStatus::Closed if raffle.tickets_sold == 0 => Err(AppError::Conflict("no tickets sold".into())),
        RaffleStatus::Closed => Ok(DrawState::Fresh),
        other => Err(AppError::Conflict(format!("raffle is {other:?}, not closed"))),
    }
}

pub struct Ceremony {
    repo: DynamoRepo,
    cognito: Cognito,
}

impl Ceremony {
    pub fn new(repo: DynamoRepo, cognito: Cognito) -> Self {
        Self { repo, cognito }
    }

    pub async fn handle(&self, request: Request) -> Response<Body> {
        answered(self.act(request, Utc::now()).await)
    }

    async fn act(&self, request: Request, now: DateTime<Utc>) -> Result<DrawReport, AppError> {
        let claims = authorise(&self.cognito, &request, now).await?;
        let drawing: DrawRequest = body(&request)?;
        tracing::info!(subject = %claims.sub, raffle_id = %drawing.raffle_id, "draw authorised");

        run(&self.repo, drawing, shared::random::u64, now).await
    }
}

pub async fn run(repo: &DynamoRepo, request: DrawRequest, mut random: impl FnMut() -> u64, now: DateTime<Utc>) -> Result<DrawReport, AppError> {
    let Some(raffle) = repo.get_raffle(&request.raffle_id).await? else {
        return Err(AppError::NotFound(format!("raffle {}", request.raffle_id)));
    };
    let counters = repo.counters(&raffle).await?;
    let raffle = raffle.summed(&counters);
    let state = draw_state(&raffle, now)?;

    let prizes = repo.list_prizes(&raffle.raffle_id).await?;
    let slots = prize_slots(&prizes);
    if slots.is_empty() {
        return Err(AppError::Conflict("no prizes configured".into()));
    }

    let draw = match state {
        DrawState::Fresh => record_draw(repo, &raffle, &counters, request, now).await?,
        DrawState::Resume => repo
            .get_draw(&raffle.raffle_id)
            .await?
            .ok_or_else(|| AppError::Internal(format!("raffle {} is drawn without a draw record", raffle.raffle_id)))?,
    };

    let mut winners = repo.list_winners(&raffle.raffle_id).await?;
    let mut won: HashSet<u64> = winners.iter().map(|winner| draw_number_of(&draw, winner)).collect();

    for (index, prize) in slots.iter().enumerate().skip(winners.len()) {
        let pick = pick_entry(repo, &draw, &mut random, &won).await?;
        let winner = Winner {
            raffle_id: raffle.raffle_id.clone(),
            sequence: index as u32 + 1,
            prize_rank: prize.rank,
            prize_amount_pence: prize.amount_pence,
            shard: pick.shard,
            ticket_number: pick.ticket_number,
            order_id: pick.entry.order_id,
            entrant_id: pick.entry.entrant_id,
            status: WinnerStatus::Pending,
        };
        if !repo.put_winner(&winner).await? {
            return Err(AppError::Conflict(format!("winner {} already recorded by another run", winner.sequence)));
        }
        won.insert(pick.drawn);
        winners.push(winner);
    }

    Ok(DrawReport {
        raffle_id: raffle.raffle_id,
        drawn_at: draw.drawn_at,
        tickets_sold: draw.tickets_sold,
        winners,
    })
}

async fn record_draw(repo: &DynamoRepo, raffle: &Raffle, counters: &[Counter], request: DrawRequest, now: DateTime<Utc>) -> Result<Draw, AppError> {
    let draw = Draw {
        raffle_id: raffle.raffle_id.clone(),
        drawn_at: now,
        tickets_sold: raffle.tickets_sold,
        shard_counts: counters.iter().map(|counter| counter.sold).collect(),
        method: METHOD.into(),
        conducted_by: request.conducted_by,
        witnessed_by: request.witnessed_by,
    };
    if !repo.record_draw(&draw).await? {
        return Err(AppError::Conflict(format!("raffle {} was drawn by another run", raffle.raffle_id)));
    }
    Ok(draw)
}

struct Pick {
    drawn: u64,
    shard: Option<u32>,
    ticket_number: u64,
    entry: Entry,
}

fn physical(draw: &Draw, drawn: u64) -> Option<(Option<u32>, u64)> {
    if draw.shard_counts.is_empty() {
        return Some((None, drawn));
    }

    locate(&draw.shard_counts, drawn).map(|(shard, number)| (Some(shard), number))
}

fn draw_number_of(draw: &Draw, winner: &Winner) -> u64 {
    match winner.shard {
        None => winner.ticket_number,
        Some(shard) => draw_number(&draw.shard_counts, shard, winner.ticket_number),
    }
}

async fn pick_entry(repo: &DynamoRepo, draw: &Draw, random: &mut impl FnMut() -> u64, won: &HashSet<u64>) -> Result<Pick, AppError> {
    for _ in 0..MAX_REDRAWS_PER_PRIZE {
        let drawn = uniform(&mut *random, draw.tickets_sold);
        if won.contains(&drawn) {
            continue;
        }
        let Some((shard, ticket_number)) = physical(draw, drawn) else {
            continue;
        };
        let Some(entry) = repo.find_entry_by_ticket(&draw.raffle_id, shard, ticket_number).await? else {
            continue;
        };

        if repo.get_order(&entry.order_id).await?.is_some_and(|order| order.status == OrderStatus::Paid) {
            return Ok(Pick {
                drawn,
                shard,
                ticket_number,
                entry,
            });
        }
    }

    Err(AppError::Internal(format!("no eligible ticket after {MAX_REDRAWS_PER_PRIZE} draws")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::testing::{at, scripted, winter};

    fn raffle(tickets_sold: u64, drawn: bool) -> Raffle {
        let mut raffle = winter();
        raffle.drawn_at = drawn.then(|| at(2027, 1, 22));
        raffle.tickets_sold = tickets_sold;
        raffle.ticket_revenue_pence = tickets_sold * 100;
        raffle
    }

    fn prize(rank: u32, amount_pence: u64, quantity: u32) -> Prize {
        shared::testing::prize("winter-2026", rank, amount_pence, quantity)
    }

    #[test]
    fn uniform_maps_into_range_and_rejects_biased_tail() {
        assert_eq!(uniform(scripted(vec![0]), 30), 1);
        assert_eq!(uniform(scripted(vec![29]), 30), 30);
        assert_eq!(uniform(scripted(vec![30]), 30), 1);
        assert_eq!(uniform(scripted(vec![u64::MAX, 7]), 30), 8);
        assert_eq!(uniform(scripted(vec![u64::MAX - 1, 42]), 1), 1);
    }

    #[test]
    fn prize_slots_follow_rank_order_and_quantity() {
        let prizes = vec![prize(2, 500_000, 1), prize(3, 100_000, 2), prize(1, 2_000_000, 1)];
        let ranks: Vec<u32> = prize_slots(&prizes).iter().map(|prize| prize.rank).collect();
        assert_eq!(ranks, vec![1, 2, 3, 3]);
        assert!(prize_slots(&[]).is_empty());
    }

    #[test]
    fn draw_state_requires_closed_raffle_past_draw_date_with_sales() {
        assert_eq!(draw_state(&raffle(30, false), at(2027, 1, 23)).unwrap(), DrawState::Fresh);
        assert_eq!(draw_state(&raffle(30, true), at(2027, 1, 23)).unwrap(), DrawState::Resume);
        assert!(matches!(draw_state(&raffle(30, false), at(2027, 1, 21)), Err(AppError::Conflict(_))));
        assert!(matches!(draw_state(&raffle(0, false), at(2027, 1, 23)), Err(AppError::Conflict(_))));
        assert!(matches!(draw_state(&raffle(30, false), at(2026, 12, 1)), Err(AppError::Conflict(_))));
        assert!(matches!(draw_state(&raffle(30, false), at(2026, 9, 1)), Err(AppError::Conflict(_))));
    }
}
