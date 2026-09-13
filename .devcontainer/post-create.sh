#!/usr/bin/env bash
set -euo pipefail

sudo chown vscode:vscode "$CARGO_TARGET_DIR"
git config --global --add safe.directory /workspaces/donation
[ -f .env ] || cp .env.example .env

echo "DynamoDB Local is at $AWS_ENDPOINT_URL_DYNAMODB; run: cargo test --workspace"
echo "Put Stripe test keys in .env, then: scripts/dev.sh   (frontend on http://localhost:3000, webhooks forwarded)"
