//! The supporter, and the two append-only histories a regulator can ask about.
//!
//! One partition — `ENTRANT#{id}` — holds all of a person's personal data: their
//! profile, their marketing-consent history and their Gift Aid declarations. The
//! gambling ledger ([`crate::order`], [`crate::draw`]) keeps only the `entrant_id`,
//! so erasure rewrites this partition and leaves the financial record intact.
//!
//! The profile row also anchors the two overloaded GSIs. Its `GSI1PK` is the
//! email index that finds a returning supporter; every other row a person owns
//! (orders, entries, subscriptions, wins) sets `GSI1PK = ENTRANT#{id}` so that
//! "everything for this person" — a subject access request — is a single query.
//! That shared entry-point is why [`entrant_pk`] and [`DynamoRepo::query_entrant_index`]
//! live here and are reused by the other feature modules.
//!
//! Consent and Gift Aid are never overwritten booleans: each is a timestamped
//! row, so "what did they agree to, and when" is always answerable and a later
//! `false` row is the withdrawal.

use chrono::{DateTime, Datelike, Months, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::table::{DynamoRepo, GSI1, Page, partition, s, sort_ts};

/// The text written over redacted fields on erasure.
pub const ERASED: &str = "[erased]";

const ADULT_AGE_MONTHS: u32 = 18 * 12;

/// Postcode areas that are outside Great Britain for lottery purposes: Northern
/// Ireland (BT) and the Crown Dependencies (Guernsey, Isle of Man, Jersey).
const NON_GB_POSTCODE_AREAS: [&str; 4] = ["BT", "GY", "IM", "JE"];

/// The sort key of the profile row, and its own `GSI1SK` on the email index.
pub(crate) const PROFILE_SK: &str = "#PROFILE";

pub(crate) fn entrant_pk(entrant_id: &str) -> String {
    partition("ENTRANT", entrant_id)
}

fn email_gsi1pk(email: &str) -> String {
    partition("EMAIL", email.trim().to_lowercase())
}

fn consent_sk(recorded_at: DateTime<Utc>) -> String {
    format!("CONSENT#{}", sort_ts(recorded_at))
}

fn gift_aid_sk(declared_at: DateTime<Utc>) -> String {
    format!("GIFTAID#{}", sort_ts(declared_at))
}

/// The email index is dropped on erasure — a redacted profile must not be
/// findable by the address it no longer holds — so the GSI1 keys are present
/// only while the entrant is live.
fn entrant_keys(entrant: &Entrant) -> Vec<(&'static str, String)> {
    let mut keys = vec![("PK", entrant_pk(&entrant.entrant_id)), ("SK", PROFILE_SK.into())];
    if entrant.erased_at.is_none() {
        keys.push(("GSI1PK", email_gsi1pk(&entrant.email)));
        keys.push(("GSI1SK", PROFILE_SK.into()));
    }
    keys
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Address {
    pub line1: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line2: Option<String>,
    pub town: String,
    pub postcode: String,
    pub country: String,
}

impl Address {
    pub fn is_great_britain(&self) -> bool {
        self.country == "GB" && is_great_britain_postcode(&self.postcode)
    }

    fn erased(&self) -> Self {
        Self {
            line1: ERASED.into(),
            line2: None,
            town: ERASED.into(),
            postcode: ERASED.into(),
            country: self.country.clone(),
        }
    }
}

/// A server-side sanity check, not a full validator: it accepts a plausible UK
/// postcode and rejects the non-GB areas outright, so a bypassed client can never
/// slip through even though the checkout's address lookup does the real work.
pub(crate) fn is_great_britain_postcode(postcode: &str) -> bool {
    let normalized: String = postcode.chars().filter(|c| !c.is_whitespace()).map(|c| c.to_ascii_uppercase()).collect();

    let area: String = normalized.chars().take_while(char::is_ascii_alphabetic).collect();

    let plausible = (1..=2).contains(&area.len()) && (5..=7).contains(&normalized.len());
    plausible && !NON_GB_POSTCODE_AREAS.contains(&area.as_str())
}

/// True once the 18th birthday is on or before today — the exact-birthday rule
/// the Gambling Commission expects, computed by adding 18 years rather than
/// dividing days so leap years never round the wrong way.
pub(crate) fn is_adult(date_of_birth: NaiveDate, today: NaiveDate) -> bool {
    date_of_birth
        .checked_add_months(Months::new(ADULT_AGE_MONTHS))
        .is_some_and(|eighteenth_birthday| eighteenth_birthday <= today)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entrant {
    pub entrant_id: String,
    pub title: String,
    pub first_name: String,
    pub last_name: String,
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telephone: Option<String>,
    pub date_of_birth: NaiveDate,
    pub address: Address,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stripe_customer_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_excluded_until: Option<NaiveDate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub erased_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl Entrant {
    pub fn is_adult_on(&self, today: NaiveDate) -> bool {
        is_adult(self.date_of_birth, today)
    }

    pub fn is_self_excluded_on(&self, today: NaiveDate) -> bool {
        self.self_excluded_until.is_some_and(|until| today < until)
    }

    /// Redact the profile for a right-to-erasure request, keeping only the birth
    /// year (so the gambling ledger stays defensibly age-verified) and the id that
    /// links to the retained financial records.
    pub fn erased(&self, now: DateTime<Utc>) -> Self {
        let birth_year_only = NaiveDate::from_ymd_opt(self.date_of_birth.year(), 1, 1).unwrap_or(self.date_of_birth);

        Self {
            title: ERASED.into(),
            first_name: ERASED.into(),
            last_name: ERASED.into(),
            email: ERASED.into(),
            telephone: None,
            date_of_birth: birth_year_only,
            address: self.address.erased(),
            stripe_customer_id: None,
            erased_at: Some(now),
            ..self.clone()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketingConsent {
    pub entrant_id: String,
    pub recorded_at: DateTime<Utc>,
    pub email: bool,
    pub post: bool,
    pub wording_version: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GiftAidDonor {
    pub title: String,
    pub first_name: String,
    pub last_name: String,
    pub address: Address,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GiftAidDeclaration {
    pub entrant_id: String,
    pub declared_at: DateTime<Utc>,
    pub is_uk_taxpayer: bool,
    pub wording_version: String,
    pub donor: GiftAidDonor,
}

impl GiftAidDeclaration {
    /// HMRC needs the donor's name and home address *as given at the time*, so we
    /// snapshot them onto the declaration; the snapshot survives later address
    /// changes and even erasure.
    pub fn from_entrant(entrant: &Entrant, is_uk_taxpayer: bool, wording_version: impl Into<String>, now: DateTime<Utc>) -> Self {
        Self {
            entrant_id: entrant.entrant_id.clone(),
            declared_at: now,
            is_uk_taxpayer,
            wording_version: wording_version.into(),
            donor: GiftAidDonor {
                title: entrant.title.clone(),
                first_name: entrant.first_name.clone(),
                last_name: entrant.last_name.clone(),
                address: entrant.address.clone(),
            },
        }
    }
}

impl DynamoRepo {
    pub async fn put_entrant(&self, entrant: &Entrant) -> Result<(), AppError> {
        self.put(entrant, &entrant_keys(entrant), None).await
    }

    pub async fn get_entrant(&self, entrant_id: &str) -> Result<Option<Entrant>, AppError> {
        self.get(entrant_pk(entrant_id), PROFILE_SK, false).await
    }

    pub async fn find_entrant_by_email(&self, email: &str) -> Result<Option<Entrant>, AppError> {
        self.query(Some(GSI1), "GSI1PK = :pk", vec![(":pk", s(email_gsi1pk(email)))], false, Some(1), None)
            .await?
            .first()
    }

    pub async fn erase_entrant(&self, entrant_id: &str, now: DateTime<Utc>) -> Result<bool, AppError> {
        let Some(entrant) = self.get_entrant(entrant_id).await? else {
            return Ok(false);
        };
        self.put_entrant(&entrant.erased(now)).await?;
        Ok(true)
    }

    pub async fn put_marketing_consent(&self, consent: &MarketingConsent) -> Result<(), AppError> {
        let keys = [("PK", entrant_pk(&consent.entrant_id)), ("SK", consent_sk(consent.recorded_at))];
        self.put(consent, &keys, None).await
    }

    pub async fn latest_marketing_consent(&self, entrant_id: &str) -> Result<Option<MarketingConsent>, AppError> {
        self.query_prefix(entrant_pk(entrant_id), "CONSENT#", true, Some(1), None).await?.first()
    }

    pub async fn put_gift_aid_declaration(&self, declaration: &GiftAidDeclaration) -> Result<(), AppError> {
        let keys = [("PK", entrant_pk(&declaration.entrant_id)), ("SK", gift_aid_sk(declaration.declared_at))];
        self.put(declaration, &keys, None).await
    }

    pub async fn latest_gift_aid_declaration(&self, entrant_id: &str) -> Result<Option<GiftAidDeclaration>, AppError> {
        self.query_prefix(entrant_pk(entrant_id), "GIFTAID#", true, Some(1), None).await?.first()
    }

    /// Query one kind of row that a supporter owns, via the entrant partition on
    /// GSI1. The order, subscription and draw modules use this to answer "my
    /// orders", "my subscriptions", "my wins" without duplicating the index shape.
    pub(crate) async fn query_entrant_index(&self, entrant_id: &str, sk_prefix: &str) -> Result<Page, AppError> {
        self.query(
            Some(GSI1),
            "GSI1PK = :pk AND begins_with(GSI1SK, :sk)",
            vec![(":pk", s(entrant_pk(entrant_id))), (":sk", s(sk_prefix))],
            true,
            None,
            None,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn adulthood_turns_over_exactly_on_the_eighteenth_birthday() {
        let cases = [
            ("day before turning 18", date(2008, 9, 10), date(2026, 9, 9), false),
            ("on the 18th birthday", date(2008, 9, 9), date(2026, 9, 9), true),
            ("leap-day birth, 18 years on", date(2008, 2, 29), date(2026, 2, 28), true),
        ];
        for (label, born, today, expected) in cases {
            assert_eq!(is_adult(born, today), expected, "{label}");
        }
    }

    #[test]
    fn great_britain_postcodes_accept_gb_and_reject_ni_and_crown_dependencies() {
        let cases = [
            ("SW1A 1AA", true),
            (" se1 1aa ", true), // spacing and case are normalised
            ("EH1 1YZ", true),   // Scotland
            ("CF10 1EP", true),  // Wales
            ("BT1 5GS", false),  // Northern Ireland
            ("JE2 3NN", false),  // Jersey
            ("GY1 1AA", false),  // Guernsey
            ("IM1 1AA", false),  // Isle of Man
            ("", false),
            ("12345", false), // not a UK shape at all
        ];
        for (postcode, expected) in cases {
            assert_eq!(is_great_britain_postcode(postcode), expected, "{postcode:?}");
        }
    }

    #[test]
    fn erasure_redacts_pii_but_keeps_the_id_and_birth_year() {
        let source = Entrant {
            entrant_id: "ent-1".into(),
            title: "Ms".into(),
            first_name: "Ada".into(),
            last_name: "Lovelace".into(),
            email: "ada@example.com".into(),
            telephone: Some("0345".into()),
            date_of_birth: date(1990, 6, 15),
            address: Address {
                line1: "199 Borough High St".into(),
                line2: None,
                town: "London".into(),
                postcode: "SW1A 1AA".into(),
                country: "GB".into(),
            },
            stripe_customer_id: Some("cus_1".into()),
            self_excluded_until: None,
            erased_at: None,
            created_at: "2026-09-01T00:00:00Z".parse().unwrap(),
        };

        let erased = source.erased("2027-03-01T00:00:00Z".parse().unwrap());
        assert_eq!(erased.entrant_id, "ent-1"); // link to the retained ledger survives
        assert_eq!(erased.email, ERASED);
        assert_eq!(erased.address.postcode, ERASED);
        assert_eq!(erased.date_of_birth, date(1990, 1, 1)); // birth year only
        assert!(erased.stripe_customer_id.is_none());
        assert!(erased.erased_at.is_some());
    }

    #[test]
    fn gift_aid_declaration_snapshots_the_donor_at_the_time() {
        let source = Entrant {
            entrant_id: "ent-1".into(),
            title: "Ms".into(),
            first_name: "Ada".into(),
            last_name: "Lovelace".into(),
            email: "ada@example.com".into(),
            telephone: None,
            date_of_birth: date(1990, 6, 15),
            address: Address {
                line1: "199 Borough High St".into(),
                line2: None,
                town: "London".into(),
                postcode: "SW1A 1AA".into(),
                country: "GB".into(),
            },
            stripe_customer_id: None,
            self_excluded_until: None,
            erased_at: None,
            created_at: "2026-09-01T00:00:00Z".parse().unwrap(),
        };

        let declaration = GiftAidDeclaration::from_entrant(&source, true, "ga-2026-v1", "2026-10-01T00:00:00Z".parse().unwrap());
        assert_eq!(declaration.donor.last_name, "Lovelace");
        assert_eq!(declaration.donor.address.postcode, "SW1A 1AA");
        assert!(declaration.is_uk_taxpayer);
    }
}
