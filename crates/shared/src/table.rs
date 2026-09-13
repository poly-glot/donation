//! The single-table core: everything about *how* we talk to DynamoDB, and
//! nothing about *what* the rows mean.
//!
//! The raffle lives in one DynamoDB table with two overloaded global secondary
//! indexes. That shape is deliberate — an atomic ticket allocation has to touch
//! the raffle, the ledger and the order in one transaction, and "everything for
//! one supporter" has to be a single query — so the mechanics of keys, pages
//! and conditional writes are shared by every feature. This module owns those
//! mechanics: the [`DynamoRepo`] handle and a small set of crate-internal
//! primitives (`get`, `put`, `query`) that the feature modules build their
//! domain methods on top of. The feature modules (`raffle`, `entrant`, `order`,
//! `subscription`, `draw`) each own their own keys and access patterns and hang
//! their repo methods off `DynamoRepo` from their own files, so the table layout
//! stays in one place while the meaning stays with each feature.

use std::collections::{BTreeMap, HashMap};

use aws_config::BehaviorVersion;
use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::AttributeValue;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_dynamo::aws_sdk_dynamodb_1::{from_item, from_items, to_item};

use crate::error::AppError;

pub(crate) type Item = HashMap<String, AttributeValue>;
pub type PageKey = HashMap<String, AttributeValue>;

pub const GSI1: &str = "GSI1";
pub const GSI2: &str = "GSI2";

/// The sort key shared by every "the entity itself" row: the raffle, an order,
/// a subscription. Child and history rows use their own prefixed sort keys.
pub(crate) const METADATA_SK: &str = "#METADATA";

pub(crate) const APP: &str = "donation";

pub(crate) fn partition(kind: &str, id: impl std::fmt::Display) -> String {
    format!("{APP}#{kind}#{id}")
}

