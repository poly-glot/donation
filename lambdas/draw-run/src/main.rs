use draw_run::Ceremony;
use lambda_http::{Request, run, service_fn};
use shared::auth::Cognito;
use shared::table::DynamoRepo;

#[tokio::main]
async fn main() -> Result<(), lambda_http::Error> {
    shared::telemetry::init_logging();
    let ceremony = Ceremony::new(DynamoRepo::from_env().await?, Cognito::from_env()?);
    let ceremony = &ceremony;

    run(service_fn(async move |request: Request| {
        Ok::<_, lambda_http::Error>(ceremony.handle(request).await)
    }))
    .await
}
