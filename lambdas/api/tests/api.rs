use std::sync::{Arc, Mutex};

use api::{Api, CreateOrderResponse, CurrentRaffles, OrderView, RaffleView, TicketRange};
use chrono::Utc;
use lambda_http::Body;
use lambda_http::http::{Method, Request, Response};
use serde_json::{Value, json};
use shared::order::OrderStatus;
use shared::raffle::RaffleStatus;
use shared::stripe::{IntentCreated, PaymentGateway, PaymentIntentRequest, StripeError};
use shared::table::DynamoRepo;
use shared::testing::{debit_payment, local_repo, prize, raffle, seed_raffle};

const ADULT: &str = "1990-01-01";
const DONATION_PENCE: u64 = 500;
const CHILD: &str = "2010-01-01";
const ENGLAND: &str = "SW1A 1AA";

type Intents = Arc<Mutex<Vec<PaymentIntentRequest>>>;

#[derive(Default)]
struct FakeStripe {
    intents: Intents,
}

impl PaymentGateway for FakeStripe {
    async fn create_customer(&self, _email: &str, _name: &str, entrant_id: &str) -> Result<String, StripeError> {
        Ok(format!("cus_{entrant_id}"))
    }

    async fn create_payment_intent(&self, request: PaymentIntentRequest) -> Result<IntentCreated, StripeError> {
        let id = format!("pi_{}", request.idempotency_key);
        self.intents.lock().unwrap().push(request);
        Ok(IntentCreated {
            client_secret: format!("{id}_secret"),
            id,
        })
    }
}

async fn api() -> Option<(Api<FakeStripe>, DynamoRepo, Intents)> {
    let repo = local_repo("api-test").await?;
    let stripe = FakeStripe::default();
    let intents = Arc::clone(&stripe.intents);
    Some((Api::new(repo.clone(), stripe), repo, intents))
}

fn order_body(email: &str, quantity: u32, date_of_birth: &str, postcode: &str) -> Value {
    json!({
        "ticketQuantity": quantity,
        "donationPence": DONATION_PENCE,
        "giftAid": true,
        "entrant": {
            "title": "Ms", "firstName": "Ada", "lastName": "Lovelace", "email": email,
            "telephone": "0345 845 1945", "dateOfBirth": date_of_birth,
            "address": { "line1": "199 Borough High St", "town": "London", "postcode": postcode, "country": "GB" }
        },
        "marketing": { "email": true, "post": false }
    })
}

fn eligible_order(email: &str, quantity: u32) -> Value {
    order_body(email, quantity, ADULT, ENGLAND)
}

fn order_donating(email: &str, donation_pence: u64) -> Value {
    let mut body = eligible_order(email, 5);
    body["donationPence"] = json!(donation_pence);
    body
}

fn post(path: &str, body: &Value) -> Request<Body> {
    Request::builder().method(Method::POST).uri(path).body(Body::from(body.to_string())).unwrap()
}

fn get(path: &str) -> Request<Body> {
    Request::builder().method(Method::GET).uri(path).body(Body::Empty).unwrap()
}

fn read(response: Response<Body>) -> (u16, Value) {
    let status = response.status().as_u16();
    let body = serde_json::from_slice(response.body().as_ref()).unwrap_or(Value::Null);
    (status, body)
}

async fn posted(api: &Api<FakeStripe>, path: &str, body: &Value) -> (u16, Value) {
    read(api.handle(post(path, body)).await)
}

async fn fetched(api: &Api<FakeStripe>, path: &str) -> (u16, Value) {
    read(api.handle(get(path)).await)
}

async fn create_order(api: &Api<FakeStripe>, raffle_id: &str, body: &Value) -> CreateOrderResponse {
    let (status, response) = posted(api, &format!("/raffles/{raffle_id}/orders"), body).await;
    assert_eq!(status, 201, "{response}");
    serde_json::from_value(response).unwrap()
}

async fn current_raffles(api: &Api<FakeStripe>, path: &str) -> CurrentRaffles {
    let (status, body) = fetched(api, path).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_value(body).unwrap()
}

fn summary(view: Option<&RaffleView>) -> (&str, RaffleStatus, usize) {
    let view = view.expect("a raffle in the view");
    (view.raffle.raffle_id.as_str(), view.status, view.prizes.len())
}

