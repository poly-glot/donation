use lambda_http::{Request, run, service_fn};
use shared::stripe::StripeClient;
use shared::table::DynamoRepo;
use stripe_webhook::Webhook;

#[tokio::main]
async fn main() -> Result<(), lambda_http::Error> {
    shared::telemetry::init_logging();
    let repo = DynamoRepo::from_env().await?;
    let stripe = StripeClient::new(std::env::var("STRIPE_SECRET_KEY")?)?;
    let webhook = Webhook::new(repo, stripe, std::env::var("STRIPE_WEBHOOK_SECRET")?);
    let webhook = &webhook;

    run(service_fn(move |request: Request| webhook.handle(request))).await
}
