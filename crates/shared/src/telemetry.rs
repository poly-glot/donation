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

fn emf_line(namespace: &str, timestamp_millis: i64, values: &[(&str, f64)], properties: &[(&str, &str)]) -> Value {
    let mut line = Map::new();
    line.insert(
        "_aws".into(),
        json!({
            "Timestamp": timestamp_millis,
            "CloudWatchMetrics": [{
                "Namespace": namespace,
                "Dimensions": [[]],
                "Metrics": values.iter().map(|(name, _)| json!({ "Name": name, "Unit": "None" })).collect::<Vec<_>>(),
            }],
        }),
    );

    for (name, value) in properties {
        line.insert((*name).into(), json!(value));
    }
    for (name, value) in values {
        line.insert((*name).into(), json!(value));
    }
    Value::Object(line)
}

pub fn emit(values: &[(&str, f64)], properties: &[(&str, &str)]) {
    println!("{}", emf_line(&NAMESPACE, Utc::now().timestamp_millis(), values, properties));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emf_line_declares_its_metrics_without_dimensions_beside_their_values_and_properties() {
        let line = emf_line(
            "raffle-test",
            1_700_000_000_000,
            &[("SubscriptionsCharged", 3.0), ("SubscriptionsErrored", 0.0)],
            &[("raffleId", "winter")],
        );
        let declared = &line["_aws"]["CloudWatchMetrics"][0];
        let names: Vec<&Value> = declared["Metrics"]
            .as_array()
            .expect("a metric list")
            .iter()
            .map(|metric| &metric["Name"])
            .collect();

        assert_eq!(
            line["_aws"]["Timestamp"], 1_700_000_000_000_i64,
            "the timestamp CloudWatch files the line under"
        );
        assert_eq!(declared["Namespace"], "raffle-test", "the namespace the metrics land in");
        assert_eq!(declared["Dimensions"], json!([[]]), "one empty dimension set, so no series multiplies");
        assert_eq!(names, ["SubscriptionsCharged", "SubscriptionsErrored"], "every value is declared as a metric");
        assert_eq!(
            (&line["SubscriptionsCharged"], &line["SubscriptionsErrored"]),
            (&json!(3.0), &json!(0.0)),
            "each declared metric carries its value at the top level"
        );
        assert_eq!(line["raffleId"], "winter", "a property rides along undeclared");
    }
}
