use api::Api;
use lambda_http::{Request, run, service_fn};
use shared::stripe::StripeClient;
use shared::table::DynamoRepo;

#[tokio::main]
async fn main() -> Result<(), lambda_http::Error> {
    shared::telemetry::init_logging();
    let repo = DynamoRepo::from_env().await?;
    let stripe = StripeClient::new(std::env::var("STRIPE_SECRET_KEY")?)?;
    let api = Api::new(repo, stripe);
    let api = &api;

    run(service_fn(async move |request: Request| Ok::<_, lambda_http::Error>(api.handle(request).await))).await
}
