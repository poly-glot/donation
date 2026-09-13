# Data model: charity raffle on Stripe and DynamoDB

## Overview

This is the storage design for a UK charity raffle run as a society lottery: £1 tickets sold online during a fixed
window, a draw on a published date, an optional donation with Gift Aid, and a VIP subscription that enters the supporter
in every future raffle. Stripe takes the payments. The code is Rust on AWS Lambda.

Everything lives in one DynamoDB table with two secondary indexes. The conventions come from the
`ticketlist-aws` ticket-sales system. What a raffle does not need, such as queues, baskets and sharded stock, has been
left out.

### What the screens require

| Requirement from the mockups                                                   | Where it lands                                                         |
|--------------------------------------------------------------------------------|------------------------------------------------------------------------|
| Raffle has open, close, draw and results dates; "not yet live", closed         | Dates on the raffle row; status is derived from them, never stored     |
| £1 tickets, quick picks of 15, 10 and 5, "Other (max 20)"                      | `ticketPricePence` and `maxTicketsPerOrder` on the raffle              |
| 400 prizes in £20k, £5k and £1k tiers                                          | One prize row per tier, one winner row per prize awarded               |
| Additional donation of £5, £10, £15 or own amount                              | `donationPence` on the order, kept apart from ticket money             |
| Gift Aid yes or no, covering four years back and the future                    | Gift Aid declaration rows with a snapshot of the donor                 |
| VIP subscription: 10 tickets, charged ahead of each draw, from the next raffle | Subscription row with `eligibleFrom`; one predictable order per raffle |
| Title, names, email, phone, date of birth, 18 or over                          | Entrant profile; the age check runs at order time                      |
| Email and post marketing yes or no                                             | Consent rows, append-only                                              |
| Great Britain residents only, with postcode lookup                             | Address on the entrant; BT, GY, IM and JE postcodes are rejected       |
| Debit cards only (Gambling Commission)                                         | `cardFunding` recorded on the order; a Stripe Radar rule blocks credit |
| Licence number, responsible person, charity number                             | Deployment configuration printed on receipts, not row data             |

### Design principles

1. **One table, two indexes.** The first index answers "everything for one supporter", "supporter by email" and "list
   the raffles". The second answers "order by Stripe PaymentIntent" and "subscriptions by status".
2. **Tickets are allocated on payment, never reserved.** Raffle tickets are not scarce. The only cap is the licence
   limit on proceeds, so there is no stock hold, no release and no basket expiry.
3. **Ticket numbers are gapless, by compare-and-set.** One transaction moves the raffle's ticket counter, writes the
   ledger row and marks the order paid. The counter update succeeds only if the counter still holds the value just read.
   A lost race re-reads and retries after a random delay. A duplicate webhook fails the order's status condition and
   returns "already paid" without consuming numbers.
4. **Money is integer pence.** Ticket money and donation money are separate columns everywhere. Only the donation
   qualifies for Gift Aid, and the two report to different regulators.
5. **Raffle status is derived from dates, never stored.** Scheduled, open and closed follow from the open and close
   dates; drawn follows from the draw timestamp. No scheduler flips state, so nothing can drift.
6. **Stripe holds the card, we hold the calendar.** The subscription is our record. Stripe keeps the customer and the
   debit card saved during the sign-up purchase. When a raffle opens, we create one off-session payment per subscriber,
   so charges land ahead of each draw however irregular the calendar. Moving to Stripe Subscriptions would change the
   trigger, not the data.
7. **Personal data lives in one place.** The entrant partition holds it. Ledger rows for orders, entries and winners
   carry only the entrant id. Erasure rewrites the profile and leaves the gambling ledger intact.
8. **Append-only where a regulator may ask what was agreed, and when.** Consent and Gift Aid are timestamped rows, never
   overwritten flags.
9. **No `updatedAt` columns.** The table's change stream records every write and when it happened. Rows carry creation
   and domain timestamps only.

## The table

DynamoDB is a key-value store. Every row has a partition key, which groups rows, and a sort key, which orders rows
within a group. A query reads one partition, optionally a range of sort keys, and cannot join. A global secondary index
is a second copy of the table under different keys, so a row can also be found by another attribute. All of the entities
here share one table and are told apart by key prefixes, so a related set of rows, such as a raffle and its prizes, sits
in one partition and is read in one query.