async fn order_view(api: &Api<FakeStripe>, order_id: &str) -> OrderView {
    let response = api.handle(get(&format!("/orders/{order_id}"))).await;
    let cache_control = response.headers().get("cache-control").and_then(|value| value.to_str().ok());
    assert_eq!(cache_control, Some("no-store"), "an order view carries personal data and must not be cached");

    let (status, body) = read(response);
    assert_eq!(status, 200, "{body}");
    serde_json::from_value(body).unwrap()
}

fn recorded(intents: &Intents) -> Vec<PaymentIntentRequest> {
    intents.lock().unwrap().clone()
}

async fn assert_rejected(api: &Api<FakeStripe>, path: &str, body: &Value, expected_status: u16, label: &str) {
    let (status, response) = posted(api, path, body).await;
    assert_eq!(status, expected_status, "{label}: {response}");
    assert!(response["error"].is_string(), "{label}: {response}");
}

#[tokio::test]
async fn current_raffle_endpoint_reports_the_status_and_prizes_of_current_and_next() {
    let Some((api, repo, _)) = api().await else {
        return;
    };
    let now = Utc::now();
    seed_raffle(&repo, &raffle("autumn", -100, -10, now)).await;
    seed_raffle(&repo, &raffle("winter", 5, 100, now)).await;
    repo.put_prize(&prize("winter", 1, 2_000_000, 1)).await.unwrap();

    let view = current_raffles(&api, "/raffles/current").await;
    assert_eq!(summary(view.current.as_ref()), ("autumn", RaffleStatus::Closed, 0));
    assert_eq!(summary(view.next.as_ref()), ("winter", RaffleStatus::Scheduled, 1));

    let trailing_slash = current_raffles(&api, "/raffles/current/").await;
    assert_eq!(trailing_slash, view, "the route trims a trailing slash");
}

#[tokio::test]
async fn a_checkout_writes_a_pending_order_the_entrant_and_their_consent_records() {
    let Some((api, repo, intents)) = api().await else {
        return;
    };
    let now = Utc::now();
    seed_raffle(&repo, &raffle("winter", -1, 100, now)).await;

    let created = create_order(&api, "winter", &eligible_order("ada@example.com", 15)).await;
    let charged = (created.ticket_amount_pence, created.donation_pence, created.total_pence);
    assert_eq!(charged, (1_500, DONATION_PENCE, 2_000), "15 tickets at 100p plus the donation");

    let intent_id = format!("pi_{}", created.order_id);
    assert_eq!(created.client_secret, format!("{intent_id}_secret"), "the secret of the intent for this order");

    let order = repo.get_order(&created.order_id).await.unwrap().unwrap();
    assert_eq!(order.status, OrderStatus::Pending, "tickets are allocated by the webhook, not here");
    assert_eq!(order.stripe_payment_intent_id.as_deref(), Some(intent_id.as_str()));
    assert_eq!((order.gift_aid, order.subscribe), (true, false));
    assert_eq!(repo.find_order_by_payment_intent(&intent_id).await.unwrap().unwrap().order_id, created.order_id);

    let entrant = repo.get_entrant(&created.entrant_id).await.unwrap().unwrap();
    assert_eq!(entrant.email, "ada@example.com");
    assert_eq!(entrant.telephone.as_deref(), Some("0345 845 1945"));

    let consent = repo.latest_marketing_consent(&entrant.entrant_id).await.unwrap().unwrap();
    assert_eq!((consent.email, consent.post), (true, false));
    assert_eq!(consent.wording_version, api::CONSENT_WORDING);

    let declaration = repo.latest_gift_aid_declaration(&entrant.entrant_id).await.unwrap().unwrap();
    assert!(declaration.is_uk_taxpayer);
    assert_eq!(declaration.donor.address.postcode, ENGLAND);

    let sent = recorded(&intents);
    let [intent] = sent.as_slice() else {
        panic!("expected one PaymentIntent for the order, got {sent:?}");
    };
    assert_eq!(intent.amount_pence, 2_000, "Stripe is asked for the tickets and the donation together");
    assert_eq!((intent.customer_id.as_deref(), intent.save_payment_method), (None, false));
    assert!(intent.metadata.contains(&("orderId".to_string(), created.order_id.clone())));
}

