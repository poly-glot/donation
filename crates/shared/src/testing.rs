use aws_config::BehaviorVersion;
use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::client::Waiters;
use aws_sdk_dynamodb::types::{
    AttributeDefinition, BillingMode, GlobalSecondaryIndex, KeySchemaElement, KeyType, Projection, ProjectionType, ScalarAttributeType,
};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};

use crate::entrant::{Address, Entrant};
use crate::error::AppError;
use crate::order::PaidPayment;
use crate::raffle::{Prize, Raffle};
use crate::subscription::{Subscription, SubscriptionStatus};
use crate::table::{DynamoRepo, GSI1, GSI2};

pub async fn local_repo(prefix: &str) -> Option<DynamoRepo> {
    if std::env::var("AWS_ENDPOINT_URL_DYNAMODB").is_err() {
        eprintln!("skipping {prefix}: AWS_ENDPOINT_URL_DYNAMODB not set");
        return None;
    }
    let client = Client::new(&aws_config::load_defaults(BehaviorVersion::latest()).await);
    let table = format!("{prefix}-{}", crate::random::id("run"));
    create_table(&client, &table).await.expect("create table");
    client
        .wait_until_table_exists()
        .table_name(&table)
        .wait(std::time::Duration::from_secs(120))
        .await
        .expect("table active");

    Some(DynamoRepo::new(client, table))
}

pub async fn seed_raffle(repo: &DynamoRepo, raffle: &Raffle) {
    repo.create_counters(raffle).await.expect("create counters");
    if repo.create_raffle(raffle).await.expect("create raffle") {
        return;
    }
    let existing = repo.get_raffle(&raffle.raffle_id).await.expect("get raffle").expect("raffle exists");
    assert!(repo.replace_raffle(raffle, existing.tickets_sold).await.expect("replace raffle"));
}

pub fn at(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap()
}

pub fn winter() -> Raffle {
    Raffle {
        raffle_id: "winter-2026".into(),
        name: "Winter Poppy Raffle 2026".into(),
        ticket_price_pence: 100,
        max_tickets_per_order: 20,
        max_tickets: 5_000_000,
        opens_at: at(2026, 9, 30),
        closes_at: at(2027, 1, 8),
        draw_at: at(2027, 1, 22),
        results_at: at(2027, 2, 5),
        drawn_at: None,
        tickets_sold: 0,
        ticket_revenue_pence: 0,
        donation_pence: 0,
        subscriptions_charged_at: None,
        shards: None,
        created_at: at(2026, 9, 1),
    }
}

pub fn prize(raffle_id: &str, rank: u32, amount_pence: u64, quantity: u32) -> Prize {
    Prize {
        raffle_id: raffle_id.into(),
        rank,
        name: format!("prize {rank}"),
        amount_pence,
        quantity,
    }
}

pub fn debit_payment(payment_intent_id: impl Into<String>, card_last4: Option<&str>) -> PaidPayment {
    PaidPayment {
        payment_intent_id: payment_intent_id.into(),
        card_funding: Some("debit".into()),
        card_last4: card_last4.map(Into::into),
    }
}

pub fn scripted(values: Vec<u64>) -> impl FnMut() -> u64 {
    let mut values = values.into_iter();
    move || values.next().expect("script exhausted")
}

pub fn raffle(raffle_id: &str, opens_in_days: i64, closes_in_days: i64, now: DateTime<Utc>) -> Raffle {
    Raffle {
        raffle_id: raffle_id.into(),
        name: format!("{raffle_id} raffle"),
        ticket_price_pence: 100,
        max_tickets_per_order: 20,
        max_tickets: 5_000_000,
        opens_at: now + Duration::days(opens_in_days),
        closes_at: now + Duration::days(closes_in_days),
        draw_at: now + Duration::days(closes_in_days + 14),
        results_at: now + Duration::days(closes_in_days + 28),
        drawn_at: None,
        tickets_sold: 0,
        ticket_revenue_pence: 0,
        donation_pence: 0,
        subscriptions_charged_at: None,
        shards: None,
        created_at: now,
    }
}

pub fn entrant(entrant_id: &str, now: DateTime<Utc>) -> Entrant {
    Entrant {
        entrant_id: entrant_id.into(),
        title: "Mr".into(),
        first_name: "Alan".into(),
        last_name: "Turing".into(),
        email: format!("{entrant_id}@example.com"),
        telephone: None,
        date_of_birth: NaiveDate::from_ymd_opt(1980, 6, 23).expect("valid date"),
        address: Address {
            line1: "1 Bletchley Rd".into(),
            line2: None,
            town: "Milton Keynes".into(),
            postcode: "MK3 6EB".into(),
            country: "GB".into(),
        },
        stripe_customer_id: None,
        self_excluded_until: None,
        erased_at: None,
        created_at: now,
    }
}

pub fn subscription(subscription_id: &str, entrant_id: &str, now: DateTime<Utc>) -> Subscription {
    Subscription {
        subscription_id: subscription_id.into(),
        entrant_id: entrant_id.into(),
        stripe_customer_id: format!("cus_{subscription_id}"),
        stripe_payment_method_id: format!("pm_{subscription_id}"),
        tickets_per_raffle: 10,
        eligible_from: now - Duration::days(30),
        status: SubscriptionStatus::Active,
        created_at: now,
    }
}

async fn create_table(client: &Client, table_name: &str) -> Result<(), AppError> {
    let key = |name: &str, key_type: KeyType| KeySchemaElement::builder().attribute_name(name).key_type(key_type).build();
    let attribute = |name: &str| {
        AttributeDefinition::builder()
            .attribute_name(name)
            .attribute_type(ScalarAttributeType::S)
            .build()
    };
    let index = |name: &str, keys: Vec<KeySchemaElement>| -> Result<GlobalSecondaryIndex, AppError> {
        Ok(GlobalSecondaryIndex::builder()
            .index_name(name)
            .set_key_schema(Some(keys))
            .projection(Projection::builder().projection_type(ProjectionType::All).build())
            .build()?)
    };

    client
        .create_table()
        .table_name(table_name)
        .billing_mode(BillingMode::PayPerRequest)
        .key_schema(key("PK", KeyType::Hash)?)
        .key_schema(key("SK", KeyType::Range)?)
        .attribute_definitions(attribute("PK")?)
        .attribute_definitions(attribute("SK")?)
        .attribute_definitions(attribute("GSI1PK")?)
        .attribute_definitions(attribute("GSI1SK")?)
        .attribute_definitions(attribute("GSI2PK")?)
        .global_secondary_indexes(index(GSI1, vec![key("GSI1PK", KeyType::Hash)?, key("GSI1SK", KeyType::Range)?])?)
        .global_secondary_indexes(index(GSI2, vec![key("GSI2PK", KeyType::Hash)?])?)
        .send()
        .await?;

    Ok(())
}