The table is the shared `aws-cloud` table, which this app reaches through the `donation#` key prefix. It is
provisioned at a fixed free-tier 25 read and 25 write units, split 15 on the table and 5 on each index, with no
autoscaling. It uses the AWS-owned encryption key, has no point-in-time recovery, and streams old and new images of
every change for the audit trail. See `modules/table` in `aws-cloud`.

| Attribute | Type   | Purpose                                                      |
|-----------|--------|--------------------------------------------------------------|
| `PK`      | string | partition key                                                |
| `SK`      | string | sort key                                                     |
| `GSI1PK`  | string | partition of the entrant, email and raffle-list index        |
| `GSI1SK`  | string | sort key of that index, ordered by time or raffle            |
| `GSI2PK`  | string | partition of the PaymentIntent and subscription-status index |

| Index | Partition | Sort     | Projection |
|-------|-----------|----------|------------|
| GSI1  | `GSI1PK`  | `GSI1SK` | all        |
| GSI2  | `GSI2PK`  | none     | all        |

GSI2 has no sort key. PaymentIntent lookups are exact, and subscription pages need no order.

## Entity map

| Entity               | PK                              | SK                      | GSI1PK                   | GSI1SK                             | GSI2PK                   |
|----------------------|---------------------------------|-------------------------|--------------------------|------------------------------------|--------------------------|
| Raffle               | `donation#RAFFLE#{raffleId}`    | `#METADATA`             | `donation#RAFFLES`       | `{opensAt}#{raffleId}`             |                          |
| Prize tier           | `donation#RAFFLE#{raffleId}`    | `PRIZE#{rank:04}`       |                          |                                    |                          |
| Entry (ticket run)   | `donation#RAFFLE#{raffleId}`    | `ENTRY#{ticketFrom:08}` | `donation#ENTRANT#{id}`  | `ENTRY#{raffleId}#{ticketFrom:08}` |                          |
| Draw record          | `donation#RAFFLE#{raffleId}`    | `#DRAW`                 |                          |                                    |                          |
| Winner               | `donation#RAFFLE#{raffleId}`    | `WINNER#{seq:04}`       | `donation#ENTRANT#{id}`  | `WINNER#{raffleId}#{seq:04}`       |                          |
| Entrant profile      | `donation#ENTRANT#{entrantId}`  | `#PROFILE`              | `donation#EMAIL#{email}` | `#PROFILE`                         |                          |
| Marketing consent    | `donation#ENTRANT#{entrantId}`  | `CONSENT#{recordedAt}`  |                          |                                    |                          |
| Gift Aid declaration | `donation#ENTRANT#{entrantId}`  | `GIFTAID#{declaredAt}`  |                          |                                    |                          |
| Order                | `donation#ORDER#{orderId}`      | `#METADATA`             | `donation#ENTRANT#{id}`  | `ORDER#{createdAt}`                | `donation#PI#{piId}`     |
| Subscription         | `donation#SUB#{subscriptionId}` | `#METADATA`             | `donation#ENTRANT#{id}`  | `SUB#{createdAt}`                  | `donation#SUBS#{status}` |

Every partition key value, on the table and both indexes, starts with the app name so the table can be shared with other
apps and each app's role can be fenced with an IAM `dynamodb:LeadingKeys` condition on its prefix. `table::partition` is
the one place that prefix is applied. Sort keys carry no prefix; they never leave their partition.

Timestamps in sort keys are RFC 3339 to the millisecond with a `Z` suffix, so they sort as text in time order. Ticket
numbers are zero-padded to eight digits, enough for a £5 million lottery at £1 a ticket. Prize ranks and winner
sequences are padded to four. The email key is lower-cased and trimmed.

## Entities

Attribute names are camelCase on the wire. The Rust structs in the shared crate's feature modules are the source of
truth. Optional attributes are absent rather than null.

### Raffle

