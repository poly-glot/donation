use chrono::Utc;
use lambda_runtime::{LambdaEvent, run, service_fn};
use shared::stripe::StripeClient;
use shared::table::DynamoRepo;

#[tokio::main]
async fn main() -> Result<(), lambda_runtime::Error> {
    shared::telemetry::init_logging();
    let repo = DynamoRepo::from_env().await?;
    let stripe = StripeClient::new(std::env::var("STRIPE_SECRET_KEY")?)?;
    let (repo, stripe) = (&repo, &stripe);

    run(service_fn(async move |_event: LambdaEvent<serde_json::Value>| {
        reconcile::run(repo, stripe, Utc::now()).await.map_err(lambda_runtime::Error::from)
    }))
    .await
}
