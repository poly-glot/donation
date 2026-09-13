use std::time::Duration;

use lambda_runtime::{LambdaEvent, run, service_fn};

#[tokio::main]
async fn main() -> Result<(), lambda_runtime::Error> {
    shared::telemetry::init_logging();
    let url = std::env::var("BROWSE_URL")?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(8))
        .build()?;
    let (url, client) = (&url, &client);

    run(service_fn(async move |_event: LambdaEvent<serde_json::Value>| canary::probe(client, url).await)).await
}