| Attribute                | Example                                        | Notes                                                        |
|--------------------------|------------------------------------------------|--------------------------------------------------------------|
| `raffleId`               | `winter-2026`                                  | letters, digits, `-` and `_`                                 |
| `name`                   | `Winter Poppy Raffle 2026`                     |                                                              |
| `ticketPricePence`       | `100`                                          | cannot change once tickets are sold                          |
| `maxTicketsPerOrder`     | `20`                                           | the "Other (max 20)" limit                                   |
| `maxTickets`             | `5000000`                                      | licence proceeds cap, enforced in the allocation transaction |
| `opensAt`, `closesAt`    | `2026-09-30T00:00:00Z`, `2027-01-08T23:59:59Z` | status derives from these                                    |
| `drawAt`, `resultsAt`    | `2027-01-22…`, `2027-02-05…`                   | published dates; results two weeks after the draw            |
| `drawnAt`                | absent until the draw                          | set by the draw transaction; makes the status drawn          |
| `ticketsSold`            | `25`                                           | monotonic counter; also the upper bound for the draw         |
| `ticketRevenuePence`     | `2500`                                         | lottery proceeds, reported to the Gambling Commission        |
| `donationPence`          | `500`                                          | voluntary donations, not lottery proceeds                    |
| `subscriptionsChargedAt` | absent until charged                           | guards the one-shot subscription charge run                  |
| `createdAt`              |                                                |                                                              |

Dates must run `opensAt < closesAt <= drawAt <= resultsAt`.

### Prize tier

`rank`, `name`, `amountPence` and `quantity`. Four hundred prizes are a handful of tiers: one at £20,000, one at £5,000,
one at £1,000 and many smaller. Winners reference the tier by rank.

### Entrant

| Attribute                                               | Notes                                                                                                                   |
|---------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------|
| `title`, `firstName`, `lastName`, `email`, `telephone?` | form fields; the email also drives the `EMAIL#` index key                                                               |
| `dateOfBirth`                                           | ISO date; adult means the eighteenth birthday is on or before today                                                     |
| `address`                                               | `{line1, line2?, town, postcode, country}`; the country must be `GB` and the postcode area must not be BT, GY, IM or JE |
| `stripeCustomerId?`                                     | created on the first purchase that ticks subscribe; required for subscriptions                                          |
| `selfExcludedUntil?`                                    | blocks purchases while in the future                                                                                    |
| `erasedAt?`                                             | set by erasure; the row is rewritten with `[erased]` and the email index keys are dropped                               |
| `createdAt`                                             |                                                                                                                         |

### Marketing consent (append-only)

`recordedAt`, `email` and `post` as booleans, `wordingVersion` and `source`, such as `web-checkout`. The latest row is
the current preference. The history is the evidence for the Privacy and Electronic Communications Regulations.

### Gift Aid declaration (append-only)

`declaredAt`, `isUkTaxpayer`, `wordingVersion` and a `donor` snapshot of title, names and address. HMRC requires the
declaration to carry the donor's name and home address as given at the time, so the snapshot survives later address
changes and erasure. A row with `isUkTaxpayer` false records a withdrawal. Every checkout writes a row, whether the
answer was yes or no.

### Order

| Attribute                             | Notes                                                                                                    |
|---------------------------------------|----------------------------------------------------------------------------------------------------------|
| `orderId`                             | `ord_` plus a random id for single purchases; `sub_{subscriptionId}_{raffleId}` for subscription charges |
| `raffleId`, `entrantId`               |                                                                                                          |
| `subscriptionId?`                     | present on subscription charges only                                                                     |
| `ticketQuantity`, `ticketAmountPence` | lottery money                                                                                            |
| `donationPence`, `giftAid`            | donation money; Gift Aid applies to this amount only                                                     |
| `totalPence`                          | what Stripe charged, always GBP                                                                          |
| `subscribe`                           | the sign-up checkbox; the webhook creates the subscription from the saved card                           |
| `status`                              | `PENDING` to `PAID` on allocation or `FAILED`; `PAID` to `REFUNDED`                                      |
| `stripePaymentIntentId?`              | set when the PaymentIntent is created; drives the `PI#` index key                                        |
| `cardFunding?`, `cardLast4?`          | from the charge; `debit` is the only acceptable funding type                                             |
| `paidAt?`                             | set by the allocation transaction; the ticket range lives on the entry row                               |
| `createdAt`                           |                                                                                                          |

### Entry (the ticket ledger)

One row per paid order: `ticketFrom`, `ticketTo`, `orderId`, `entrantId` and `allocatedAt`. Ranges are contiguous and
gapless across a raffle. "Who holds ticket N" is one query: the raffle partition, sort keys from `ENTRY#00000001` to
`ENTRY#{N}`, descending, limit one, then check that the range covers N.

