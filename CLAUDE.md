# Raffle: Rust on Lambda

A UK charity society-lottery raffle: £1 tickets, gapless ticket numbers allocated on payment, Gift Aid
donations, a VIP subscription charged when each raffle opens, a witnessed draw. Layout, commands and
deploy steps are in `README.md`. The data model, the flows, the compliance mapping and every deliberate
ceiling are in `specs/01-dynamodb-data-model.md`; read it before changing behaviour, and record a new
ceiling there rather than in code. The AWS resources live in the shared `aws-cloud` repository,
whose `README.md` holds the always-free-tier budgets and the $5-a-month ceiling that every change must
respect; this repository deploys code only.

## 1. Coding standards

- Dependencies are workspace-managed in the root `Cargo.toml`; crates take `{ workspace = true }`. Do not
  add a dependency or enable a feature without confirmation first. Check `Cargo.lock` before asking: most
  needs (`tracing`, `getrandom`, `serde_json`) were already present transitively.
- `AppError` in `crates/shared/src/error.rs` is the one fallible-API type: a `thiserror` enum whose
  variants map to HTTP status and whose `From` impls absorb DynamoDB, Stripe, serde_dynamo and builder
  errors. Lambda mains convert it into `lambda_runtime::Error` at the boundary and nowhere else. There is
  no `anyhow`; do not introduce it.
- Guards return early with the specific variant: `BadRequest` for input, `Forbidden` for licence rules,
  `Conflict` for state, `NotFound` for absent rows. Use `let … else` for absent rows.
- A type lives once, in the feature module that owns it (`raffle`, `entrant`, `order`, `subscription`,
  `draw`, `stripe`). A second crate imports it and never redeclares a copy; the webhook and reconcile
  Lambdas share `stripe::Charge` for exactly this reason. Every `DynamoRepo` method sits in an `impl`
  block inside the feature module whose rows it reads.
- No comments of any kind in new or changed code. Rationale goes in this file or the spec. The existing
  `//!` and `///` blocks predate the rule; leave them alone and do not extend them.
- Money is integer pence. Ticket money and donation money stay in separate fields at every layer.
- Every partition key value, on the table and both indexes, starts with `donation#` through
  `table::partition`. Never build a `PK`, `GSI1PK` or `GSI2PK` string by hand: the prefix is what lets the
  table be shared with other apps and what the shared platform's `dynamodb:LeadingKeys` fence matches.

## 2. Async and concurrency

- Handlers do their work inline and return. Never `tokio::spawn` fire-and-forget inside a Lambda: the
  sandbox freezes the moment the response is returned and the task dies silently. Tests may spawn to race
  the allocation transaction.
- Nothing here should block the runtime; HMAC and OS entropy are microseconds. If real CPU work ever
  arrives, hand it to `tokio::task::spawn_blocking` and await it.
- Every loop over the network is bounded: DynamoDB paging by `last_evaluated_key`, the allocation
  compare-and-set by `ALLOCATION_ATTEMPTS`, the draw by `MAX_REDRAWS_PER_PRIZE`, Stripe listing by
  `has_more`. Retries sleep a jittered interval, never a fixed one.
- Reserved concurrency of 1 on `subscription-charge`, `draw-run`, `reconcile` and `canary` is what makes
  their runs non-overlapping. Do not remove it.
- Each Lambda crate's `[[bin]]` is named after the package, never `bootstrap`. `cargo lambda build`
  renames the artifact to `bootstrap` inside `target/lambda/<package>/`, and `cargo lambda watch` serves
  it at `/lambda-url/<package>`; a binary called `bootstrap` collides across the workspace and is
  unreachable locally.

## 3. Tracing and logging

- Every main calls `shared::telemetry::init_logging()` first; output is one JSON object per line with
  event fields flattened to the top level. Log with `tracing::{info,warn,error}!` plus fields, never
  `println!` or `eprintln!`.
- Instrument new public async functions with `#[tracing::instrument(skip_all, fields(...))]`, naming ids
  only.
- Fields carry ids and outcomes: `order_id`, `raffle_id`, `event_id`, `status`, `outcome`, `error`. Never
  an email, name, address, date of birth, card detail or Stripe key. The `ENTRANT#` partition is the one
  place personal data lives; logs and EMF properties must not become a second.
- A handler that can answer 5xx logs it at error level with a `status` field. The `Http5xx` metric
  filter in `aws-cloud` reads that field, so the browse, checkout and webhook availability SLOs depend on it.
- Metrics are EMF lines through `shared::telemetry::{emit,count}`. The budget is 10 custom metrics and 8
  are in use. A dimension multiplies series, which is why none are dimensioned. Do not add a metric
  without removing one or asking.

## 4. Error handling

- DynamoDB errors convert via `From` into `AppError::Dynamo` and Stripe errors into `AppError::Payment`.
  Never stringify them early: the HTTP mapping, `public_message` and `is_condition_failed` all need the
  typed error.
- Conditional writes return `Result<bool, AppError>` through `condition_failed_as_false`; `false` means
  "lost the condition" and is never an error. `allocate_entry` classifies the positional cancellation
  reasons of its three-item transaction in `allocation_conflict`: `ConditionalCheckFailed` on the order
  item is `AlreadyPaid`; on the raffle or entry item it is `Retryable`, and so is any reason in
  `RETRYABLE_CANCELLATIONS` — `TransactionConflict`, `ThrottlingError`, `ProvisionedThroughputExceeded` —
  on any item. All of them mean the attempt made no progress, so re-reading and retrying under the
  compare-and-set is safe. The separate paths exist because DynamoDB reports a concurrent transaction on
  the same item as `TransactionConflict`, and a throttled item as a throttling reason, neither of them as
  a failed condition.
