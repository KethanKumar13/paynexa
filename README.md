# Paynexa - Invoice & Payment Service

A small invoicing and payments backend in Rust. A business creates customers and invoices, customers pay invoices through a payment service provider (PSP), and the business is told about state changes through signed webhooks.

The interesting parts are the invoice state machine, payment correctness under concurrency, retries and PSP failures, and webhook delivery. They are explained in [DESIGN.md](DESIGN.md).

```mermaid
flowchart LR
    client[Business client] -->|Bearer API key| api[api :8088]
    api --> db[(PostgreSQL)]
    api -->|POST /charges, 5 s timeout| psp[mock PSP :8089]
    sweeper[sweeper, in api] -->|GET /charges/id lookup only| psp
    sweeper --> db
    dispatcher[webhook dispatcher, in api] --> db
    dispatcher -->|signed POST| receiver[webhook receiver, e.g. psp /sink]
```

| Part | What it is |
|---|---|
| `crates/api` | Axum HTTP API, plus two background workers: the payment **sweeper** (reconciles uncertain PSP outcomes) and the **webhook dispatcher** |
| `crates/mock-psp` | Separate binary that simulates the PSP from the card token, plus a demo webhook receiver at `/sink` |
| `migrations/` | SQL migrations, applied automatically on API startup |

## Quick start

Requirements: Docker with Compose v2. Nothing else; no `.env` file is needed.

```bash
docker compose up --build
```

The first build compiles Rust and takes a few minutes. Then:

```bash
curl http://127.0.0.1:8088/healthz        # {"status":"ok"}
```

| Service | Host address | Notes |
|---|---|---|
| API | `127.0.0.1:8088` | |
| Mock PSP | `127.0.0.1:8089` | Inside the compose network it is `http://psp:8081` |
| PostgreSQL | `127.0.0.1:5544` | user, password and database are all `paynexa` |

All ports are bound to `127.0.0.1` only, and use non-default host ports so they don't clash with other local databases or services.

### Dev API key (DEV ONLY)

On startup the API seeds one business and this key, so the examples work immediately:

```
pnx_live_devkey01_DEVONLY_local_demo_key_not_a_secret
```

It is public and exists only because `docker-compose.yml` sets `DEV_API_KEY` by default. **Never deploy with it.** Unset `DEV_API_KEY` outside local development. To change it, also change the 8-character prefix (`devkey01`); a new key with an existing prefix is not re-seeded and gets `401`.

## curl examples

```bash
API=http://127.0.0.1:8088
AUTH="Authorization: Bearer pnx_live_devkey01_DEVONLY_local_demo_key_not_a_secret"
```

**1. Create a customer**

```bash
curl -s -X POST $API/customers -H "$AUTH" -H "Content-Type: application/json" \
  -d '{"name":"Ada Lovelace","email":"ada@example.com"}'
# 201 {"id":"<customer_id>","name":"Ada Lovelace","email":"ada@example.com","created_at":"..."}
```

**2. Create an invoice** (the server computes every total; sending `total_cents` is rejected with `422`)

```bash
curl -s -X POST $API/invoices -H "$AUTH" -H "Content-Type: application/json" \
  -d '{"customer_id":"<customer_id>","due_date":"2026-12-31",
       "line_items":[{"description":"Consulting","quantity":2,"unit_amount_cents":1500},
                     {"description":"Hosting","quantity":1,"unit_amount_cents":99}]}'
# 201 {"id":"<invoice_id>","status":"open","total_cents":3099,"currency":"USD","line_items":[...],...}
```

**3. Pay it successfully**

```bash
curl -s -X POST $API/invoices/<invoice_id>/pay -H "$AUTH" -H "Content-Type: application/json" \
  -H "Idempotency-Key: pay-<invoice_id>-1" -d '{"card_token":"tok_success"}'
# 200 {"id":"<attempt_id>","status":"succeeded","psp_reference":"...","invoice_status":"paid",...}
```

Sending the same request again with the same `Idempotency-Key` returns the identical response without charging again.

**4. A failing payment** (on a second invoice)

```bash
curl -s -X POST $API/invoices/<invoice_id_2>/pay -H "$AUTH" -H "Content-Type: application/json" \
  -H "Idempotency-Key: pay-<invoice_id_2>-1" -d '{"card_token":"tok_card_declined"}'
# 200 {"status":"failed","failure_code":"card_declined","invoice_status":"open",...}
```

A decline is a valid outcome, so it is `200`; the invoice goes back to `open` and can be paid with another card and a new key.

**5. A slow PSP** (optional)