### Subscription

| Attribute                                   | Notes                                                                                                                                                          |
|---------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `subscriptionId`                            | `sub_{entrantId}`, so a supporter has at most one and re-subscribing reuses the row                                                                            |
| `stripeCustomerId`, `stripePaymentMethodId` | the debit card saved from the sign-up purchase                                                                                                                 |
| `ticketsPerRaffle`                          | `10`                                                                                                                                                           |
| `eligibleFrom`                              | the close date of the raffle open at sign-up, otherwise the sign-up time, so the subscription starts with the next raffle rather than the one just bought into |
| `status`                                    | `ACTIVE`, `PAST_DUE` or `CANCELLED`, mirrored into the `SUBS#` index key; phone cancellations are an admin status change                                       |
| `createdAt`                                 |                                                                                                                                                                |

A subscription is due for a raffle when it is active and `eligibleFrom` is on or before the raffle's open date. The
order id for that pair is predictable, so the conditional put in DynamoDB and the idempotency key in Stripe together
make the charge exactly once per subscription and raffle.

### Draw and winner

The draw record holds `drawnAt`, `ticketsSold` at draw time, `method` (`os-csprng-uniform-rejection`),
`conductedBy` and `witnessedBy?`. It is written once, in a transaction that also sets `drawnAt` on the raffle.

Each winner row holds `sequence`, `prizeRank`, `prizeAmountPence`, `ticketNumber`, `orderId`,
`entrantId` and `status`. Status moves from `PENDING` to `NOTIFIED` to `PAID`, or to `UNCLAIMED`, by admin action. The
table stream records when each transition happened.

## Access patterns

| #  | Need                                                    | Query                                                                                                                                             |
|----|---------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------|
| 1  | Raffle page and status                                  | get `donation#RAFFLE#{id}` / `#METADATA`                                                                                                          |
| 2  | Current, previous and next raffle                       | GSI1, partition `donation#RAFFLES`, ascending by open date                                                                                        |
| 3  | Prizes                                                  | raffle partition, sort keys beginning `PRIZE#`                                                                                                    |
| 4  | Returning supporter by email                            | GSI1, partition `donation#EMAIL#{email}`, limit one                                                                                               |
| 5  | Latest consent, latest Gift Aid                         | entrant partition, `CONSENT#` or `GIFTAID#` prefix, descending, limit one                                                                         |
| 6  | Create an order                                         | put, conditional on the key not existing                                                                                                          |
| 7  | Webhook to order                                        | `metadata.orderId` on the event, else GSI2 partition `donation#PI#{paymentIntentId}`                                                              |
| 8  | Allocate tickets on payment                             | one transaction, described under Flows                                                                                                            |
| 9  | Draw: owner of ticket N                                 | raffle partition, `ENTRY#00000001` to `ENTRY#{N}`, descending, limit one                                                                          |
| 10 | Ledger export for a raffle                              | raffle partition, `ENTRY#` prefix, paginated                                                                                                      |
| 11 | One supporter's orders, tickets, subscriptions and wins | GSI1, partition `donation#ENTRANT#{id}`; one query returns every order, entry, subscription and winner row, or add a `GSI1SK` prefix for one kind |
| 12 | Charge every active subscriber                          | GSI2, partition `donation#SUBS#ACTIVE`, paginated                                                                                                 |
| 13 | Record the draw once                                    | transaction: put `#DRAW` and set `drawnAt`, both conditional on absence                                                                           |
| 14 | Winners list                                            | raffle partition, `WINNER#` prefix                                                                                                                |
| 15 | Order status for the payment page                       | get the order, then GSI1 entrant partition with the `ENTRY#` prefix, matched on order id                                                          |

## Flows

### Single purchase (`api` Lambda)

1. `POST /raffles/{id}/orders` validates the form and caps the donation at £10,000. It finds any returning entrant by
   email and keeps their id, Stripe customer and self-exclusion. It then checks the licence rules: the raffle is open,
   the quantity is between one and the per-order cap, the entrant is 18 or over, resident in Great Britain and not
   self-excluded. Nothing is written before these pass.
2. If the subscribe box is ticked and the entrant has no Stripe customer yet, one is created. The entrant is upserted,
   then a consent row and a Gift Aid row are appended.
