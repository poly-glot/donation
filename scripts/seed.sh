#!/usr/bin/env bash
set -euo pipefail

LAMBDA="${LAMBDA_URL:-http://localhost:9000}"

TOKEN=$(aws --endpoint-url "$COGNITO_ENDPOINT" cognito-idp initiate-auth \
    --auth-flow USER_PASSWORD_AUTH --client-id "$COGNITO_CLIENT_ID" \
    --auth-parameters "USERNAME=$COGNITO_USERNAME,PASSWORD=$COGNITO_PASSWORD" \
    --query AuthenticationResult.AccessToken --output text)

invoke() {
    curl -sS -X POST "$LAMBDA/lambda-url/admin/" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' -d "$1"
    echo
}

read -r opens closes draw results < <(python3 -c 'from datetime import datetime, timedelta, timezone
now = datetime.now(timezone.utc)
print(*((now + timedelta(days=days)).strftime("%Y-%m-%dT%H:%M:%SZ") for days in (-1, 100, 114, 128)))')

invoke "{\"action\":\"createRaffle\",\"raffleId\":\"winter-2026\",\"name\":\"Winter Poppy Raffle 2026\",\"ticketPricePence\":100,\"maxTicketsPerOrder\":20,\"maxTickets\":5000000,\"opensAt\":\"$opens\",\"closesAt\":\"$closes\",\"drawAt\":\"$draw\",\"resultsAt\":\"$results\"}"
invoke '{"action":"putPrize","raffleId":"winter-2026","rank":1,"name":"First prize","amountPence":2000000,"quantity":1}'
invoke '{"action":"putPrize","raffleId":"winter-2026","rank":2,"name":"Second prize","amountPence":500000,"quantity":1}'
invoke '{"action":"putPrize","raffleId":"winter-2026","rank":3,"name":"Runner-up prizes","amountPence":10000,"quantity":50}'
