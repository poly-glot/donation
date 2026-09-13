use chrono::{DateTime, Utc};
use lambda_http::{Body, Request, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shared::auth::Cognito;
use shared::draw::{Draw, Winner, WinnerStatus};
use shared::entrant::{Entrant, GiftAidDeclaration, MarketingConsent};
use shared::error::AppError;
use shared::http::{answered, authorise, body};
use shared::order::{Entry, Order};
use shared::raffle::{Prize, Raffle, RaffleStatus};
use shared::subscription::{Subscription, SubscriptionStatus};
use shared::table::{DynamoRepo, page_cursor, page_key};

const PAGE_SIZE: i32 = 50;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum AdminRequest {
    ListRaffles,
    GetRaffle { raffle_id: String },
    ListEntries { raffle_id: String, cursor: Option<String> },
    GetOrder { order_id: String },
    FindTicket { raffle_id: String, ticket_number: u64 },
    FindEntrant { email: String },
    GetEntrant { entrant_id: String },
    ListSubscriptions { status: SubscriptionStatus, cursor: Option<String> },
    CreateRaffle(RaffleInput),
    UpdateRaffle(RaffleInput),
    PutPrize(Prize),
    RemovePrize { raffle_id: String, rank: u32 },
    SetWinnerStatus { raffle_id: String, sequence: u32, status: WinnerStatus },
    CancelSubscription { subscription_id: String },
    EraseEntrant { entrant_id: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RaffleInput {
    pub raffle_id: String,
    pub name: String,
    pub ticket_price_pence: u64,
    pub max_tickets_per_order: u32,
    pub max_tickets: u64,
    pub opens_at: DateTime<Utc>,
    pub closes_at: DateTime<Utc>,
    pub draw_at: DateTime<Utc>,
    pub results_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RaffleRow {
    #[serde(flatten)]
    raffle: Raffle,
    status: RaffleStatus,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RaffleDetail {
    #[serde(flatten)]
    raffle: Raffle,
    status: RaffleStatus,
    prizes: Vec<Prize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    draw: Option<Draw>,
    winners: Vec<Winner>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OrderDetail {
    #[serde(flatten)]
    order: Order,
    #[serde(skip_serializing_if = "Option::is_none")]
    entry: Option<Entry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entrant: Option<Entrant>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EntrantDossier {
    #[serde(flatten)]
    entrant: Entrant,
    orders: Vec<Order>,
    entries: Vec<Entry>,
    subscriptions: Vec<Subscription>,
    winners: Vec<Winner>,
    #[serde(skip_serializing_if = "Option::is_none")]
    consent: Option<MarketingConsent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gift_aid_declaration: Option<GiftAidDeclaration>,
}

impl RaffleInput {
    pub fn into_raffle(self, now: DateTime<Utc>) -> Raffle {
        Raffle {
            raffle_id: self.raffle_id,
            name: self.name,
            ticket_price_pence: self.ticket_price_pence,
            max_tickets_per_order: self.max_tickets_per_order,
            max_tickets: self.max_tickets,
            opens_at: self.opens_at,
            closes_at: self.closes_at,
            draw_at: self.draw_at,
            results_at: self.results_at,
            drawn_at: None,
            tickets_sold: 0,
            ticket_revenue_pence: 0,
            donation_pence: 0,
            subscriptions_charged_at: None,
            created_at: now,
        }
    }
}

pub fn updated_raffle(existing: &Raffle, input: RaffleInput) -> Result<Raffle, AppError> {
    if existing.tickets_sold > 0 && input.ticket_price_pence != existing.ticket_price_pence {
        return Err(AppError::BadRequest("ticketPricePence cannot change once tickets are sold".into()));
    }

    let raffle = Raffle {
        drawn_at: existing.drawn_at,
        tickets_sold: existing.tickets_sold,
        ticket_revenue_pence: existing.ticket_revenue_pence,
        donation_pence: existing.donation_pence,
        subscriptions_charged_at: existing.subscriptions_charged_at,
        ..input.into_raffle(existing.created_at)
    };
    validate_raffle(&raffle)?;
    Ok(raffle)
}

pub fn validate_raffle(raffle: &Raffle) -> Result<(), AppError> {
    let id_ok = !raffle.raffle_id.is_empty() && raffle.raffle_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !id_ok {
        return Err(AppError::BadRequest("raffleId must be alphanumeric with - or _".into()));
    }
    if raffle.name.trim().is_empty() {
        return Err(AppError::BadRequest("name is required".into()));
    }
    if raffle.ticket_price_pence == 0 || raffle.max_tickets_per_order == 0 || raffle.max_tickets == 0 {
        return Err(AppError::BadRequest(
            "ticketPricePence, maxTicketsPerOrder and maxTickets must be positive".into(),
        ));
    }
    if raffle.max_tickets < raffle.tickets_sold {
        return Err(AppError::BadRequest("maxTickets is below tickets already sold".into()));
    }
    let ordered = raffle.opens_at < raffle.closes_at && raffle.closes_at <= raffle.draw_at && raffle.draw_at <= raffle.results_at;
    if !ordered {
        return Err(AppError::BadRequest("dates must satisfy opensAt < closesAt <= drawAt <= resultsAt".into()));
    }
    Ok(())
}

pub struct Console {
    repo: DynamoRepo,
    cognito: Cognito,
}

impl Console {
    pub fn new(repo: DynamoRepo, cognito: Cognito) -> Self {
        Self { repo, cognito }
    }

    pub async fn handle(&self, request: Request) -> Response<Body> {
        answered(self.act(request, Utc::now()).await)
    }

    async fn act(&self, request: Request, now: DateTime<Utc>) -> Result<Value, AppError> {
        let claims = authorise(&self.cognito, &request, now).await?;
        tracing::info!(subject = %claims.sub, "admin request authorised");

        run(&self.repo, body(&request)?, now).await
    }
}

pub async fn run(repo: &DynamoRepo, request: AdminRequest, now: DateTime<Utc>) -> Result<Value, AppError> {
    match request {
        AdminRequest::ListRaffles => list_raffles(repo, now).await,
        AdminRequest::GetRaffle { raffle_id } => get_raffle(repo, &raffle_id, now).await,
        AdminRequest::ListEntries { raffle_id, cursor } => list_entries(repo, &raffle_id, cursor).await,
        AdminRequest::GetOrder { order_id } => get_order(repo, &order_id).await,
        AdminRequest::FindTicket { raffle_id, ticket_number } => find_ticket(repo, &raffle_id, ticket_number).await,
        AdminRequest::FindEntrant { email } => find_entrant(repo, &email).await,
        AdminRequest::GetEntrant { entrant_id } => get_entrant(repo, &entrant_id).await,
        AdminRequest::ListSubscriptions { status, cursor } => list_subscriptions(repo, status, cursor).await,
        AdminRequest::CreateRaffle(input) => create_raffle(repo, input, now).await,
        AdminRequest::UpdateRaffle(input) => update_raffle(repo, input).await,
        AdminRequest::PutPrize(prize) => put_prize(repo, prize, now).await,
        AdminRequest::RemovePrize { raffle_id, rank } => remove_prize(repo, &raffle_id, rank, now).await,
        AdminRequest::SetWinnerStatus { raffle_id, sequence, status } => set_winner_status(repo, &raffle_id, sequence, status).await,
        AdminRequest::CancelSubscription { subscription_id } => cancel_subscription(repo, &subscription_id).await,
        AdminRequest::EraseEntrant { entrant_id } => erase_entrant(repo, &entrant_id, now).await,
    }
}

async fn list_raffles(repo: &DynamoRepo, now: DateTime<Utc>) -> Result<Value, AppError> {
    let rows: Vec<RaffleRow> = repo
        .list_raffles()
        .await?
        .into_iter()
        .map(|raffle| RaffleRow {
            status: raffle.status_at(now),
            raffle,
        })
        .collect();

    Ok(json!(rows))
}

async fn get_raffle(repo: &DynamoRepo, raffle_id: &str, now: DateTime<Utc>) -> Result<Value, AppError> {
    let Some(raffle) = repo.get_raffle(raffle_id).await? else {
        return Err(AppError::NotFound(format!("raffle {raffle_id}")));
    };

    let prizes = repo.list_prizes(raffle_id).await?;
    let draw = repo.get_draw(raffle_id).await?;
    let winners = repo.list_winners(raffle_id).await?;

    Ok(json!(RaffleDetail {
        status: raffle.status_at(now),
        raffle,
        prizes,
        draw,
        winners,
    }))
}

async fn list_entries(repo: &DynamoRepo, raffle_id: &str, cursor: Option<String>) -> Result<Value, AppError> {
    let start = cursor.as_deref().map(page_key).transpose()?;
    let (entries, last_key) = repo.list_entries(raffle_id, PAGE_SIZE, start).await?;

    Ok(json!({ "entries": entries, "cursor": last_key.as_ref().map(page_cursor).transpose()? }))
}

async fn get_order(repo: &DynamoRepo, order_id: &str) -> Result<Value, AppError> {
    let Some(order) = repo.get_order(order_id).await? else {
        return Err(AppError::NotFound(format!("order {order_id}")));
    };
    let entry = repo
        .list_entries_for_entrant(&order.entrant_id)
        .await?
        .into_iter()
        .find(|entry| entry.order_id == order.order_id);

    order_detail(repo, order, entry).await
}

async fn find_ticket(repo: &DynamoRepo, raffle_id: &str, ticket_number: u64) -> Result<Value, AppError> {
    let Some(entry) = repo.find_entry_by_ticket(raffle_id, ticket_number).await? else {
        return Err(AppError::NotFound(format!("ticket {ticket_number} of raffle {raffle_id}")));
    };
    let Some(order) = repo.get_order(&entry.order_id).await? else {
        return Err(AppError::NotFound(format!("order {}", entry.order_id)));
    };

    order_detail(repo, order, Some(entry)).await
}

async fn order_detail(repo: &DynamoRepo, order: Order, entry: Option<Entry>) -> Result<Value, AppError> {
    let entrant = repo.get_entrant(&order.entrant_id).await?;

    Ok(json!(OrderDetail { order, entry, entrant }))
}

async fn find_entrant(repo: &DynamoRepo, email: &str) -> Result<Value, AppError> {
    let Some(entrant) = repo.find_entrant_by_email(email).await? else {
        return Err(AppError::NotFound("entrant with that email".into()));
    };

    entrant_dossier(repo, entrant).await
}

async fn get_entrant(repo: &DynamoRepo, entrant_id: &str) -> Result<Value, AppError> {
    let Some(entrant) = repo.get_entrant(entrant_id).await? else {
        return Err(AppError::NotFound(format!("entrant {entrant_id}")));
    };

    entrant_dossier(repo, entrant).await
}

async fn entrant_dossier(repo: &DynamoRepo, entrant: Entrant) -> Result<Value, AppError> {
    let entrant_id = entrant.entrant_id.as_str();

    let orders = repo.list_orders_for_entrant(entrant_id).await?;
    let entries = repo.list_entries_for_entrant(entrant_id).await?;
    let subscriptions = repo.list_subscriptions_for_entrant(entrant_id).await?;
    let winners = repo.list_winners_for_entrant(entrant_id).await?;
    let consent = repo.latest_marketing_consent(entrant_id).await?;
    let gift_aid_declaration = repo.latest_gift_aid_declaration(entrant_id).await?;

    Ok(json!(EntrantDossier {
        entrant,
        orders,
        entries,
        subscriptions,
        winners,
        consent,
        gift_aid_declaration,
    }))
}

async fn list_subscriptions(repo: &DynamoRepo, status: SubscriptionStatus, cursor: Option<String>) -> Result<Value, AppError> {
    let start = cursor.as_deref().map(page_key).transpose()?;
    let (subscriptions, last_key) = repo.list_subscriptions(status, PAGE_SIZE, start).await?;

    Ok(json!({ "subscriptions": subscriptions, "cursor": last_key.as_ref().map(page_cursor).transpose()? }))
}

async fn create_raffle(repo: &DynamoRepo, input: RaffleInput, now: DateTime<Utc>) -> Result<Value, AppError> {
    let raffle = input.into_raffle(now);
    validate_raffle(&raffle)?;
    if !repo.create_raffle(&raffle).await? {
        return Err(AppError::Conflict(format!("raffle {} already exists", raffle.raffle_id)));
    }
    Ok(json!(raffle))
}

async fn update_raffle(repo: &DynamoRepo, input: RaffleInput) -> Result<Value, AppError> {
    let Some(existing) = repo.get_raffle(&input.raffle_id).await? else {
        return Err(AppError::NotFound(format!("raffle {}", input.raffle_id)));
    };
    let raffle = updated_raffle(&existing, input)?;

    if !repo.replace_raffle(&raffle, existing.tickets_sold).await? {
        return Err(AppError::Conflict("raffle changed while updating, retry".into()));
    }
    Ok(json!(raffle))
}

async fn put_prize(repo: &DynamoRepo, prize: Prize, now: DateTime<Utc>) -> Result<Value, AppError> {
    if prize.quantity == 0 || prize.amount_pence == 0 {
        return Err(AppError::BadRequest("quantity and amountPence must be positive".into()));
    }
    validate_prize_change(repo, &prize.raffle_id, now).await?;

    repo.put_prize(&prize).await?;
    Ok(json!(prize))
}

async fn remove_prize(repo: &DynamoRepo, raffle_id: &str, rank: u32, now: DateTime<Utc>) -> Result<Value, AppError> {
    validate_prize_change(repo, raffle_id, now).await?;

    if !repo.delete_prize(raffle_id, rank).await? {
        return Err(AppError::NotFound(format!("prize {rank} of raffle {raffle_id}")));
    }
    Ok(json!({ "raffleId": raffle_id, "rank": rank, "removed": true }))
}

async fn validate_prize_change(repo: &DynamoRepo, raffle_id: &str, now: DateTime<Utc>) -> Result<(), AppError> {
    let Some(raffle) = repo.get_raffle(raffle_id).await? else {
        return Err(AppError::NotFound(format!("raffle {raffle_id}")));
    };
    if raffle.status_at(now) == RaffleStatus::Drawn {
        return Err(AppError::Conflict("prizes cannot change once the raffle is drawn".into()));
    }
    Ok(())
}

async fn set_winner_status(repo: &DynamoRepo, raffle_id: &str, sequence: u32, status: WinnerStatus) -> Result<Value, AppError> {
    if !repo.set_winner_status(raffle_id, sequence, status).await? {
        return Err(AppError::NotFound(format!("winner {sequence} of raffle {raffle_id}")));
    }
    Ok(json!({ "raffleId": raffle_id, "sequence": sequence, "status": status }))
}

async fn cancel_subscription(repo: &DynamoRepo, subscription_id: &str) -> Result<Value, AppError> {
    if !repo.set_subscription_status(subscription_id, SubscriptionStatus::Cancelled).await? {
        return Err(AppError::NotFound(format!("subscription {subscription_id}")));
    }
    Ok(json!({ "subscriptionId": subscription_id, "status": SubscriptionStatus::Cancelled }))
}

async fn erase_entrant(repo: &DynamoRepo, entrant_id: &str, now: DateTime<Utc>) -> Result<Value, AppError> {
    let unpaid_prize = repo
        .list_winners_for_entrant(entrant_id)
        .await?
        .iter()
        .any(|winner| matches!(winner.status, WinnerStatus::Pending | WinnerStatus::Notified));
    if unpaid_prize {
        return Err(AppError::Conflict("entrant has an unpaid prize".into()));
    }

    let subscriptions = repo.list_subscriptions_for_entrant(entrant_id).await?;
    let live = subscriptions.iter().filter(|subscription| subscription.status != SubscriptionStatus::Cancelled);

    let mut subscriptions_cancelled = 0;
    for subscription in live {
        repo.set_subscription_status(&subscription.subscription_id, SubscriptionStatus::Cancelled)
            .await?;
        subscriptions_cancelled += 1;
    }

    let erased = repo.erase_entrant(entrant_id, now).await?;
    Ok(json!({ "entrantId": entrant_id, "erased": erased, "subscriptionsCancelled": subscriptions_cancelled }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::testing::at;

    fn input() -> RaffleInput {
        RaffleInput {
            raffle_id: "winter-2026".into(),
            name: "Winter Poppy Raffle 2026".into(),
            ticket_price_pence: 100,
            max_tickets_per_order: 20,
            max_tickets: 5_000_000,
            opens_at: at(2026, 9, 30),
            closes_at: at(2027, 1, 8),
            draw_at: at(2027, 1, 22),
            results_at: at(2027, 2, 5),
        }
    }

    #[test]
    fn create_input_becomes_a_raffle_with_zeroed_counters() {
        let raffle = input().into_raffle(at(2026, 9, 1));
        assert!(validate_raffle(&raffle).is_ok());
        assert_eq!((raffle.tickets_sold, raffle.ticket_revenue_pence, raffle.donation_pence), (0, 0, 0));
        assert!(raffle.drawn_at.is_none());
    }

    #[test]
    fn validation_rejects_bad_ids_dates_and_limits() {
        let created = at(2026, 9, 1);
        let with_id = |raffle_id: &str| {
            let mut input = input();
            input.raffle_id = raffle_id.into();
            input.into_raffle(created)
        };

        let mut closes_before_open = input();
        closes_before_open.closes_at = at(2026, 9, 1);

        let mut draw_before_close = input();
        draw_before_close.draw_at = at(2027, 1, 7);

        let mut free_tickets = input();
        free_tickets.ticket_price_pence = 0;

        let mut sold_past_the_cap = input().into_raffle(created);
        sold_past_the_cap.tickets_sold = 10;
        sold_past_the_cap.max_tickets = 5;

        let cases = [
            ("a raffle id with a space in it", with_id("winter 2026")),
            ("a raffle that closes before it opens", closes_before_open.into_raffle(created)),
            ("a draw before the tickets stop selling", draw_before_close.into_raffle(created)),
            ("a free ticket", free_tickets.into_raffle(created)),
            ("a cap below the tickets already sold", sold_past_the_cap),
        ];
        for (label, raffle) in cases {
            let refused = validate_raffle(&raffle);
            assert!(matches!(&refused, Err(AppError::BadRequest(_))), "{label}: got {refused:?}");
        }

        assert!(validate_raffle(&with_id("winter-2026")).is_ok(), "a dash is allowed in a raffle id");
    }

    #[test]
    fn update_keeps_counters_and_freezes_the_price_after_sales() {
        let mut existing = input().into_raffle(at(2026, 9, 1));
        existing.tickets_sold = 10;
        existing.ticket_revenue_pence = 1_000;

        let mut change = input();
        change.name = "Renamed".into();
        change.closes_at = at(2027, 1, 15);
        let updated = updated_raffle(&existing, change).unwrap();
        assert_eq!((updated.name.as_str(), updated.closes_at), ("Renamed", at(2027, 1, 15)));
        assert_eq!((updated.tickets_sold, updated.ticket_revenue_pence), (10, 1_000));
        assert_eq!(updated.created_at, existing.created_at);

        let mut repriced = input();
        repriced.ticket_price_pence = 200;
        assert!(matches!(updated_raffle(&existing, repriced), Err(AppError::BadRequest(_))));
    }

    #[test]
    fn requests_deserialize_from_tagged_json() {
        let request: AdminRequest = serde_json::from_value(json!({
            "action": "setWinnerStatus", "raffleId": "winter-2026", "sequence": 3, "status": "PAID"
        }))
        .unwrap();
        assert!(matches!(
            request,
            AdminRequest::SetWinnerStatus {
                sequence: 3,
                status: WinnerStatus::Paid,
                ..
            }
        ));

        let request: AdminRequest = serde_json::from_value(json!({
            "action": "putPrize", "raffleId": "winter-2026", "rank": 1, "name": "First prize", "amountPence": 2000000, "quantity": 1
        }))
        .unwrap();
        assert!(matches!(request, AdminRequest::PutPrize(Prize { rank: 1, .. })));

        let request: AdminRequest = serde_json::from_value(json!({ "action": "eraseEntrant", "entrantId": "ent_1" })).unwrap();
        assert!(matches!(request, AdminRequest::EraseEntrant { .. }));
    }
}