/// Timestamps in sort keys are RFC 3339 with millisecond precision and a `Z`
/// suffix, chosen so that lexicographic string order matches chronological order.
pub(crate) fn sort_ts(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub(crate) fn s(value: impl Into<String>) -> AttributeValue {
    AttributeValue::S(value.into())
}

pub(crate) fn n(value: u64) -> AttributeValue {
    AttributeValue::N(value.to_string())
}

/// Serialise a domain entity to an item and stamp the key attributes onto it.
/// Every feature builds its rows this way: the struct is the payload, the key
/// list is the table layout the feature owns.
pub(crate) fn item<T: Serialize>(entity: &T, keys: &[(&str, String)]) -> Result<Item, AppError> {
    let mut item = to_item(entity)?;
    for (name, value) in keys {
        item.insert((*name).to_string(), s(value.clone()));
    }
    Ok(item)
}

/// A conditional write either succeeded or lost its condition; any other error
/// is a real failure. Callers that race for a row (create-if-absent, compare-and-set
/// state transitions) read the `bool` as "did I win?".
pub(crate) fn condition_failed_as_false<T, E: Into<AppError>>(result: Result<T, E>) -> Result<bool, AppError> {
    match result.map_err(Into::into) {
        Ok(_) => Ok(true),
        Err(err) if err.is_condition_failed() => Ok(false),
        Err(err) => Err(err),
    }
}

pub fn page_cursor(key: &PageKey) -> Result<String, AppError> {
    let mut attributes: BTreeMap<&str, &str> = BTreeMap::new();

    for (name, value) in key {
        let AttributeValue::S(text) = value else {
            return Err(AppError::Internal(format!("page key {name} is not a string")));
        };
        attributes.insert(name.as_str(), text.as_str());
    }

    serde_json::to_string(&attributes).map_err(|err| AppError::Internal(err.to_string()))
}

pub fn page_key(cursor: &str) -> Result<PageKey, AppError> {
    let attributes: HashMap<String, String> = serde_json::from_str(cursor).map_err(|err| AppError::BadRequest(format!("invalid cursor: {err}")))?;

    Ok(attributes.into_iter().map(|(name, value)| (name, s(value))).collect())
}

/// One page of a query, still in raw item form so the caller decides whether it
/// wants all of it, just the first row, or a page plus a cursor to resume from.
pub(crate) struct Page {
    items: Vec<Item>,
    last_key: Option<PageKey>,
}

impl Page {
    pub(crate) fn all<T: DeserializeOwned>(self) -> Result<Vec<T>, AppError> {
        Ok(from_items(self.items)?)
    }

    pub(crate) fn first<T: DeserializeOwned>(self) -> Result<Option<T>, AppError> {
        Ok(self.items.into_iter().next().map(from_item).transpose()?)
    }

    pub(crate) fn paged<T: DeserializeOwned>(self) -> Result<(Vec<T>, Option<PageKey>), AppError> {
        let Page { items, last_key } = self;
        Ok((from_items(items)?, last_key))
    }
}

/// A handle to the raffle table. Cheap to clone (it wraps an `Arc` client), so
/// each Lambda builds one and shares it. The domain methods live in the feature
/// modules; this file provides only the primitives they are written in terms of.
#[derive(Clone)]
pub struct DynamoRepo {
    client: Client,
    table: String,
}

impl DynamoRepo {
    pub fn new(client: Client, table: impl Into<String>) -> Self {
        Self { client, table: table.into() }
    }

    pub async fn from_env() -> Result<Self, std::env::VarError> {
        let client = Client::new(&aws_config::load_defaults(BehaviorVersion::latest()).await);
        Ok(Self::new(client, std::env::var("TABLE_NAME")?))
    }

    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    pub(crate) fn table(&self) -> &str {
        &self.table
    }

    pub(crate) async fn get<T: DeserializeOwned>(&self, pk: String, sk: &str, consistent: bool) -> Result<Option<T>, AppError> {
        let out = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("PK", s(pk))
            .key("SK", s(sk))
            .consistent_read(consistent)
            .send()
            .await?;

        Ok(out.item.map(from_item).transpose()?)
    }

    pub(crate) async fn put<T: Serialize>(&self, entity: &T, keys: &[(&str, String)], condition: Option<&str>) -> Result<(), AppError> {
        self.client
            .put_item()
            .table_name(&self.table)
            .set_item(Some(item(entity, keys)?))
            .set_condition_expression(condition.map(String::from))
            .send()
            .await?;
        Ok(())
    }

    pub(crate) async fn query(
        &self,
        index: Option<&str>,
        key_condition: &str,
        values: Vec<(&str, AttributeValue)>,
        newest_first: bool,
        limit: Option<i32>,
        start: Option<PageKey>,
    ) -> Result<Page, AppError> {
        let mut request = self
            .client
            .query()
            .table_name(&self.table)
            .set_index_name(index.map(String::from))
            .key_condition_expression(key_condition)
            .scan_index_forward(!newest_first)
            .set_limit(limit)
            .set_exclusive_start_key(start);

        for (name, value) in values {
            request = request.expression_attribute_values(name, value);
        }

        let out = request.send().await?;
        Ok(Page {
            items: out.items.unwrap_or_default(),
            last_key: out.last_evaluated_key,
        })
    }

    /// The commonest query: rows under one partition whose sort key starts with a
    /// prefix — a raffle's prizes, a supporter's consent history, a raffle's winners.
    pub(crate) async fn query_prefix(
        &self,
        pk: String,
        sk_prefix: &str,
        newest_first: bool,
        limit: Option<i32>,
        start: Option<PageKey>,
    ) -> Result<Page, AppError> {
        self.query(
            None,
            "PK = :pk AND begins_with(SK, :sk)",
            vec![(":pk", s(pk)), (":sk", s(sk_prefix))],
            newest_first,
            limit,
            start,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_key_survives_the_round_trip_through_a_cursor() {
        let key = PageKey::from([("PK".to_string(), s("donation#RAFFLE#winter-2026")), ("SK".to_string(), s("ENTRY#00000015"))]);

        let cursor = page_cursor(&key).expect("a key of strings encodes");
        assert_eq!(page_key(&cursor).expect("the cursor decodes"), key);
    }

    #[test]
    fn a_cursor_that_is_not_a_page_key_is_a_bad_request() {
        let cases = [
            ("text that was never a cursor", "resume-here"),
            ("a JSON array instead of a map", "[\"PK\"]"),
            ("a map whose values are not strings", "{\"PK\":7}"),
        ];
        for (label, cursor) in cases {
            let refused = page_key(cursor);
            assert!(matches!(&refused, Err(AppError::BadRequest(_))), "{label}: got {refused:?}");
        }
    }

    #[test]
    fn a_page_key_that_is_not_all_strings_is_refused_rather_than_silently_dropped() {
        let key = PageKey::from([("count".to_string(), n(7))]);

        let refused = page_cursor(&key);
        assert!(matches!(&refused, Err(AppError::Internal(_))), "got {refused:?}");
    }
}
