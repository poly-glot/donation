use std::future::Future;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

/// Every charge is in GBP; the raffle is a UK society lottery.
const CURRENCY: &str = "gbp";

const API_BASE: &str = "https://api.stripe.com/v1";
const CARD_ERROR: &str = "card_error";
const UNSUPPORTED: u16 = 0;
const CHARGE_PAGE_SIZE: u32 = 100;

pub(crate) type Form = Vec<(String, String)>;

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
pub struct OrderMetadata {
    #[serde(rename = "orderId")]
    pub order_id: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
pub struct Card {
    pub funding: Option<String>,
    pub last4: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
pub struct PaymentMethodDetails {
    #[serde(default)]
    pub card: Card,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
pub struct Charge {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub status: String,
    pub payment_intent: Option<String>,
    pub customer: Option<String>,
    pub payment_method: Option<String>,
    #[serde(default)]
    pub refunded: bool,
    #[serde(default)]
    pub metadata: OrderMetadata,
    #[serde(default)]
    pub payment_method_details: PaymentMethodDetails,
}

#[derive(Debug, Deserialize)]
struct ChargeList {
    data: Vec<Charge>,
    #[serde(default)]
    has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentIntentRequest {
    pub amount_pence: u64,
    pub customer_id: Option<String>,
    pub save_payment_method: bool,
    pub idempotency_key: String,
    pub description: String,
    pub metadata: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IntentCreated {
    pub id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffSessionCharge {
    pub amount_pence: u64,
    pub customer_id: String,
    pub payment_method_id: String,
    pub idempotency_key: String,
    pub description: String,
    pub metadata: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChargeOutcome {
    Succeeded { payment_intent_id: String },
    Pending { payment_intent_id: String },
    Declined { payment_intent_id: Option<String>, code: String },
}

#[derive(Debug, thiserror::Error)]
pub enum StripeError {
    #[error("stripe request failed: {0}")]
    Transport(#[from] reqwest::Error),

    #[error("stripe api error {status}: {message}")]
    Api { status: u16, message: String },
}

fn unsupported(operation: &str) -> StripeError {
    StripeError::Api {
        status: UNSUPPORTED,
        message: format!("{operation} not supported by this gateway"),
    }
}

pub trait PaymentGateway {
    fn create_customer(&self, email: &str, name: &str, entrant_id: &str) -> impl Future<Output = Result<String, StripeError>> {
        let _ = (email, name, entrant_id);
        async { Err(unsupported("create_customer")) }
    }

    fn create_payment_intent(&self, request: PaymentIntentRequest) -> impl Future<Output = Result<IntentCreated, StripeError>> {
        let _ = request;
        async { Err(unsupported("create_payment_intent")) }
    }

    fn charge_off_session(&self, charge: OffSessionCharge) -> impl Future<Output = Result<ChargeOutcome, StripeError>> {
        let _ = charge;
        async { Err(unsupported("charge_off_session")) }
    }

    fn refund(&self, payment_intent_id: &str, idempotency_key: &str) -> impl Future<Output = Result<String, StripeError>> {
        let _ = (payment_intent_id, idempotency_key);
        async { Err(unsupported("refund")) }
    }

    fn list_charges(&self, created_from: i64, created_to: i64) -> impl Future<Output = Result<Vec<Charge>, StripeError>> {
        let _ = (created_from, created_to);
        async { Err(unsupported("list_charges")) }
    }
}

#[derive(Debug, Deserialize)]
struct Id {
    id: String,
}

#[derive(Debug, Deserialize)]
struct IntentStatus {
    id: String,
    status: String,
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: ApiError,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(rename = "type")]
    kind: String,
    code: Option<String>,
    decline_code: Option<String>,
    message: Option<String>,
    payment_intent: Option<Id>,
}

pub(crate) fn parse_ok<T: DeserializeOwned>(status: u16, body: &str) -> Result<T, StripeError> {
    if !(200..300).contains(&status) {
        let message = serde_json::from_str::<ErrorResponse>(body)
            .ok()
            .and_then(|response| response.error.message)
            .unwrap_or_else(|| body.to_string());
        return Err(StripeError::Api { status, message });
    }
    serde_json::from_str(body).map_err(|err| StripeError::Api {
        status,
        message: format!("unreadable response: {err}"),
    })
}

pub(crate) fn classify_charge(status: u16, body: &str) -> Result<ChargeOutcome, StripeError> {
    let unreadable = |err: serde_json::Error| StripeError::Api {
        status,
        message: format!("unreadable response: {err}"),
    };

    if (200..300).contains(&status) {
        let intent: IntentStatus = serde_json::from_str(body).map_err(unreadable)?;
        return Ok(match intent.status.as_str() {
            "succeeded" => ChargeOutcome::Succeeded { payment_intent_id: intent.id },
            "processing" => ChargeOutcome::Pending { payment_intent_id: intent.id },
            other => ChargeOutcome::Declined {
                payment_intent_id: Some(intent.id),
                code: other.to_string(),
            },
        });
    }

    let response: ErrorResponse = serde_json::from_str(body).map_err(unreadable)?;
    let error = response.error;
    if error.kind != CARD_ERROR {
        return Err(StripeError::Api {
            status,
            message: error.message.unwrap_or_default(),
        });
    }

    Ok(ChargeOutcome::Declined {
        payment_intent_id: error.payment_intent.map(|intent| intent.id),
        code: error.decline_code.or(error.code).unwrap_or_else(|| CARD_ERROR.to_string()),
    })
}

fn with_metadata(mut form: Form, metadata: &[(String, String)]) -> Form {
    for (key, value) in metadata {
        form.push((format!("metadata[{key}]"), value.clone()));
    }
    form
}

pub(crate) fn payment_intent_form(request: &PaymentIntentRequest) -> Form {
    let mut form = vec![
        ("amount".to_string(), request.amount_pence.to_string()),
        ("currency".to_string(), CURRENCY.to_string()),
        ("payment_method_types[]".to_string(), "card".to_string()),
        ("description".to_string(), request.description.clone()),
    ];
    if let Some(customer_id) = &request.customer_id {
        form.push(("customer".to_string(), customer_id.clone()));
        if request.save_payment_method {
            form.push(("setup_future_usage".to_string(), "off_session".to_string()));
        }
    }
    with_metadata(form, &request.metadata)
}

pub(crate) fn off_session_form(charge: &OffSessionCharge) -> Form {
    let form = vec![
        ("amount".to_string(), charge.amount_pence.to_string()),
        ("currency".to_string(), CURRENCY.to_string()),
        ("customer".to_string(), charge.customer_id.clone()),
        ("payment_method".to_string(), charge.payment_method_id.clone()),
        ("payment_method_types[]".to_string(), "card".to_string()),
        ("confirm".to_string(), "true".to_string()),
        ("off_session".to_string(), "true".to_string()),
        ("description".to_string(), charge.description.clone()),
    ];
    with_metadata(form, &charge.metadata)
}

pub struct StripeClient {
    http: reqwest::Client,
    secret_key: String,
    base_url: String,
}

impl StripeClient {
    pub fn new(secret_key: impl Into<String>) -> Result<Self, reqwest::Error> {
        Self::with_base_url(secret_key, API_BASE)
    }

    /// Build a client pointed at a specific API base. Production uses [`API_BASE`]
    /// via [`StripeClient::new`]; tests point it at a mock server so the request
    /// building and response parsing are exercised without touching Stripe.
    pub fn with_base_url(secret_key: impl Into<String>, base_url: impl Into<String>) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            secret_key: secret_key.into(),
            base_url: base_url.into(),
        })
    }

    async fn post(&self, path: &str, idempotency_key: Option<&str>, form: &Form) -> Result<(u16, String), StripeError> {
        let mut request = self.http.post(format!("{}/{path}", self.base_url)).bearer_auth(&self.secret_key).form(form);
        if let Some(key) = idempotency_key {
            request = request.header("Idempotency-Key", key);
        }
        send(request).await
    }

    async fn get(&self, path: &str, query: &Form) -> Result<(u16, String), StripeError> {
        send(self.http.get(format!("{}/{path}", self.base_url)).bearer_auth(&self.secret_key).query(query)).await
    }
}

async fn send(request: reqwest::RequestBuilder) -> Result<(u16, String), StripeError> {
    let response = request.send().await?;
    let status = response.status().as_u16();
    Ok((status, response.text().await?))
}

impl PaymentGateway for StripeClient {
    async fn create_customer(&self, email: &str, name: &str, entrant_id: &str) -> Result<String, StripeError> {
        let form = vec![
            ("email".to_string(), email.to_string()),
            ("name".to_string(), name.to_string()),
            ("metadata[entrantId]".to_string(), entrant_id.to_string()),
        ];
        let (status, body) = self.post("customers", Some(&format!("customer_{entrant_id}")), &form).await?;
        parse_ok::<Id>(status, &body).map(|customer| customer.id)
    }