3. A Stripe PaymentIntent is created for the total in GBP, card only, with the order, raffle and entrant ids in its
   metadata and the order id as the idempotency key. When subscribing it also carries the customer and asks Stripe to
   save the card for off-session use.
4. The order is put as `PENDING` with the PaymentIntent id, which populates GSI2. The response carries the client secret
   for Stripe's Payment Element.
5. A Stripe Radar rule, `Block if :card_funding: = 'credit'`, refuses credit cards at authorisation.
6. `GET /raffles/current` returns the open raffle, or failing that the most recently finished one, as its full row with
   running totals and prizes, plus the next scheduled raffle. That is exactly what the "raffle closed, next one launches
   on" page needs.

### Stripe webhook

1. The `Stripe-Signature` header is verified: HMAC-SHA256 over the timestamp and payload, with a five-minute tolerance.
   Every handler is idempotent through DynamoDB conditions and Stripe idempotency keys, so a replayed event is harmless.
   A processing error returns 500 so Stripe retries.
2. `charge.succeeded` is used rather than `payment_intent.succeeded` because the charge carries the card's funding type
   and last four digits, so no extra API call is needed. The order is found by the
   `orderId` in the metadata, falling back to GSI2 on the PaymentIntent. A charge with no PaymentIntent is ignored.
3. If the funding type is not `debit`, the charge is refunded with idempotency key `refund_{orderId}`, the order is
   marked `FAILED`, and a subscription order also sets its subscription `PAST_DUE`. This is belt and braces to the Radar
   rule.
4. Otherwise the allocation runs:
    - Read the order with a consistent read. Already `PAID` means "already paid"; any state other than
      `PENDING` is a conflict.
    - Read the raffle with a consistent read. The new range is `ticketsSold + 1` to
      `ticketsSold + quantity`. If that passes `maxTickets` the result is "sold out".
    - One transaction with three items: update the raffle, setting `ticketsSold` to the range end and adding the revenue
      and donation, conditional on `ticketsSold` still holding the value read; put the entry row, conditional on
      absence; update the order to `PAID` with `paidAt`, `cardFunding` and
      `cardLast4`, conditional on it still being `PENDING`.
    - A condition failure on the order item means "already paid". A condition failure on the raffle or entry item means
      a lost race: re-read and retry after a random delay of up to 25 ms times the attempt number, ten attempts in all,
      so a cohort of webhooks that lost the same round does not retry in lockstep. A transaction conflict on any item
      means another transaction was touching it at that instant and is also retried; the re-read then sees either the
      moved counter or the paid order. Anything else is an error, and Stripe retries the webhook.
    - "Sold out" refunds the charge and marks the order `FAILED`. The subscription, if any, stays
      `ACTIVE`.
5. When the order has `subscribe` and the charge carries a customer and payment method, a subscription
   `sub_{entrantId}` is created `ACTIVE`, eligible from the close of the raffle just entered if it is still open,
   otherwise from now. An existing row is left alone, so replays and re-subscriptions after cancellation behave.
6. `payment_intent.payment_failed` moves the order from `PENDING` to `FAILED` without a refund, and sets a subscription
   order's subscription `PAST_DUE`.
7. `charge.refunded` with `refunded` true moves the order from `PAID` to `REFUNDED`. The tickets stay in the ledger as
   void and the draw redraws when it lands on one. Partial refunds are ignored.

### Subscription charge run (hourly)

1. List the raffles and keep those that are open and not yet stamped `subscriptionsChargedAt`.
2. Page through the `SUBS#ACTIVE` index. For each subscription due for the raffle, create the order for that pair with a
   conditional put. If the order already exists, it is charged again only when it is still `PENDING` with no
   PaymentIntent, which means a previous run died between DynamoDB and Stripe; any other state is skipped.
3. Create an off-session PaymentIntent with the customer and saved payment method, confirmed immediately, with the order
   id in the metadata and as the idempotency key. Stripe deduplicates that key for 24 hours. Success or processing
   records the PaymentIntent id on the order, and the webhook flow above allocates the tickets. A card decline marks the
   order `FAILED` and the subscription
   `PAST_DUE`. Any other Stripe or network error is logged, the order is left `PENDING` without a PaymentIntent, and the
   run counts it as errored.
