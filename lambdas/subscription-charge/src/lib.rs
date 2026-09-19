use chrono::{DateTime, Utc};
use serde::Serialize;
use shared::error::AppError;
use shared::order::{Order, OrderStatus};
use shared::raffle::Raffle;
use shared::stripe::{ChargeOutcome, OffSessionCharge, PaymentGateway};
use shared::subscription::{Subscription, SubscriptionStatus};
use shared::table::DynamoRepo;
use shared::telemetry;

const SECONDS_PER_HOUR: f64 = 3_600.0;

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RaffleRun {
    pub raffle_id: String,
    pub charged: u32,
    pub declined: u32,
    pub skipped: u32,
    pub errored: u32,
}

enum Attempt {
    Charged,
    Declined,
    Skipped,
    Errored,
}

pub fn charge_lag_hours(raffle: &Raffle, now: DateTime<Utc>) -> f64 {
    (now - raffle.opens_at).num_seconds().max(0) as f64 / SECONDS_PER_HOUR
}

pub async fn run<G: PaymentGateway>(repo: &DynamoRepo, gateway: &G, now: DateTime<Utc>) -> Result<Vec<RaffleRun>, AppError> {
    let raffles = repo.list_raffles().await?;
    let mut runs = Vec::new();

    for raffle in raffles.iter().filter(|raffle| raffle.needs_subscription_charge(now)) {
        telemetry::emit(
            &[("SubscriptionChargeLagHours", charge_lag_hours(raffle, now))],
            &[("raffleId", &raffle.raffle_id)],
        );
        runs.push(charge_raffle(repo, gateway, raffle, now).await?);
    }
    Ok(runs)
}

async fn charge_raffle<G: PaymentGateway>(repo: &DynamoRepo, gateway: &G, raffle: &Raffle, now: DateTime<Utc>) -> Result<RaffleRun, AppError> {
    let mut run = RaffleRun {
        raffle_id: raffle.raffle_id.clone(),
        ..RaffleRun::default()
    };

    for subscription in &repo.due_subscriptions(raffle).await? {
        match charge_subscription(repo, gateway, raffle, subscription, now).await? {
            Attempt::Charged => run.charged += 1,
            Attempt::Declined => run.declined += 1,
            Attempt::Skipped => run.skipped += 1,
            Attempt::Errored => run.errored += 1,
        }
    }

    if run.errored == 0 {
        repo.mark_subscriptions_charged(&raffle.raffle_id, now).await?;
    }

    telemetry::emit(
        &[
            ("SubscriptionsCharged", f64::from(run.charged)),
            ("SubscriptionsDeclined", f64::from(run.declined)),
            ("SubscriptionsErrored", f64::from(run.errored)),
        ],
        &[("raffleId", &raffle.raffle_id)],
    );
    tracing::info!(raffle_id = %run.raffle_id, run.charged, run.declined, run.skipped, run.errored, "subscription charge run finished");
    Ok(run)
}

async fn charge_subscription<G: PaymentGateway>(
    repo: &DynamoRepo,
    gateway: &G,
    raffle: &Raffle,
    subscription: &Subscription,
    now: DateTime<Utc>,
) -> Result<Attempt, AppError> {
    let Some(order) = pending_order(repo, raffle, subscription, now).await? else {
        return Ok(Attempt::Skipped);
    };

    let outcome = match gateway.charge_off_session(charge_request(&order, raffle, subscription)).await {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::error!(order_id = %order.order_id, error = %err, "off-session charge errored");
            return Ok(Attempt::Errored);
        }
    };

    match outcome {
        ChargeOutcome::Succeeded { payment_intent_id } | ChargeOutcome::Pending { payment_intent_id } => {
            repo.set_order_payment_intent(&order.order_id, &payment_intent_id).await?;
            Ok(Attempt::Charged)
        }
        ChargeOutcome::Declined { payment_intent_id, code } => {
            if let Some(payment_intent_id) = payment_intent_id {
                repo.set_order_payment_intent(&order.order_id, &payment_intent_id).await?;
            }
            repo.set_order_status(&order.order_id, OrderStatus::Pending, OrderStatus::Failed).await?;
            repo.set_subscription_status(&subscription.subscription_id, SubscriptionStatus::PastDue).await?;
            tracing::warn!(order_id = %order.order_id, code = %code, "off-session charge declined");
            Ok(Attempt::Declined)
        }
    }
}

async fn pending_order(repo: &DynamoRepo, raffle: &Raffle, subscription: &Subscription, now: DateTime<Utc>) -> Result<Option<Order>, AppError> {
    let order = Order::from_subscription(raffle, subscription, now);
    if repo.create_order(&order).await? {
        return Ok(Some(order));
    }

    let existing = repo.get_order(&order.order_id).await?;
    Ok(existing.filter(|order| order.status == OrderStatus::Pending && order.stripe_payment_intent_id.is_none()))
}

fn charge_request(order: &Order, raffle: &Raffle, subscription: &Subscription) -> OffSessionCharge {
    OffSessionCharge {
        amount_pence: order.total_pence,
        customer_id: subscription.stripe_customer_id.clone(),
        payment_method_id: subscription.stripe_payment_method_id.clone(),
        idempotency_key: order.order_id.clone(),
        description: format!("{}: {} subscription tickets", raffle.name, order.ticket_quantity),
        metadata: vec![
            ("orderId".into(), order.order_id.clone()),
            ("raffleId".into(), raffle.raffle_id.clone()),
            ("entrantId".into(), order.entrant_id.clone()),
            ("subscriptionId".into(), subscription.subscription_id.clone()),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use shared::testing::winter;

    #[test]
    fn charge_lag_counts_hours_since_open_and_never_goes_negative() {
        let raffle = winter();
        let opens_at = raffle.opens_at;

        assert_eq!(charge_lag_hours(&raffle, opens_at + Duration::minutes(90)), 1.5);
        assert_eq!(charge_lag_hours(&raffle, opens_at - Duration::hours(2)), 0.0);
    }
}
