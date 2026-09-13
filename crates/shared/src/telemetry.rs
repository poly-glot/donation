use std::sync::LazyLock;

use chrono::Utc;
use serde_json::{Map, Value, json};
use tracing_subscriber::filter::LevelFilter;

const DEFAULT_NAMESPACE: &str = "Raffle";

static NAMESPACE: LazyLock<String> = LazyLock::new(|| std::env::var("METRIC_NAMESPACE").unwrap_or_else(|_| DEFAULT_NAMESPACE.into()));

pub fn init_logging() {
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_target(false)
        .without_time()
        .with_max_level(LevelFilter::INFO)
        .init();
}

fn emf_line(namespace: &str, timestamp_millis: i64, values: &[(&str, f64)], dimensions: &[(&str, &str)], properties: &[(&str, &str)]) -> Value {
    let mut line = Map::new();
    line.insert(
        "_aws".into(),
        json!({
            "Timestamp": timestamp_millis,
            "CloudWatchMetrics": [{
                "Namespace": namespace,
                "Dimensions": [dimensions.iter().map(|(name, _)| *name).collect::<Vec<_>>()],
                "Metrics": values.iter().map(|(name, _)| json!({ "Name": name, "Unit": "None" })).collect::<Vec<_>>(),
            }],
        }),
    );

    for (name, value) in dimensions.iter().chain(properties) {
        line.insert((*name).into(), json!(value));
    }
    for (name, value) in values {
        line.insert((*name).into(), json!(value));
    }
    Value::Object(line)
}

pub fn emit(values: &[(&str, f64)], dimensions: &[(&str, &str)], properties: &[(&str, &str)]) {
    println!("{}", emf_line(&NAMESPACE, Utc::now().timestamp_millis(), values, dimensions, properties));
}

pub fn count(name: &str, dimensions: &[(&str, &str)], properties: &[(&str, &str)]) {
    emit(&[(name, 1.0)], dimensions, properties);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emf_line_declares_metrics_and_dimensions_and_carries_their_values() {
        let line = emf_line(
            "raffle-test",
            1_700_000_000_000,
            &[("WebhookOutcome", 1.0)],
            &[("Outcome", "Allocated")],
            &[("eventId", "evt_1")],
        );

        assert_eq!(line["_aws"]["Timestamp"], 1_700_000_000_000_i64);
        assert_eq!(line["_aws"]["CloudWatchMetrics"][0]["Namespace"], "raffle-test");
        assert_eq!(line["_aws"]["CloudWatchMetrics"][0]["Dimensions"], json!([["Outcome"]]));
        assert_eq!(line["_aws"]["CloudWatchMetrics"][0]["Metrics"][0]["Name"], "WebhookOutcome");
        assert_eq!(line["Outcome"], "Allocated");
        assert_eq!(line["eventId"], "evt_1");
        assert_eq!(line["WebhookOutcome"], 1.0);
    }

    #[test]
    fn emf_line_without_dimensions_still_lists_an_empty_dimension_set() {
        let line = emf_line(
            "raffle-test",
            0,
            &[("SubscriptionsCharged", 3.0), ("SubscriptionsErrored", 0.0)],
            &[],
            &[("raffleId", "winter")],
        );

        assert_eq!(line["_aws"]["CloudWatchMetrics"][0]["Dimensions"], json!([[]]));
        assert_eq!(line["_aws"]["CloudWatchMetrics"][0]["Metrics"].as_array().map(Vec::len), Some(2));
        assert_eq!(line["SubscriptionsCharged"], 3.0);
        assert_eq!(line["raffleId"], "winter");
    }
}