4. The raffle is stamped `subscriptionsChargedAt` only when nothing errored, so the next hourly run picks up the
   stragglers. A run that times out resumes the same way. The Lambda has reserved concurrency of one so two runs never
   overlap.

### Draw

1. An admin invokes the draw with the raffle id, who conducted it and optionally who witnessed it, so the record names
   the responsible person. The raffle must be closed with its draw date reached, have sold tickets, and have prizes
   configured.
2. The draw is claimed atomically: the draw row is put and `drawnAt` set on the raffle, both conditional on absence.
   `ticketsSold` is snapshotted as the draw universe, so allocations that land after this point never enter the draw.
3. Prize tiers expand to one slot per prize in rank order. For each slot a ticket number is drawn uniformly from one to
   `ticketsSold` using operating-system entropy with rejection sampling, so there is no modulo bias. The ticket's entry
   is looked up, and the draw moves on if the ticket has already won or its order is not `PAID`. Up to a thousand
   redraws are allowed per prize. The winner is written conditional on its sequence number.
4. The run is resumable. A raffle already drawn continues from the number of winners recorded so far, using the draw
   record's own `ticketsSold`. A completed draw returns the existing winners unchanged.
5. Winners move from `PENDING` to `NOTIFIED` to `PAID`, or to `UNCLAIMED` after the claim window, by admin action.

### Admin actions (`admin` Lambda, invoked with IAM credentials)

`createRaffle` is conditional on absence. `updateRaffle` takes the same full input, keeps the counters and the created
and drawn timestamps, refuses a price change once tickets are sold, validates the date order, and writes back
conditional on `ticketsSold` so a concurrent sale is never overwritten.
`putPrize` adds or replaces a tier and `removePrize` deletes one by rank; both first read the raffle, so a tier can
never be written into a partition that has no raffle and the advertised prize table cannot change after the draw.
`cancelSubscription` is the phone cancellation. `setWinnerStatus`
moves a winner. `eraseEntrant` refuses while a prize is `PENDING` or `NOTIFIED`, cancels the entrant's subscriptions,
then rewrites the profile.

The same Lambda answers the read actions the admin console is built on: `listRaffles`, `getRaffle` (raffle, prizes,
draw and winners in one call), `listEntries`, `getOrder`, `findTicket`, `findEntrant`, `getEntrant` and
`listSubscriptions`. Each is a fixed number of queries against the access patterns above — the dossier is seven, the
raffle detail four, everything else one to three — and nothing scans. `listEntries` and `listSubscriptions` page with
an opaque cursor, which is the DynamoDB `LastEvaluatedKey` encoded as JSON by `table::page_cursor`: every key
attribute in this table is a string, so the round trip is exact. A cursor carries partition keys such as
`donation#EMAIL#…`, so it is handed back verbatim and never shown, logged or put in a URL.

### Daily reconciliation (`reconcile` Lambda, 06:00 UTC)

For every live raffle, meaning one not yet drawn or drawn within the last 30 days:

- revenue must equal tickets sold times the ticket price, and tickets sold must not exceed the cap;
- the ledger is walked in full for gaps, and its last ticket must not pass `ticketsSold`, nor fall short of it once the
  raffle has closed;
- entries allocated in the last 48 hours must have an order that is `PAID` or `REFUNDED`;
- once the raffle has been open for a day, every due active subscriber must have an order for it carrying a
  PaymentIntent.

Separately, Stripe charges from 48 hours ago until one hour ago are listed. A successful charge carrying an order id
must have an order that is `PAID`, or `FAILED` or `REFUNDED` if the charge was refunded. Every violation is an error log
line and the count is the `IntegrityViolations` metric.

## Compliance

### Gambling Act 2005 and the Gambling Commission's licence conditions

