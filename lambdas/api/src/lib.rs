use chrono::{DateTime, NaiveDate, Utc};
use lambda_http::http::Method;
use lambda_http::http::header::{CACHE_CONTROL, HeaderValue};
use lambda_http::{Body, Request, Response};
use serde::{Deserialize, Serialize};
use shared::entrant::{Address, Entrant, GiftAidDeclaration, MarketingConsent};
use shared::error::AppError;
use shared::order::{Order, OrderStatus, validate_purchase};
use shared::raffle::{Prize, Raffle, RaffleStatus};
use shared::random;
use shared::stripe::{PaymentGateway, PaymentIntentRequest};
use shared::table::DynamoRepo;

pub const CONSENT_WORDING: &str = "web-checkout-2026-09";
const GIFT_AID_WORDING: &str = "gift-aid-2026-09";
const CONSENT_SOURCE: &str = "web-checkout";
const MAX_DONATION_PENCE: u64 = 1_000_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateOrderRequest {
    pub ticket_quantity: u32,
    #[serde(default)]
    pub donation_pence: u64,
    pub gift_aid: bool,
    #[serde(default)]
    pub subscribe: bool,
    pub entrant: EntrantInput,
    pub marketing: MarketingInput,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntrantInput {
    pub title: String,
    pub first_name: String,
    pub last_name: String,
    pub email: String,
    #[serde(default)]
    pub telephone: Option<String>,
    pub date_of_birth: NaiveDate,
    pub address: Address,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct MarketingInput {
    pub email: bool,
    pub post: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateOrderResponse {
    pub order_id: String,
    pub entrant_id: String,
    pub ticket_quantity: u32,
    pub ticket_amount_pence: u64,
    pub donation_pence: u64,
    pub total_pence: u64,
    pub client_secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RaffleView {
    #[serde(flatten)]
    pub raffle: Raffle,
    pub status: RaffleStatus,
    pub prizes: Vec<Prize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketRange {
    pub from: u64,
    pub to: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderView {
    pub order_id: String,
    pub status: OrderStatus,
    pub ticket_quantity: u32,
    pub total_pence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tickets: Option<TicketRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentRaffles {
    pub current: Option<RaffleView>,
    pub next: Option<RaffleView>,
}

pub fn pick_current(raffles: &[Raffle], now: DateTime<Utc>) -> (Option<&Raffle>, Option<&Raffle>) {
    let open = raffles.iter().find(|raffle| raffle.status_at(now) == RaffleStatus::Open);
    let latest_finished = raffles
        .iter()
        .filter(|raffle| matches!(raffle.status_at(now), RaffleStatus::Closed | RaffleStatus::Drawn))
        .max_by_key(|raffle| raffle.closes_at);
    let next = raffles
        .iter()
        .filter(|raffle| raffle.status_at(now) == RaffleStatus::Scheduled)
        .min_by_key(|raffle| raffle.opens_at);

    (open.or(latest_finished), next)
}

pub fn validate_input(input: &EntrantInput) -> Result<(), AppError> {
    let required = [
        ("title", &input.title),
        ("firstName", &input.first_name),
        ("lastName", &input.last_name),
        ("email", &input.email),
        ("address.line1", &input.address.line1),
        ("address.town", &input.address.town),
        ("address.postcode", &input.address.postcode),
    ];
    if let Some((name, _)) = required.iter().find(|(_, value)| value.trim().is_empty()) {
        return Err(AppError::BadRequest(format!("{name} is required")));
    }
    if !input.email.contains('@') {
        return Err(AppError::BadRequest("email is invalid".into()));
    }
    Ok(())
}

pub fn merge_entrant(existing: Option<Entrant>, input: EntrantInput, now: DateTime<Utc>) -> Entrant {
    let (entrant_id, created_at, stripe_customer_id, self_excluded_until) = match existing {
        Some(entrant) => (entrant.entrant_id, entrant.created_at, entrant.stripe_customer_id, entrant.self_excluded_until),
        None => (random::id("ent"), now, None, None),
    };

    Entrant {
        entrant_id,
        title: input.title.trim().to_string(),
        first_name: input.first_name.trim().to_string(),
        last_name: input.last_name.trim().to_string(),
        email: input.email.trim().to_string(),
        telephone: input
            .telephone
            .map(|telephone| telephone.trim().to_string())
            .filter(|telephone| !telephone.is_empty()),
        date_of_birth: input.date_of_birth,
        address: input.address,
        stripe_customer_id,
        self_excluded_until,
        erased_at: None,
        created_at,
    }
}

pub struct Api<G> {
    repo: DynamoRepo,
    gateway: G,
}

impl<G: PaymentGateway> Api<G> {
    pub fn new(repo: DynamoRepo, gateway: G) -> Self {
        Self { repo, gateway }
    }

    pub async fn handle(&self, request: Request) -> Response<Body> {
        self.route(&request, Utc::now()).await.unwrap_or_else(|err| {
            let status = err.status_code();
            let method = request.method().as_str();
            let path = request.uri().path();

            if status >= 500 {
                tracing::error!(status, method, path, error = %err, "request failed");
            } else {
                tracing::warn!(status, method, path, error = %err, "request rejected");
            }
            json(err.status_code(), &serde_json::json!({ "error": err.public_message() }))
        })
    }

    async fn route(&self, request: &Request, now: DateTime<Utc>) -> Result<Response<Body>, AppError> {
        let segments: Vec<&str> = request.uri().path().trim_matches('/').split('/').collect();

        match (request.method(), segments.as_slice()) {
            (&Method::GET, ["raffles", "current"]) => Ok(json(200, &self.current(now).await?)),
            (&Method::GET, ["orders", order_id]) => {
                let mut response = json(200, &self.order(order_id).await?);
                response.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
                Ok(response)
            }
            (&Method::POST, ["raffles", raffle_id, "orders"]) => {
                let body = serde_json::from_slice(request.body().as_ref()).map_err(|err| AppError::BadRequest(format!("invalid body: {err}")))?;
                Ok(json(201, &self.create_order(raffle_id, body, now).await?))
            }
            _ => Err(AppError::NotFound("route".into())),
        }
    }

    pub async fn current(&self, now: DateTime<Utc>) -> Result<CurrentRaffles, AppError> {
        let raffles = self.repo.list_raffles().await?;
        let (current, next) = pick_current(&raffles, now);

        Ok(CurrentRaffles {
            current: self.view(current, now).await?,
            next: self.view(next, now).await?,
        })
    }

    pub async fn order(&self, order_id: &str) -> Result<OrderView, AppError> {
        let Some(order) = self.repo.get_order(order_id).await? else {
            return Err(AppError::NotFound(format!("order {order_id}")));
        };

        let tickets = if matches!(order.status, OrderStatus::Paid | OrderStatus::Refunded) {
            let entries = self.repo.list_entries_for_entrant(&order.entrant_id).await?;
            entries.into_iter().find(|entry| entry.order_id == order.order_id).map(|entry| TicketRange {
                from: entry.ticket_from,
                to: entry.ticket_to,
            })
        } else {
            None
        };

        Ok(OrderView {
            order_id: order.order_id,
            status: order.status,
            ticket_quantity: order.ticket_quantity,
            total_pence: order.total_pence,
            tickets,
        })
    }

    async fn view(&self, raffle: Option<&Raffle>, now: DateTime<Utc>) -> Result<Option<RaffleView>, AppError> {
        let Some(raffle) = raffle else {
            return Ok(None);
        };
        let prizes = self.repo.list_prizes(&raffle.raffle_id).await?;
        Ok(Some(RaffleView {
            status: raffle.status_at(now),
            raffle: raffle.clone(),
            prizes,
        }))
    }

    pub async fn create_order(&self, raffle_id: &str, request: CreateOrderRequest, now: DateTime<Utc>) -> Result<CreateOrderResponse, AppError> {
        let CreateOrderRequest {
            ticket_quantity,
            donation_pence,
            gift_aid,
            subscribe,
            entrant: input,
            marketing,
        } = request;
        validate_input(&input)?;
        if donation_pence > MAX_DONATION_PENCE {
            return Err(AppError::BadRequest(format!("donationPence must not exceed {MAX_DONATION_PENCE}")));
        }
        let Some(raffle) = self.repo.get_raffle(raffle_id).await? else {
            return Err(AppError::NotFound(format!("raffle {raffle_id}")));
        };

        let existing = self.repo.find_entrant_by_email(&input.email).await?;
        let mut entrant = merge_entrant(existing, input, now);
        validate_purchase(&raffle, &entrant, ticket_quantity, now)?;

        if subscribe && entrant.stripe_customer_id.is_none() {
            let name = format!("{} {}", entrant.first_name, entrant.last_name);
            entrant.stripe_customer_id = Some(self.gateway.create_customer(&entrant.email, &name, &entrant.entrant_id).await?);
        }
        self.record_supporter(&entrant, marketing, gift_aid, now).await?;

        let mut order = Order::single(random::id("ord"), &raffle, &entrant.entrant_id, ticket_quantity, donation_pence, gift_aid, now);
        order.subscribe = subscribe;

        let intent = self.gateway.create_payment_intent(payment_intent_request(&order, &raffle, &entrant)).await?;
        order.stripe_payment_intent_id = Some(intent.id);
        self.repo.create_order(&order).await?;

        Ok(CreateOrderResponse {
            order_id: order.order_id,
            entrant_id: entrant.entrant_id,
            ticket_quantity: order.ticket_quantity,
            ticket_amount_pence: order.ticket_amount_pence,
            donation_pence: order.donation_pence,
            total_pence: order.total_pence,
            client_secret: intent.client_secret,
        })
    }

    async fn record_supporter(&self, entrant: &Entrant, marketing: MarketingInput, gift_aid: bool, now: DateTime<Utc>) -> Result<(), AppError> {
        let consent = MarketingConsent {
            entrant_id: entrant.entrant_id.clone(),
            recorded_at: now,
            email: marketing.email,
            post: marketing.post,
            wording_version: CONSENT_WORDING.into(),
            source: CONSENT_SOURCE.into(),
        };
        let declaration = GiftAidDeclaration::from_entrant(entrant, gift_aid, GIFT_AID_WORDING, now);

        self.repo.put_entrant(entrant).await?;
        self.repo.put_marketing_consent(&consent).await?;
        self.repo.put_gift_aid_declaration(&declaration).await
    }
}

fn payment_intent_request(order: &Order, raffle: &Raffle, entrant: &Entrant) -> PaymentIntentRequest {
    PaymentIntentRequest {
        amount_pence: order.total_pence,
        customer_id: entrant.stripe_customer_id.clone(),
        save_payment_method: order.subscribe,
        idempotency_key: order.order_id.clone(),
        description: format!("{}: {} tickets", raffle.name, order.ticket_quantity),
        metadata: vec![
            ("orderId".into(), order.order_id.clone()),
            ("raffleId".into(), raffle.raffle_id.clone()),
            ("entrantId".into(), entrant.entrant_id.clone()),
        ],
    }
}

fn json<T: Serialize>(status: u16, body: &T) -> Response<Body> {
    let payload = serde_json::to_string(body).unwrap_or_else(|_| "{}".to_string());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(payload))
        .unwrap_or_else(|_| Response::new(Body::Empty))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::testing::{at, winter};

    fn raffle(id: &str, opens: DateTime<Utc>, closes: DateTime<Utc>) -> Raffle {
        let mut raffle = winter();
        raffle.raffle_id = id.into();
        raffle.opens_at = opens;
        raffle.closes_at = closes;
        raffle
    }

    fn input(email: &str) -> EntrantInput {
        EntrantInput {
            title: " Ms ".into(),
            first_name: "Ada".into(),
            last_name: "Lovelace".into(),
            email: email.into(),
            telephone: Some("  ".into()),
            date_of_birth: NaiveDate::from_ymd_opt(1990, 1, 1).unwrap(),
            address: Address {
                line1: "1 Road".into(),
                line2: None,
                town: "London".into(),
                postcode: "SW1A 1AA".into(),
                country: "GB".into(),
            },
        }
    }

    #[test]
    fn current_prefers_open_then_latest_finished_and_next_is_earliest_scheduled() {
        let summer = raffle("summer", at(2026, 4, 1), at(2026, 7, 1));
        let autumn = raffle("autumn", at(2026, 7, 2), at(2026, 9, 1));
        let winter = raffle("winter", at(2026, 9, 30), at(2027, 1, 8));
        let spring = raffle("spring", at(2027, 1, 9), at(2027, 4, 1));
        let raffles = vec![summer, autumn, winter, spring];

        let (current, next) = pick_current(&raffles, at(2026, 9, 9));
        assert_eq!(current.map(|r| r.raffle_id.as_str()), Some("autumn"));
        assert_eq!(next.map(|r| r.raffle_id.as_str()), Some("winter"));

        let (current, next) = pick_current(&raffles, at(2026, 10, 1));
        assert_eq!(current.map(|r| r.raffle_id.as_str()), Some("winter"));
        assert_eq!(next.map(|r| r.raffle_id.as_str()), Some("spring"));

        let (current, next) = pick_current(&raffles, at(2027, 5, 1));
        assert_eq!(current.map(|r| r.raffle_id.as_str()), Some("spring"));
        assert!(next.is_none());

        assert_eq!(pick_current(&[], at(2026, 9, 9)), (None, None));
    }

    #[test]
    fn merge_keeps_identity_customer_and_exclusion_of_a_returning_entrant() {
        let now = at(2026, 10, 1);
        let fresh = merge_entrant(None, input("ada@example.com"), now);
        assert!(fresh.entrant_id.starts_with("ent_"));
        assert_eq!(fresh.title, "Ms");
        assert!(fresh.telephone.is_none());
        assert_eq!(fresh.created_at, now);

        let mut existing = fresh.clone();
        existing.stripe_customer_id = Some("cus_1".into());
        existing.self_excluded_until = NaiveDate::from_ymd_opt(2030, 1, 1);
        let later = at(2027, 1, 1);
        let merged = merge_entrant(Some(existing.clone()), input("ada@example.com"), later);
        assert_eq!(merged.entrant_id, existing.entrant_id);
        assert_eq!(merged.created_at, now);
        assert_eq!(merged.stripe_customer_id.as_deref(), Some("cus_1"));
        assert_eq!(merged.self_excluded_until, existing.self_excluded_until);
    }

    #[test]
    fn input_validation_requires_identity_and_address_fields() {
        assert!(validate_input(&input("ada@example.com")).is_ok());
        assert!(matches!(validate_input(&input("not-an-email")), Err(AppError::BadRequest(_))));
        let mut blank = input("ada@example.com");
        blank.last_name = "  ".into();
        assert!(matches!(validate_input(&blank), Err(AppError::BadRequest(_))));
        let mut no_postcode = input("ada@example.com");
        no_postcode.address.postcode = String::new();
        assert!(matches!(validate_input(&no_postcode), Err(AppError::BadRequest(_))));
    }
}
