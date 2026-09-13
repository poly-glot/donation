#!/usr/bin/env bash
set -euo pipefail

ENDPOINT="$COGNITO_ENDPOINT"
PORT="${ENDPOINT##*:}"

if ! curl -s -o /dev/null "$ENDPOINT"; then
    command -v docker >/dev/null || { echo "Cognito Local is not answering at $ENDPOINT and docker is not installed to start it" >&2; exit 1; }
    echo "starting Cognito Local in Docker (container raffle-cognito, stop it with: docker rm -f raffle-cognito)"
    docker rm -f raffle-cognito >/dev/null 2>&1 || true
    docker run -d --rm --name raffle-cognito -p "$PORT:9229" jagregory/cognito-local:5.3.0 >/dev/null
    for _ in $(seq 1 30); do
        curl -s -o /dev/null "$ENDPOINT" && break
        sleep 1
    done
fi

idp() { aws --endpoint-url "$ENDPOINT" cognito-idp "$@"; }

POOL=$(idp list-user-pools --max-results 10 --query "UserPools[?Name=='$COGNITO_POOL_NAME'].Id | [0]" --output text)
if [ "$POOL" = "None" ] || [ -z "$POOL" ]; then
    POOL=$(idp create-user-pool --pool-name "$COGNITO_POOL_NAME" --query UserPool.Id --output text)
    echo "user pool $POOL created"
fi

CLIENT=$(idp list-user-pool-clients --user-pool-id "$POOL" --max-results 10 --query "UserPoolClients[?ClientName=='console'].ClientId | [0]" --output text)
if [ "$CLIENT" = "None" ] || [ -z "$CLIENT" ]; then
    CLIENT=$(idp create-user-pool-client --user-pool-id "$POOL" --client-name console \
        --callback-urls "$COGNITO_REDIRECT_URI" \
        --allowed-o-auth-flows code --allowed-o-auth-scopes openid --allowed-o-auth-flows-user-pool-client \
        --supported-identity-providers COGNITO \
        --explicit-auth-flows ALLOW_USER_PASSWORD_AUTH ALLOW_REFRESH_TOKEN_AUTH \
        --query UserPoolClient.ClientId --output text)
    echo "app client $CLIENT created"
fi

if ! idp admin-get-user --user-pool-id "$POOL" --username "$COGNITO_USERNAME" >/dev/null 2>&1; then
    idp admin-create-user --user-pool-id "$POOL" --username "$COGNITO_USERNAME" --message-action SUPPRESS >/dev/null
    idp admin-set-user-password --user-pool-id "$POOL" --username "$COGNITO_USERNAME" --password "$COGNITO_PASSWORD" --permanent
    echo "administrator $COGNITO_USERNAME created"
fi

export COGNITO_CLIENT_ID="$CLIENT"
export COGNITO_ISSUER="http://0.0.0.0:${PORT}/${POOL}"
export COGNITO_JWKS_URL="${ENDPOINT}/${POOL}/.well-known/jwks.json"
