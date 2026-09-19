# Raffle: Rust on Lambda

A UK charity society-lottery raffle: £1 tickets, gapless ticket numbers allocated on payment, Gift
Aid donations, a VIP subscription charged when each raffle opens, a witnessed draw. Layout, commands
and deploy steps are in `README.md`. The data model, the flows, the compliance mapping and every
deliberate ceiling are in `specs/01-dynamodb-data-model.md`; read it before changing behaviour, and
record a new ceiling there rather than in code. The AWS resources live in the shared `aws-cloud`
repository, whose `README.md` holds the always-free-tier budgets and the $5-a-month ceiling that
every change must respect; this repository deploys code only. Before writing or changing a test,
read `docs/testing.md`; its rules bind exactly as this file does.

## 1. Invariants

These hold on every change, whatever the task.

- Never build a `PK`, `GSI1PK` or `GSI2PK` string by hand. Every partition key value, on the table
  and both indexes, goes through `table::partition`, whose `donation#` prefix is what lets the table
  be shared with other apps and what the shared platform's `dynamodb:LeadingKeys` fence matches.
- Logs and EMF properties never carry an email, name, address, date of birth, card detail or Stripe
  key. The `ENTRANT#` partition is the one place personal data lives; nothing else becomes a second.
- Money is integer pence. Ticket money and donation money stay in separate fields at every layer.
- Never `tokio::spawn` fire-and-forget inside a handler: the sandbox freezes the moment the response
  is returned and the task dies silently. (Tests may spawn to race the allocation transaction.)
- No scans and no N+1 gets: the role does not grant `Scan`, and a request does O(1) round trips by
  design.
- No new dependency, and no new feature on an existing one, without confirmation first. Check
  `Cargo.lock` before asking: most needs (`tracing`, `getrandom`, `serde_json`) are already present
  transitively.
- `AppError` in `crates/shared/src/error.rs` is the one fallible-API type and `anyhow` does not
  enter. Lambda mains convert it into `lambda_runtime::Error` at the boundary and nowhere else.
- No comments of any kind in new or changed code. Rationale lives here, in `docs/`, or in the spec.
  The existing `//!` and `///` blocks predate the rule; leave them alone and do not extend them.
- Never swallow a `Result` on a datastore or payment path with `.ok()`, `.unwrap_or_default()` or
  `let _ =`. (Defaulting an absent request header to empty is fine, because the signature check then
  rejects it.)
- The metric budget is 10 and 8 are in use; a new metric means removing one or asking first.
- Reserved concurrency of 1 on `subscription-charge`, `draw-run`, `reconcile` and `canary` is what
  makes their runs non-overlapping. Do not remove it.
- A change to an alarm, a schedule, a function URL or an environment variable is a change to
  `apps/donation.tf` in `aws-cloud`, never to this repository.

## 2. Code shape

- A function tells one story, and its body reads as paragraphs: a blank line between steps, each
  paragraph one step of the story — load, check, decide, write. The moment a paragraph needs a name
  to be understood, it becomes a function carrying that name. With comments banned, names and shape
  are the only explanation the reader gets; spend the effort there.
- Guards come first and the happy path stays flat. After the early returns, the main flow runs at
  one level of indentation; a third level of nesting is the signal to extract the inner block into a
  function named for what it decides.
- The reader's working memory is a budget like the metric budget. Name an intermediate `let` rather
  than extend a chain the reader must replay; keep a `let mut` confined to the paragraph that fills
  it and hand the result on immutably; declare at first use, not at the top.
- An iterator chain is idiomatic until it needs simulating. One transformation and one filter read
  at a glance; past that, break it into named stages or write the loop — whichever a stranger parses
  faster wins.
- Dispatch on domain state with an exhaustive `match`, never an `if`-ladder over the same enum: a
  new variant must fail compilation at every point that cares, which is what keeps change
  reasonable.
- One idea per function mirrors one behaviour per test: when a function's test wants "and" in its
  name, the function is asking to be split. (`docs/testing.md` owns the test half of this rule.)
- Reach for the language before a helper: `let … else` for absent rows, `let` chains
  (`if let Some(x) = a && cond`) instead of `.filter(|_| cond)` or nested `if let`, `LazyLock` for
  process-wide values, and `async move |x| …` closures where a plain closure would only wrap an
  `async move` block.

## 3. Ownership and dependencies

- Dependencies are workspace-managed in the root `Cargo.toml`; crates take `{ workspace = true }`.
- A type lives once, in the feature module that owns it (`raffle`, `entrant`, `order`,
  `subscription`, `draw`, `stripe`). A second crate imports it and never redeclares a copy; the
  webhook and reconcile Lambdas share `stripe::Charge` for exactly this reason. Every `DynamoRepo`
  method sits in an `impl` block inside the feature module whose rows it reads.

## 4. Error handling

- DynamoDB errors convert via `From` into `AppError::Dynamo` and Stripe errors into
  `AppError::Payment`. Never stringify them early: the HTTP mapping, `public_message` and
  `is_condition_failed` all need the typed error.