```bash
curl -s -X POST $API/invoices/<invoice_id_3>/pay -H "$AUTH" -H "Content-Type: application/json" \
  -H "Idempotency-Key: pay-<invoice_id_3>-1" -d '{"card_token":"tok_timeout"}'
# after ~5 s: 202 {"id":"<attempt_id>","status":"unknown","invoice_status":"processing",...}

curl -s $API/payment_attempts/<attempt_id> -H "$AUTH"
# within ~45 s: {"status":"succeeded","invoice_status":"paid",...}
```

<details>
<summary>The same examples in Windows PowerShell 5.1</summary>

PowerShell 5.1 strips the quotes from JSON passed to `curl.exe -d '...'`, so use `Invoke-RestMethod`:

```powershell
$api = "http://127.0.0.1:8088"
$h = @{ Authorization = "Bearer pnx_live_devkey01_DEVONLY_local_demo_key_not_a_secret" }

$c = Invoke-RestMethod -Method Post -Uri "$api/customers" -Headers $h -ContentType "application/json" `
  -Body '{"name":"Ada Lovelace","email":"ada@example.com"}'

$inv = Invoke-RestMethod -Method Post -Uri "$api/invoices" -Headers $h -ContentType "application/json" `
  -Body (@{ customer_id = $c.id; due_date = "2026-12-31"; line_items = @(
      @{ description = "Consulting"; quantity = 2; unit_amount_cents = 1500 },
      @{ description = "Hosting"; quantity = 1; unit_amount_cents = 99 }) } | ConvertTo-Json -Depth 4)

Invoke-RestMethod -Method Post -Uri "$api/invoices/$($inv.id)/pay" -ContentType "application/json" `
  -Headers ($h + @{ "Idempotency-Key" = "pay-$($inv.id)-1" }) -Body '{"card_token":"tok_success"}'
```

`Invoke-RestMethod` throws on `4xx`/`5xx`; the error body is in `$_.ErrorDetails.Message`.
</details>

The full API, including every error code, is in [openapi.yaml](openapi.yaml).

## Webhooks demo

Every state change (`invoice.created`, `invoice.paid`, `invoice.payment_failed`) is written to an outbox table in the same transaction and delivered by a background dispatcher, so the API response never waits for it. The mock PSP includes a receiver that verifies signatures.

```bash
# 1. Register the receiver. Use its address inside the compose network.
curl -s -X POST $API/webhook_endpoints -H "$AUTH" -H "Content-Type: application/json" \
  -d '{"url":"http://psp:8081/sink"}'
# 201 {..., "signing_secret":"whsec_..."}   <- shown only once

# 2. Give the receiver that secret so it can verify signatures.
curl -s -X PUT http://127.0.0.1:8089/sink/secret -H "Content-Type: application/json" \
  -d '{"secret":"whsec_..."}'

# 3. Create and pay invoices (examples above), then look at what arrived:
curl -s http://127.0.0.1:8089/sink/events
docker compose logs psp | grep "webhook received"     # signature="valid"
curl -s "$API/webhook_events?status=delivered" -H "$AUTH"
```

The sink remembers only the last secret it was given. If you run the demo again without `docker compose down -v`, the endpoint from the earlier run is still registered with its old secret, so its deliveries show `signature_valid: false`.

To see retries, also register `http://psp:8081/sink/fail`, which always answers `503`, then check `GET /webhook_events?status=pending`: `attempt_count`, `last_error` and `next_attempt_at` grow along the backoff schedule (1 m, 5 m, 15 m, 1 h, 6 h, then `failed`). To watch it give up quickly, put `WEBHOOK_RETRY_SCHEDULE_MS=2000,2000` in a `.env` file and run `docker compose up -d` again.

**Verifying a delivery as a receiver**

1. Read `Paynexa-Signature: t=<unix>,v1=<hex>`.
2. Compute `HMAC-SHA256(secret, "<t>." + raw request body)` and hex-encode it.
3. Compare it with `v1` in constant time, and reject if `t` is more than 5 minutes old.
4. Deduplicate on `Paynexa-Event-Id`: delivery is at-least-once, and events can arrive out of order. `GET /invoices/{id}` is the source of truth.

[crates/mock-psp/src/sink.rs](crates/mock-psp/src/sink.rs) is a working example of these steps.

## Mock PSP

The outcome is chosen by `card_token`. Charges are idempotent per `Idempotency-Key` (the API uses the payment attempt ID) and are kept in memory, so they are lost when the PSP container restarts.