| Obligation                                                       | Structure                                                                                                                                   |
|------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------|
| Under-18s may not play; the operator must verify age             | date of birth on the entrant and the adult check at order time; add an `ageVerifiedAt` column when an electronic check provider is wired in |
| Tickets only sold to people in Great Britain                     | country `GB` and the GB postcode rule, stored per entrant and checked per order                                                             |
| Credit cards banned for gambling since April 2020                | Radar rule, the `cardFunding` audit column, and a refund on any non-debit charge in the webhook                                             |
| Records of tickets sold, price, purchaser and date               | the entry ledger with gapless ranges, the orders, and the raffle's counters                                                                 |
| Proceeds limits of £5 million per lottery and £50 million a year | `maxTickets` enforced in the allocation transaction; the yearly limit is a report over `ticketRevenuePence`                                 |
| The draw must be fair and recorded, with a responsible person    | the draw row with method, conductor, witness and `ticketsSold` at draw time                                                                 |
| Prize records and unclaimed prizes                               | winner rows with status; the table stream keeps each transition's time                                                                      |
| Self-exclusion and social responsibility                         | `selfExcludedUntil` blocks purchases; cancellations are recorded on the subscription                                                        |
| A ticket must state the promoter, licence, price and draw date   | the data is on the raffle; the licence number and responsible person are deployment configuration printed on the receipt                    |
| Returns to the Commission after each lottery                     | totals are on the raffle row; the entry export is the supporting ledger                                                                     |

### HMRC Gift Aid

| Obligation                                                    | Structure                                                                                |
|---------------------------------------------------------------|------------------------------------------------------------------------------------------|
| Raffle tickets are not gifts; only the donation qualifies     | ticket and donation money are separate columns; the eligible amount is the donation only |
| A declaration must carry name, home address, date and wording | a Gift Aid row per checkout with a donor snapshot and a wording version                  |
| Declarations may cover four past years and future gifts       | rows are append-only; a later row with `isUkTaxpayer` false is the withdrawal            |
| Keep records six years after the last claim                   | no expiry on entrant, order or Gift Aid rows; erasure keeps the donor snapshot           |

### UK GDPR and PECR

| Obligation                                                | Structure                                                                                                                                                       |
|-----------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Consent for email marketing must be recorded and provable | consent rows with a wording version and a source                                                                                                                |
| Data minimisation                                         | no card data; only Stripe ids, the last four digits and the funding type                                                                                        |
| Right to erasure against retention obligations            | erasure rewrites the profile with `[erased]`, keeps only the birth year, drops the email index keys, and leaves orders, entries and winners keyed by entrant id |
| Accountability and audit trail                            | the table stream is enabled with old and new images; nothing consumes it yet. The intended consumer is Firehose to S3 with object lock as the immutable log     |
| Access requests                                           | everything for one person is the `ENTRANT#{id}` index partition plus the profile partition                                                                      |

### PCI DSS

Stripe's Payment Element keeps card data off our systems, so the scope is the simplest self-assessment questionnaire.
The table never sees a card number. The last four digits and the funding type are the only card attributes stored.

### Fundraising Regulator code

Opt-in, channel-specific marketing preferences with timestamps, a clear way to change them by writing a new consent row,
and versioned Gift Aid wording.

## Retention

| Data                                        | Retention                                      | Mechanism                               |
|---------------------------------------------|------------------------------------------------|-----------------------------------------|
| orders, entries, raffles, draws and winners | at least six years after the draw              | no expiry; archive job after the window |
| Gift Aid rows                               | six years after the last claim                 | no expiry                               |
| entrant personal data                       | until an erasure request or six years inactive | `eraseEntrant`                          |
| consent rows                                | life of the relationship plus six years        | no expiry                               |

## Lambdas

| Lambda                | Trigger                                                                 | Role                                                       |
|-----------------------|-------------------------------------------------------------------------|------------------------------------------------------------|
| `api`                 | `GET /raffles/current`, `POST /raffles/{id}/orders`, `GET /orders/{id}` | browse, purchase, and the payment page's ticket range      |
| `stripe-webhook`      | Stripe events                                                           | allocation, refunds, subscription creation                 |
| `subscription-charge` | every hour                                                              | off-session charges once a raffle opens                    |
| `draw-run`            | admin invocation                                                        | records the draw and picks winners                         |
| `admin`               | admin invocation                                                        | raffles, prizes, cancellations, winner status, erasure     |
| `reconcile`           | daily at 06:00 UTC                                                      | integrity checks across the ledger, Stripe and subscribers |
| `canary`              | every 5 minutes                                                         | probes `GET /raffles/current`                              |

All seven are thin handlers over the shared crate. Stripe calls go through the `PaymentGateway` trait, so every Lambda
is tested end to end on DynamoDB Local with a scripted gateway. The `aws-cloud` repository deploys the table, the
functions, the site, public URLs for `api` and `stripe-webhook`, the schedules, IAM and the alarms.