#[tokio::test]
async fn the_same_email_in_any_case_belongs_to_one_entrant() {
    let Some((api, repo, _)) = api().await else {
        return;
    };
    let now = Utc::now();
    seed_raffle(&repo, &raffle("winter", -1, 100, now)).await;

    let first = create_order(&api, "winter", &eligible_order("Ada@Example.com", 15)).await;
    let second = create_order(&api, "winter", &eligible_order("ada@example.com", 5)).await;

    assert_eq!(second.entrant_id, first.entrant_id);
    assert_ne!(second.order_id, first.order_id);
    assert_eq!(repo.list_orders_for_entrant(&first.entrant_id).await.unwrap().len(), 2);
}

#[tokio::test]
async fn subscribe_creates_a_stripe_customer_and_saves_the_card_on_the_intent() {
    let Some((api, repo, intents)) = api().await else {
        return;
    };
    let now = Utc::now();
    seed_raffle(&repo, &raffle("winter", -1, 100, now)).await;

    let mut body = order_body("sub@example.com", 10, "1985-05-05", "EH1 1YZ");
    body["subscribe"] = json!(true);
    let created = create_order(&api, "winter", &body).await;
    let customer_id = format!("cus_{}", created.entrant_id);

    let entrant = repo.get_entrant(&created.entrant_id).await.unwrap().unwrap();
    assert_eq!(entrant.stripe_customer_id.as_deref(), Some(customer_id.as_str()));
    assert!(repo.get_order(&created.order_id).await.unwrap().unwrap().subscribe);

    let sent = recorded(&intents);
    let [intent] = sent.as_slice() else {
        panic!("a subscribing checkout asks Stripe for one intent, got {sent:?}");
    };
    assert_eq!(
        (intent.customer_id.as_deref(), intent.save_payment_method),
        (Some(customer_id.as_str()), true),
        "the intent is charged to the new customer and keeps the card for the next raffle"
    );
}

#[tokio::test]
async fn rejected_requests_write_nothing() {
    let Some((api, repo, intents)) = api().await else {
        return;
    };
    let now = Utc::now();
    seed_raffle(&repo, &raffle("winter", -1, 100, now)).await;
    seed_raffle(&repo, &raffle("autumn", -100, -10, now)).await;
    let email = "reject@example.com";

    let refused_orders = [
        ("a donation beyond the cap", order_donating(email, 2_000_000), 400),
        ("a body that is not an order", json!({ "ticketQuantity": "five" }), 400),
        (
            "under eighteen, the one row that proves the licence guard runs",
            order_body(email, 5, CHILD, ENGLAND),
            403,
        ),
    ];

    for (label, body, expected_status) in refused_orders {
        assert_rejected(&api, "/raffles/winter/orders", &body, expected_status, label).await;
    }

    let refused_paths = [
        ("a raffle that does not exist", "/raffles/nope/orders", 404),
        ("a route that does not exist", "/somewhere/else", 404),
        ("a raffle that has closed", "/raffles/autumn/orders", 409),
    ];
    for (label, path, expected_status) in refused_paths {
        assert_rejected(&api, path, &eligible_order(email, 5), expected_status, label).await;
    }

    assert!(repo.find_entrant_by_email(email).await.unwrap().is_none(), "no entrant was written");
    assert!(recorded(&intents).is_empty(), "Stripe was never asked for an intent");
}

#[tokio::test]
async fn order_view_shows_the_ticket_range_once_the_webhook_has_allocated() {
    let Some((api, repo, _)) = api().await else {
        return;
    };
    let now = Utc::now();
    seed_raffle(&repo, &raffle("winter-2026", -1, 100, now)).await;
    let created = create_order(&api, "winter-2026", &eligible_order("ada@example.com", 15)).await;

    let pending = order_view(&api, &created.order_id).await;
    assert_eq!(pending.status, OrderStatus::Pending);
    assert_eq!(pending.tickets, None, "no ticket numbers before the charge settles");

    let webhook_payment = debit_payment(format!("pi_{}", created.order_id), Some("4242"));
    repo.allocate_entry(&created.order_id, &webhook_payment, now).await.unwrap();

    let paid = order_view(&api, &created.order_id).await;
    assert_eq!(paid.status, OrderStatus::Paid);
    assert_eq!(paid.tickets, Some(TicketRange { from: 1, to: 15 }));
    assert_eq!(paid.total_pence, created.total_pence, "the view reports what the checkout charged");

    let (status, body) = fetched(&api, "/orders/ord_missing").await;
    assert_eq!(status, 404, "an unknown order id: {body}");
}
