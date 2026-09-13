#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

if [ ! -f .env ]; then
    echo "No .env: copy .env.example to .env and put your Stripe test keys in it" >&2
    exit 1
fi
set -a
. ./.env
set +a

export AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-local}"
export AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-local}"
export AWS_ENDPOINT_URL_DYNAMODB="${AWS_ENDPOINT_URL_DYNAMODB:-http://localhost:8000}"
export AWS_REGION="${AWS_REGION:-eu-west-2}"
export METRIC_NAMESPACE="${METRIC_NAMESPACE:-donation}"
export TABLE_NAME="${TABLE_NAME:-raffle-local}"
export COGNITO_ENDPOINT="${COGNITO_ENDPOINT:-http://localhost:9229}"
export COGNITO_PUBLIC_URL="${COGNITO_PUBLIC_URL:-http://localhost:9229}"
export COGNITO_POOL_NAME="${COGNITO_POOL_NAME:-raffle-admins}"
export COGNITO_REDIRECT_URI="${COGNITO_REDIRECT_URI:-http://localhost:3000/admin.html}"
export COGNITO_USERNAME="${COGNITO_USERNAME:-admin@example.com}"
export COGNITO_PASSWORD="${COGNITO_PASSWORD:-Sup3rSecret!}"
: "${STRIPE_PUBLISHABLE_KEY:?put STRIPE_PUBLISHABLE_KEY in .env}"
: "${STRIPE_SECRET_KEY:?put STRIPE_SECRET_KEY in .env}"

cargo lambda --version >/dev/null 2>&1 || { echo "cargo-lambda not found: open this repo in the dev container, or install rustup and then pip install cargo-lambda ziglang" >&2; exit 1; }

PIDS=()
trap 'kill "${PIDS[@]}" 2>/dev/null' EXIT

scripts/local-table.sh
. scripts/cognito-local.sh
printf 'export default {\n    cognito: {\n        clientId: "%s",\n        domain: "%s",\n    },\n    stripePublishableKey: "%s",\n};\n' \
    "$COGNITO_CLIENT_ID" "$COGNITO_PUBLIC_URL" "$STRIPE_PUBLISHABLE_KEY" > frontend/config.js

export STRIPE_WEBHOOK_SECRET=""
if command -v stripe >/dev/null; then
    stripe listen --api-key "$STRIPE_SECRET_KEY" \
        --events charge.succeeded,charge.refunded,payment_intent.payment_failed \
        --forward-to http://localhost:9000/lambda-url/stripe-webhook/ > .stripe-listen.log 2>&1 &
    PIDS+=($!)
    for _ in $(seq 1 60); do
        grep -q 'Ready!' .stripe-listen.log && break
        sleep 1
    done
    STRIPE_WEBHOOK_SECRET=$(grep -o 'signing secret is whsec_[A-Za-z0-9]*' .stripe-listen.log | head -1 | sed 's/.* //')
    if [ -n "$STRIPE_WEBHOOK_SECRET" ]; then
        echo "forwarding Stripe test webhooks to the stripe-webhook function"
        tail -n +1 -f .stripe-listen.log | sed 's/^/stripe   /' &
        PIDS+=($!)
    else
        echo "stripe listen did not become ready; payments will stay pending:" >&2
        cat .stripe-listen.log >&2
    fi
else
    echo "stripe CLI not found; webhooks will not be forwarded and payments will stay pending" >&2
fi

cargo lambda watch --invoke-address 0.0.0.0 --invoke-port 9000 &
PIDS+=($!)

echo "waiting for the functions to compile…"
until curl -sf http://localhost:9000/lambda-url/api/raffles/current >/dev/null; do sleep 3; done
scripts/seed.sh

python3 frontend/serve.py &
PIDS+=($!)
echo "ready: http://localhost:3000"
wait