| Token | Behaviour | What the API does |
|---|---|---|
| `tok_success` | `succeeded` after ~100 ms | attempt `succeeded`, invoice `paid` |
| `tok_insufficient_funds` | `failed` / `insufficient_funds` after ~100 ms | attempt `failed`, invoice back to `open` |
| `tok_card_declined` | `failed` / `card_declined` after ~100 ms | attempt `failed`, invoice back to `open` |
| `tok_timeout` | succeeds after 30 s | `202` after 5 s (`unknown`); the sweeper later finds `succeeded` |
| `tok_network_error` | HTTP `500`, no charge | `202` (`unknown`); the sweeper later finds `failed` / `processing_error`, invoice `open` |
| `tok_network_drop` | charges, then drops the connection | `202` (`unknown`); the sweeper later finds `succeeded`, so no second charge |
| anything else | `failed` / `card_declined` | as a decline |

Other endpoints: `GET /charges/{key}` (lookup), `GET /stats` (calls per key, used by the tests), `/sink`, `/sink/secret`, `/sink/events`, `/sink/fail`.

## Configuration

Every variable has a default in `docker-compose.yml`; override any of them in a `.env` file. [.env.example](.env.example) lists them all.

| Variable | Default | Meaning |
|---|---|---|
| `DEV_API_KEY` | the demo key above | seeds a demo business on startup; unset it outside development |
| `PSP_CONNECT_TIMEOUT_MS` / `PSP_TIMEOUT_MS` | `2000` / `5000` | API → PSP timeouts |
| `SWEEPER_INTERVAL_MS` | `10000` | how often uncertain attempts are reconciled |
| `SWEEPER_MIN_AGE_MS` | `15000` | attempts younger than this are left to their request |
| `PSP_NOT_FOUND_GRACE_MS` | `60000` | a PSP `404` only means "never received" after this age |
| `SWEEPER_ALERT_AGE_MS` | `600000` | still unresolved after this → error log |
| `WEBHOOK_POLL_INTERVAL_MS` | `1000` | dispatcher poll interval |
| `WEBHOOK_TIMEOUT_MS` | `5000` | per-delivery timeout |
| `WEBHOOK_LEASE_MS` | `30000` | how long a claimed event is hidden from other runs; must be greater than the timeout |
| `WEBHOOK_RETRY_SCHEDULE_MS` | `60000,300000,900000,3600000,21600000` | delay before each retry; attempts = retries + 1 |
| `PSP_LATENCY_MS` / `PSP_TIMEOUT_TOKEN_MS` | `100` / `30000` | mock PSP delays |
| `DB_HOST_PORT` / `API_HOST_PORT` / `PSP_HOST_PORT` | `5544` / `8088` / `8089` | host ports |

## Testing

The integration tests need PostgreSQL; the one from `docker compose up` works. Each test creates and drops its own database.

```bash
export DATABASE_URL=postgres://paynexa:paynexa@127.0.0.1:5544/paynexa   # PowerShell: $env:DATABASE_URL = "..."
cargo test --workspace            # all 38 tests
cargo test -p api --test required # only the three required tests
```

Use `127.0.0.1`, not `localhost`: on Windows `localhost` tries IPv6 first, which is not bound, and the suite slows from about 5 s to several minutes.

The three tests the brief requires are in [crates/api/tests/required.rs](crates/api/tests/required.rs). Each one checks the PSP's own call counts, not just our database:

| Requirement | Test | What it asserts |
|---|---|---|
| Concurrent payments | `concurrent_payments_charge_at_most_once` | 10 concurrent `/pay` calls with different keys: exactly one `200`, the rest `409`; one attempt, one PSP charge, invoice `paid`, one `invoice.paid` event |
| Idempotency | `idempotent_retry_returns_same_response_without_second_psp_call` | two retries with the same key return the identical response; the PSP saw exactly one call |
| PSP failure | `psp_failures_do_not_leave_invoice_stuck` | `tok_network_error` → `202`, then `failed` and invoice `open`, payable again; `tok_timeout` → `202` in under 2 s, then `paid` with one PSP call |

None of the required tests is skipped.

## Development

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Lint policy: no `unsafe`, no `unwrap()` outside tests and no `as` numeric casts (`[workspace.lints]` in [Cargo.toml](Cargo.toml)). Suppressions use `#[expect(lint, reason = "...")]`.

## Isolation from other Docker projects

The compose project is named `paynexa`, so every container, network and volume is prefixed `paynexa_`, and nothing joins another project's network. `docker compose down -v` removes only this project's resources, including its database volume.

## Documents

- [DESIGN.md](DESIGN.md): data model, state machine, payment failure modes, webhooks, API keys, what was cut, production gaps
- [openapi.yaml](openapi.yaml): API reference
- [AI_USAGE.md](AI_USAGE.md): how AI tools were used

## Demo Video

_Link to be added._