## Deliberate simplifications and their ceilings

- **A compare-and-set counter instead of a single-writer queue.** Fine to roughly 50 paid orders per second on one
  raffle. Past that, route payment events through a FIFO queue keyed by raffle so one consumer allocates per raffle, and
  drop the retry loop.
- **The subscription charge run calls Stripe one subscriber at a time inside one Lambda.** Fine to a few thousand
  subscribers per raffle. Because the run is resumable, larger lists finish over several hourly runs. Past that, fan out
  one queue message per subscription and let the consumer create the order and charge.
- **The postcode rule is an area check, not a full validator.** The address-lookup service at checkout does the real
  validation. The rule exists so the server can never accept BT, GY, IM or JE even if the client is bypassed.
- **Refunded tickets stay in the ledger.** The draw skips them. There is no renumbering and no gap.
- **The admin console is authenticated, not authorised.** Any member of the Cognito pool can do anything the
  console can do: erase a supporter, change a prize table, run a draw. There are no roles and no per-action
  audit trail beyond the `sub` each Lambda logs, and a token stays valid until it expires however quickly the
  pool member is removed. Fine while the administrators are one small team who could each be given AWS
  credentials anyway. Past that, put people in `cognito:groups`, check the group in `shared::auth`, and write
  the actor onto the rows they change.
- **The admin console's ledger appends pages rather than windowing them.** Fifty ticket runs a page, and every page
  loaded stays in the DOM, so it is a spot-check for a raffle of a few thousand runs, not an export. A five-million
  ticket raffle would exhaust the browser long before the cursor ran out. The reconciliation Lambda owns real gap
  detection; a genuine export belongs in a job that streams the partition to S3.
- **No Stripe Subscriptions object.** Adopt it if dunning and customer-portal self-service matter more than aligning
  charges to the raffle calendar. The subscription row would gain a Stripe subscription id and the charge trigger would
  become `invoice.paid`.
- **A `PAST_DUE` subscriber has no self-service way back.** Reinstating means a new sign-up purchase with the subscribe
  box ticked, which saves the new card; the existing subscription row is left as it is. Add a SetupIntent flow when
  supporters ask to update a card without buying.
- **The browse canary probes every five minutes, not every minute.** A browse outage is noticed after at least ten
  minutes rather than two, and 288 probes a day instead of 1,440 means a short outage may be sampled once or not at
  all. Two things came free with the old cadence and no longer do: the probe kept one `api` environment warm between
  raffles, and during an outage with no user traffic it alone cleared the `Http5xx` filter's five-in-five-minutes
  threshold. Past that, shorten the cadence again and move `canary_errors`'s `period` with it, or measure browse
  availability from real traffic rather than from the probe.

- **Fixed provisioned capacity of 25 units.** Fine between raffles and for steady selling. A launch spike throttles; the
  SDK's retries absorb short bursts and the throttle alarms page for the rest. Raise `local.free_capacity` by hand for
  the launch window, billed per unit-hour above the free 25, or go back to on-demand and pay per request. Autoscaling
  was tried and removed: every target-tracking policy creates its own CloudWatch alarms, which bill beyond the ten free
  ones.
- **No point-in-time recovery.** DynamoDB's own replication covers hardware loss and deletion protection is on in
  production, but nothing rewinds a bad admin write or an accidental delete. Turn it back on, or add an AWS Backup plan,
  as soon as a few pounds a month matter less than a 35-day rewind.
- **Function URLs instead of API Gateway.** No gateway-level throttling, web application firewall or per-route metrics.
  Lambda reserved concurrency is the only rate limit and 5xx counting comes from a log metric filter. Put API Gateway or
  CloudFront in front when either is needed.
- **Logs are JSON lines and metrics are embedded metric format lines on stdout.** Order and event ids only, no personal
  data. There is no tracing backend; two-hop call chains are followed by id in Logs Insights.
- **Reconciliation checks the ledger tail, not every order.** A paid order without an entry cannot happen, because the
  allocation transaction writes both atomically, and paid is only reachable through a signature-verified
  `charge.succeeded`. So the daily run walks every entry for contiguity but fetches only the orders behind the last 48
  hours of entries and the last 48 hours of Stripe charges. One entrant per email is not checked: it would need a table
  scan, and the fix is a conditional put on the email row.