    async fn create_payment_intent(&self, request: PaymentIntentRequest) -> Result<IntentCreated, StripeError> {
        let (status, body) = self
            .post("payment_intents", Some(&request.idempotency_key), &payment_intent_form(&request))
            .await?;
        parse_ok(status, &body)
    }

    async fn charge_off_session(&self, charge: OffSessionCharge) -> Result<ChargeOutcome, StripeError> {
        let (status, body) = self.post("payment_intents", Some(&charge.idempotency_key), &off_session_form(&charge)).await?;
        classify_charge(status, &body)
    }

    async fn refund(&self, payment_intent_id: &str, idempotency_key: &str) -> Result<String, StripeError> {
        let form = vec![("payment_intent".to_string(), payment_intent_id.to_string())];
        let (status, body) = self.post("refunds", Some(idempotency_key), &form).await?;
        parse_ok::<Id>(status, &body).map(|refund| refund.id)
    }

    async fn list_charges(&self, created_from: i64, created_to: i64) -> Result<Vec<Charge>, StripeError> {
        let mut charges = Vec::new();
        let mut starting_after: Option<String> = None;

        loop {
            let mut query = vec![
                ("created[gte]".to_string(), created_from.to_string()),
                ("created[lte]".to_string(), created_to.to_string()),
                ("limit".to_string(), CHARGE_PAGE_SIZE.to_string()),
            ];
            if let Some(cursor) = &starting_after {
                query.push(("starting_after".to_string(), cursor.clone()));
            }

            let (status, body) = self.get("charges", &query).await?;
            let ChargeList { data, has_more } = parse_ok(status, &body)?;
            starting_after = data.last().map(|charge| charge.id.clone());
            charges.extend(data);

            if !has_more || starting_after.is_none() {
                return Ok(charges);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field<'a>(form: &'a Form, name: &str) -> Option<&'a str> {
        form.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    fn declined(pi: Option<&str>) -> ChargeOutcome {
        declined_with(pi, "insufficient_funds")
    }

    fn declined_with(pi: Option<&str>, code: &str) -> ChargeOutcome {
        ChargeOutcome::Declined {
            payment_intent_id: pi.map(str::to_string),
            code: code.into(),
        }
    }

    fn succeeded(pi: &str) -> ChargeOutcome {
        ChargeOutcome::Succeeded { payment_intent_id: pi.into() }
    }

    fn pending(pi: &str) -> ChargeOutcome {
        ChargeOutcome::Pending { payment_intent_id: pi.into() }
    }

    #[test]
    fn a_charge_status_decides_the_outcome_the_charge_run_books() {
        let cases = [
            (
                "settled, so the webhook will allocate",
                200,
                r#"{"id":"pi_1","status":"succeeded"}"#,
                succeeded("pi_1"),
            ),
            (
                "still settling, booked as charged rather than failed",
                200,
                r#"{"id":"pi_2","status":"processing"}"#,
                pending("pi_2"),
            ),
            (
                "needs the cardholder, which an off-session charge cannot get",
                200,
                r#"{"id":"pi_3","status":"requires_action"}"#,
                declined_with(Some("pi_3"), "requires_action"),
            ),
        ];
        for (label, status, body, expected) in cases {
            assert_eq!(classify_charge(status, body).unwrap(), expected, "{label}");
        }
    }

    #[test]
    fn card_errors_decline_with_the_most_specific_code() {
        let body = r#"{"error":{"type":"card_error","code":"card_declined","decline_code":"insufficient_funds","message":"Your card has insufficient funds.","payment_intent":{"id":"pi_4"}}}"#;
        assert_eq!(classify_charge(402, body).unwrap(), declined(Some("pi_4")));
        assert_eq!(
            classify_charge(402, r#"{"error":{"type":"card_error","code":"insufficient_funds"}}"#).unwrap(),
            declined(None)
        );
    }

    #[test]
    fn other_errors_and_garbage_are_retryable_failures() {
        let api = classify_charge(500, r#"{"error":{"type":"api_error","message":"boom"}}"#).unwrap_err();
        let StripeError::Api { status, message } = api else {
            panic!("a 5xx is an error to retry, never a decline, got {api:?}");
        };
        assert_eq!((status, message.as_str()), (500, "boom"), "Stripe's own status and message reach the caller");

        let cases = [("a 200 that is not json", 200, "not json"), ("an unauthorised key", 401, "{}")];
        for (label, status, body) in cases {
            let err = classify_charge(status, body).unwrap_err();
            let StripeError::Api { status: reported, .. } = err else {
                panic!("{label}: expected an api error, got {err:?}");
            };
            assert_eq!(reported, status, "{label}");
        }
    }

    #[test]
    fn parse_ok_reads_objects_and_surfaces_api_messages() {
        let intent: IntentCreated = parse_ok(200, r#"{"id":"pi_1","client_secret":"pi_1_secret","status":"requires_payment_method"}"#).unwrap();
        assert_eq!(
            intent,
            IntentCreated {
                id: "pi_1".into(),
                client_secret: "pi_1_secret".into()
            }
        );
        let customer: Id = parse_ok(200, r#"{"id":"cus_1","object":"customer"}"#).unwrap();
        assert_eq!(customer.id, "cus_1");

        let err = parse_ok::<Id>(400, r#"{"error":{"type":"invalid_request_error","message":"No such customer"}}"#).unwrap_err();
        let StripeError::Api { status, message } = err else {
            panic!("a 4xx keeps its own status and message, got {err:?}");
        };
        assert_eq!((status, message.as_str()), (400, "No such customer"));

        let html = parse_ok::<Id>(502, "<html>").unwrap_err();
        let StripeError::Api { status, .. } = html else {
            panic!("a gateway page is still an api error, got {html:?}");
        };
        assert_eq!(status, 502, "a body that is not json keeps the status it arrived with");
    }

    #[test]
    fn payment_intent_form_saves_the_card_only_with_a_customer() {
        let mut request = PaymentIntentRequest {
            amount_pence: 2_500,
            customer_id: None,
            save_payment_method: true,
            idempotency_key: "ord_1".into(),
            description: "Winter Poppy Raffle 2026".into(),
            metadata: vec![("orderId".into(), "ord_1".into())],
        };
        let form = payment_intent_form(&request);
        assert_eq!(field(&form, "amount"), Some("2500"));
        assert_eq!(field(&form, "currency"), Some("gbp"));
        assert_eq!(field(&form, "metadata[orderId]"), Some("ord_1"));
        assert_eq!(field(&form, "customer"), None);
        assert_eq!(field(&form, "setup_future_usage"), None);

        request.customer_id = Some("cus_1".into());
        let form = payment_intent_form(&request);
        assert_eq!(field(&form, "customer"), Some("cus_1"));
        assert_eq!(field(&form, "setup_future_usage"), Some("off_session"));
    }

    #[test]
    fn off_session_form_confirms_a_saved_card_charge() {
        let charge = OffSessionCharge {
            amount_pence: 1_000,
            customer_id: "cus_1".into(),
            payment_method_id: "pm_1".into(),
            idempotency_key: "sub_sub-1_winter-2026".into(),
            description: "Winter Poppy Raffle 2026: 10 subscription tickets".into(),
            metadata: vec![("orderId".into(), "sub_sub-1_winter-2026".into())],
        };
        let form = off_session_form(&charge);
        assert_eq!(field(&form, "customer"), Some("cus_1"));
        assert_eq!(field(&form, "payment_method"), Some("pm_1"));
        assert_eq!(field(&form, "confirm"), Some("true"));
        assert_eq!(field(&form, "off_session"), Some("true"));
        assert_eq!(field(&form, "payment_method_types[]"), Some("card"));
        assert_eq!(field(&form, "metadata[orderId]"), Some("sub_sub-1_winter-2026"));
    }
}