- A cancelled transaction is a lost condition only when a reason says so. `is_condition_failed` therefore
  requires a `ConditionalCheckFailed` reason before it calls a `TransactionCanceledException` a lost
  condition: capacity is fixed at the free 25 units, so a throttled transaction is a live failure mode,
  and treating one as "condition failed" would make `record_draw` report a throttle as "another run
  already claimed the draw" and skip the draw.
- Never swallow a `Result` on a datastore or payment path with `.ok()`, `.unwrap_or_default()` or
  `let _ =`. Defaulting an absent request header to empty is fine because the signature check then
  rejects it.
- The Stripe webhook answers 500 only for errors Stripe should retry. Terminal outcomes (`Failed`,
  `Unchanged`, `Ignored`, `AlreadyPaid`) answer 200 so Stripe stops redelivering.
- `expect` is allowed on OS entropy only.

## 5. Testing and linting

- Pure rules get unit tests beside them in a `tests` module, table-driven with a `cases` array and a
  label per case. Anything that touches the table gets an integration test in the crate's `tests/`
  against DynamoDB Local through `shared::testing`. `local_repo` prints the skip notice when
  `AWS_ENDPOINT_URL_DYNAMODB` is unset, so a test body opens with nothing but
  `let Some(repo) = local_repo("x-test").await else { return; };` — never a second `eprintln!` per test.
  Its table name carries fresh entropy rather than a process id and a counter: a `-inMemory` DynamoDB
  Local outlives a run, so a deterministic name fails the second one with `ResourceInUseException`.
  Stripe is scripted with a local `impl PaymentGateway` in the test; the real client is exercised only in
  `crates/shared/tests/stripe_client.rs` with wiremock.
- A bug fix lands with the test that would have caught it, in the same change.
- A test asserts one behaviour and its name says which one. A name joined with "and" over two
  arrangements is two tests: the credit-card and sold-out refunds were one test whose four events
  interleaved a capped raffle with an uncapped one. A name that lists the rows a single call writes is
  one story and stays whole.
- An integration test never restates a rule a pure test already owns. `pick_current` is unit-tested at
  four points in time, so the endpoint test proves only the wiring its unit test cannot reach: the
  status, the prizes and the route.
- Every row of a case table, and every assertion whose expectation is not self-evident, carries a
  message. Never assert a compound boolean: a tuple comparison says which half broke and
  `assert!(a && !b)` does not. Assert the error variant, not `is_err`. A slice pattern in a `let … else`
  states cardinality once, instead of asserting a length and then subscripting three times.
- Expect the value the system produced, not a constant restating it: the order view's total is compared
  with the total the checkout reported. Name fixtures for their role and never for their values — ticket
  numbers in a draw script come from the sold orders (`sales.refunded.first_ticket`), ids say what the
  row is there to do (`ord-credit`), and a repeated convention is spelled once (`refund_of`, `tally`,
  `failed`, `order_donating`).
- An invariant of an endpoint belongs in the helper every test fetches through, not in the one test that
  happened to notice it: `order_view` asserts `cache-control: no-store` for all callers because an order
  view carries personal data. Repeated `get`/`unwrap` chains become a helper named for the question it
  answers — `tickets_sold`, `order_status`, `owner_of_ticket`, `refunded`.
- Build rows through `shared::testing` — `raffle`, `prize`, `subscription`, `entrant`, `debit_payment`,
  `scripted` — rather than hand-rolling a struct literal a helper already produces.
- `rust-toolchain.toml` pins the toolchain and declares the `clippy` and `rustfmt` components, so any
  rustup-managed environment, the dev container and the `rust:1-slim` Docker image included, resolves the
  same compiler on first use. Bump the pin deliberately, in its own change, with the suite green.
- No cargo on the host. In the dev container run `cargo fmt --all`, `cargo clippy --workspace
  --all-targets` and `cargo test --workspace`. Off-container the Docker command in `README.md` runs the
  same, and `.github/workflows/test.yml` runs the three against a DynamoDB Local service container on
  every pull request and push to `main`, so a red suite is caught before the merge and never in the
  deploy.
- Reach for the language before a helper: `let … else` for absent rows, `let` chains
  (`if let Some(x) = a && cond`) instead of `.filter(|_| cond)` or nested `if let`, `LazyLock` for
  process-wide values, and `async move |x| …` closures where a plain closure would only wrap an
  `async move` block.
- `rustfmt.toml` sets the line width. Format before submitting. Address clippy warnings; never `#[allow]`
  without a reason written next to it in the spec.
- A change to an alarm, a schedule, a function URL or an environment variable is a change to
  `apps/donation.tf` in `aws-cloud`, never to this repository.

## 6. Performance and memory

- Every DynamoDB round trip is latency and provisioned capacity, and the free floor is 25 units. A
  request does O(1) round trips by design: one transaction allocates tickets, one query answers
  "everything for this entrant", one query finds the owner of ticket N. No N+1 gets, no scans (the role
  does not grant `Scan`), and think twice before widening the reconciliation's 48-hour window.
- Hoist invariants out of loops and use a `HashSet` or `HashMap` for membership instead of `contains`
  inside a loop; `draw-run` keeps `won` as a set for this reason.
- Borrow rather than clone. `DynamoRepo` is cheap to clone because it wraps an `Arc`; domain structs are
  not. Clone only to hand ownership to a row write.
- Handlers run at 256 MB and the canary at 128 MB, all arm64 with `lto` and a single codegen unit. Keep
  dependency features minimal so cold starts stay short; the whole stack must fit 400k GB-seconds a
  month.
- The allocation counter is a compare-and-set on one item; its ceiling is roughly 50 paid orders a
  second per raffle. The spec names the queue-based upgrade path.