- Guards return early with the specific variant — `BadRequest` for input, `Forbidden` for licence
  rules, `Conflict` for state, `NotFound` for absent rows — and `let … else` reads the absent row.
- Conditional writes return `Result<bool, AppError>` through `condition_failed_as_false`; `false`
  means "lost the condition" and is never an error.
- `allocate_entry` classifies the positional cancellation reasons of its three-item transaction in
  `allocation_conflict`: `ConditionalCheckFailed` on the order item is `AlreadyPaid`; on the raffle
  or entry item it is `Retryable`, as is any reason in `RETRYABLE_CANCELLATIONS`
  (`TransactionConflict`, `ThrottlingError`, `ProvisionedThroughputExceeded`) on any item. All of
  them mean the attempt made no progress, so re-reading and retrying under the compare-and-set is
  safe.
- `is_condition_failed` requires a `ConditionalCheckFailed` reason before it calls a
  `TransactionCanceledException` a lost condition. DynamoDB reports a concurrent transaction on the
  same item as `TransactionConflict` and a throttled item as a throttling reason, never as a failed
  condition; capacity is fixed at the free 25 units, so a throttle is a live failure mode, and
  treating one as "condition failed" would make `record_draw` report it as "another run already
  claimed the draw" and skip the draw.
- The Stripe webhook answers 500 only for errors Stripe should retry. Terminal outcomes (`Failed`,
  `Unchanged`, `Ignored`, `AlreadyPaid`) answer 200 so Stripe stops redelivering.
- `expect` is allowed on OS entropy only.

## 5. Async and the Lambda runtime

- Handlers do their work inline and return, and nothing here should block the runtime; HMAC and OS
  entropy are microseconds. If real CPU work ever arrives, hand it to `tokio::task::spawn_blocking`
  and await it.
- Every loop over the network is bounded: DynamoDB paging by `last_evaluated_key`, the allocation
  compare-and-set by `ALLOCATION_ATTEMPTS`, the draw by `MAX_REDRAWS_PER_PRIZE`, Stripe listing by
  `has_more`. Retries sleep a jittered interval, never a fixed one.
- Each Lambda crate's `[[bin]]` is named after the package, never `bootstrap`. `cargo lambda build`
  renames the artifact to `bootstrap` inside `target/lambda/<package>/`, and `cargo lambda watch`
  serves it at `/lambda-url/<package>`; a binary called `bootstrap` collides across the workspace
  and is unreachable locally.

## 6. Tracing and metrics

- Every main calls `shared::telemetry::init_logging()` first; output is one JSON object per line
  with event fields flattened to the top level. Log with `tracing::{info,warn,error}!` plus fields,
  never `println!` or `eprintln!`.
- Instrument new public async functions with `#[tracing::instrument(skip_all, fields(...))]`, naming
  ids only. Fields carry ids and outcomes: `order_id`, `raffle_id`, `event_id`, `status`, `outcome`,
  `error`.
- A handler that can answer 5xx logs it at error level with a `status` field. The `Http5xx` metric
  filter in `aws-cloud` reads that field, so the browse, checkout and webhook availability SLOs
  depend on it.
- Metrics are EMF lines through `shared::telemetry::{emit,count}` and carry no dimensions: a
  dimension multiplies series.

## 7. Performance and memory

- Every DynamoDB round trip is latency and provisioned capacity on a 25-unit floor. The O(1) shapes
  are the design: one transaction allocates tickets, one query answers "everything for this
  entrant", one query finds the owner of ticket N. Think twice before widening the reconciliation's
  48-hour window.
- Hoist invariants out of loops and use a `HashSet` or `HashMap` for membership instead of
  `contains` inside a loop; `draw-run` keeps `won` as a set for this reason.
- Borrow rather than clone. `DynamoRepo` is cheap to clone because it wraps an `Arc`; domain structs
  are not, and are cloned only to hand ownership to a row write.
- Handlers run at 256 MB and the canary at 128 MB, all arm64 with `lto` and a single codegen unit.
  Keep dependency features minimal so cold starts stay short; the whole stack must fit 400k
  GB-seconds a month.
- The allocation counter is a compare-and-set on one item; its ceiling is roughly 50 paid orders a
  second per raffle. The spec names the queue-based upgrade path.

## 8. Tooling and workflow

- `rust-toolchain.toml` pins the toolchain and declares the `clippy` and `rustfmt` components, so
  any rustup-managed environment, the dev container and the `rust:1-slim` Docker image included,
  resolves the same compiler on first use. Bump the pin deliberately, in its own change, with the
  suite green.
- No cargo on the host. In the dev container run `cargo fmt --all`, `cargo clippy --workspace
  --all-targets` and `cargo test --workspace`. Off-container the Docker command in `README.md` runs
  the same, and `.github/workflows/test.yml` runs the three against a DynamoDB Local service
  container on every pull request and push to `main`, so a red suite is caught before the merge and
  never in the deploy.
- `rustfmt.toml` sets the line width. Format before submitting. Address clippy warnings; never
  `#[allow]` without a reason written next to it in the spec.
- A bug fix lands with the test that would have caught it, in the same change.