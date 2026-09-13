use admin::Console;
use lambda_http::{Request, run, service_fn};
use shared::auth::Cognito;
use shared::table::DynamoRepo;

#[tokio::main]
async fn main() -> Result<(), lambda_http::Error> {
    shared::telemetry::init_logging();
    let console = Console::new(DynamoRepo::from_env().await?, Cognito::from_env()?);
    let console = &console;

    run(service_fn(async move |request: Request| {
        Ok::<_, lambda_http::Error>(console.handle(request).await)
    }))
    .await
}
