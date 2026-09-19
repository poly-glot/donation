# Testing

These rules bind exactly as `CLAUDE.md` does. Read this file before writing or changing any test.

## Placement and harness

- Pure rules get unit tests beside them in a `tests` module, table-driven with a `cases` array and
  a label per case. Anything that touches the table gets an integration test in the crate's
  `tests/` against DynamoDB Local through `shared::testing`.
- `local_repo` prints the skip notice when `AWS_ENDPOINT_URL_DYNAMODB` is unset, so a test body
  opens with nothing but `let Some(repo) = local_repo("x-test").await else { return; };` — never a
  second `eprintln!` per test. Its table name carries fresh entropy rather than a process id and a
  counter: a `-inMemory` DynamoDB Local outlives a run, so a deterministic name fails the second one
  with `ResourceInUseException`.
- Stripe is scripted with a local `impl PaymentGateway` in the test; the real client is exercised
  only in `crates/shared/tests/stripe_client.rs` with wiremock.
- Build rows through `shared::testing` — `raffle`, `prize`, `subscription`, `entrant`,
  `debit_payment`, `scripted` — rather than hand-rolling a struct literal a helper already produces.
- Tests may `tokio::spawn` to race the allocation transaction; that licence stops at the test
  boundary and never reaches a handler.

## One behaviour per test

- A test asserts one behaviour and its name says which one. A name joined with "and" over two
  arrangements is two tests: the credit-card and sold-out refunds were one test whose four events
  interleaved a capped raffle with an uncapped one. A name that lists the rows a single call writes
  is one story and stays whole.
- An integration test never restates a rule a pure test already owns. `pick_current` is unit-tested
  at four points in time, so the endpoint test proves only the wiring its unit test cannot reach:
  the status, the prizes and the route.

## Assertions

- Every row of a case table, and every assertion whose expectation is not self-evident, carries a
  message. Never assert a compound boolean: a tuple comparison says which half broke and
  `assert!(a && !b)` does not. Assert the error variant, not `is_err`. A slice pattern in a
  `let … else` states cardinality once, instead of asserting a length and then subscripting three
  times.
- Expect the value the system produced, not a constant restating it: the order view's total is
  compared with the total the checkout reported.

## Fixtures and helpers

- Name fixtures for their role and never for their values: ticket numbers in a draw script come
  from the sold orders (`sales.refunded.first_ticket`), ids say what the row is there to do
  (`ord-credit`), and a repeated convention is spelled once (`refund_of`, `tally`, `failed`,
  `order_donating`).
- An invariant of an endpoint belongs in the helper every test fetches through, not in the one test
  that happened to notice it: `order_view` asserts `cache-control: no-store` for all callers because
  an order view carries personal data. Repeated `get`/`unwrap` chains become a helper named for the
  question it answers — `tickets_sold`, `order_status`, `owner_of_ticket`, `refunded`.