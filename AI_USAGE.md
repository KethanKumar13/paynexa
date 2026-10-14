# AI Usage

## Tools and what I used them for

- **Claude (chat):** first read of the assignment. I used it to break the brief into required features, the invoice-vs-attempt split, and the failure modes I would need to answer, and to sketch an initial stage plan.
- **GitHub Copilot Chat in VS Code, Plan mode:** a written plan for each stage before any code (schema, pay flow, sweeper, webhook dispatcher, tests, docs). I reviewed every plan and sent back changes before approving it.
- **GitHub Copilot Chat in VS Code, Agent mode:** implemented the approved plans: Rust code, SQL migration, tests, Docker files and these documents. It also ran `cargo clippy`, the tests and Docker checks after each stage.
- **Me:** reviewed each stage's diff, ran the manual walkthroughs against `docker compose up`, and asked for corrections. Several design choices below came out of those reviews, not from the AI.

How I checked the output: 38 automated tests, including the three required ones, which assert on the mock PSP's own call counts; clippy with `-D warnings`; manual runs of every token and the webhook demo in Docker; and reading DESIGN.md against the code line by line.

## Three decisions I made

1. **A PSP `404` only counts as "never received" after 60 s.** The first design marked an attempt `failed` as soon as the PSP lookup returned `404`. I pointed out that a delayed request could still reach the PSP after that, by which point the invoice would be `open` again and a second attempt could charge the customer twice. I chose a grace period well past the 5 s client timeout, and listed the remaining risk (a request delayed beyond 60 s) as a production gap.

2. **Clients see `unknown`, not `pending`.** The first design returned `status: "pending"` on a `202` while the database held `unknown`. I chose to expose `unknown` as-is: it tells the caller the outcome is unconfirmed and that they must not retry with a new key, and the API never reports a state that differs from what is stored.

3. **One crate with module layering, and only three targeted lints.** I considered splitting the code into separate `api`, `domain`, `db` and `shared` crates, and turning on `clippy::pedantic`. I chose a single `api` crate (`domain`, `services`, `routes`, `workers` modules) and only `unsafe_code = "forbid"`, `clippy::unwrap_used` and `clippy::as_conversions`. The brief values restraint and doesn't score abstractions, and those three lints target the risks that matter here: panics on the request path and silent numeric truncation near money.

## What the AI got wrong

1. **It held a database lock across a network call.** The first design had the reconciliation sweeper keep `FOR UPDATE SKIP LOCKED` row locks while it called the PSP, and planned the same for webhook delivery. That is the exact mistake the pay flow was built to avoid: a slow PSP or receiver would hold locks for seconds. I changed it to a lookup with no lock, relying on a status-conditional `finalize()`, and a short claim-and-lease for webhooks.

2. **It credited the wrong mechanism for "no double charge".** The first answer to failure mode (c) presented the PSP's idempotency key as the protection. The real guarantee is on our side: the invoice stays `processing` so no second attempt can be created, and the sweeper only looks charges up and never re-sends. PSP deduplication is a backstop, and DESIGN.md now says so.

3. **It left idempotency on rejected requests unspecified.** The design didn't say what happens to an `Idempotency-Key` whose request is rejected with `409`. I made it explicit: the losing request's transaction rolls back, including its idempotency record, so `409`s are never stored and the same key can be used again later.
