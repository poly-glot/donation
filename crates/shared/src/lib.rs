//! Shared crate for the charity raffle: the domain model and the single-table
//! persistence, organised by feature.
//!
//! [`table`] is the domain-free core — the `DynamoRepo` handle and the query and
//! write primitives every feature is written in terms of. Each other module is a
//! feature slice that owns its domain types, its rules, its slice of the table
//! layout, and the `DynamoRepo` methods that read and write it:
//!
//! - [`raffle`] — the raffle and its prize tiers; status derived from dates
//! - [`entrant`] — the supporter, marketing consent and Gift Aid; the PII partition
//! - [`order`] — orders, the ticket ledger, and the gapless allocation transaction
//! - [`subscription`] — the saved-card VIP subscription and its eligibility rules
//! - [`draw`] — the draw record and its winners
//! - [`stripe`] — the payment gateway trait and the Stripe client
//!
//! The Lambdas are thin handlers over these; the shared crate is where the
//! behaviour and its tests live.

pub mod error;
pub mod http;
pub mod random;
pub mod table;

pub mod auth;
pub mod draw;
pub mod entrant;
pub mod order;
pub mod raffle;
pub mod subscription;

pub mod stripe;
pub mod telemetry;

#[cfg(feature = "testing")]
pub mod testing;
