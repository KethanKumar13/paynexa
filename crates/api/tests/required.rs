//! The three tests the assignment requires: concurrent payments, idempotent
//! retries without a second PSP call, and PSP failures that must not leave an
//! invoice stuck. PSP call counts come from the mock PSP's `/stats`.

#![expect(
    clippy::unwrap_used,
    reason = "test helpers: a failed setup step should abort the test"
)]

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use api::{
    auth::seed_dev_key,
    config::{PaymentSettings, WebhookSettings},
    routes::router,
    services::{psp_client::PspClient, webhook_delivery::WebhookClient},
    state::AppState,
    workers::sweeper,
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::Barrier;
use tower::ServiceExt;

const TIMEOUT_TOKEN_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone)]
struct Harness {
    state: AppState,
    key: String,
    psp: String,
}

async fn start_psp() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = mock_psp::Config {
        // Long enough for concurrent requests to overlap the PSP call.
        latency: Duration::from_millis(50),
        timeout_token_delay: TIMEOUT_TOKEN_DELAY,
    };
    tokio::spawn(async move { axum::serve(listener, mock_psp::app(config)).await.unwrap() });
    format!("http://{addr}")
}

async fn call(
    state: AppState,
    key: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
    idempotency_key: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json");
    if let Some(k) = idempotency_key {
        req = req.header("idempotency-key", k);
    }
    let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
    let resp = router(state)
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

impl Harness {
    async fn new(db: &PgPool, prefix: &str) -> Self {
        let psp = start_psp().await;
        // The client gives up long before tok_timeout settles.
        let psp_client =
            PspClient::new(&psp, Duration::from_millis(300), Duration::from_millis(300)).unwrap();
        let payments = PaymentSettings {
            sweeper_min_age: Duration::ZERO,
            not_found_grace: Duration::ZERO,
            ..PaymentSettings::default()
        };
        let webhooks = WebhookClient::new(WebhookSettings::default()).unwrap();
        let key = format!("pnx_live_{prefix}_testsecret");
        seed_dev_key(db, &key).await.unwrap();
        let h = Self {
            state: AppState::new(db.clone(), psp_client, payments, webhooks),
            key,
            psp,
        };
        // An endpoint makes every state change write a countable outbox row.
        h.request(
            "POST",
            "/webhook_endpoints",
            Some(json!({"url": "http://receiver.test/hook"})),
            None,
        )
        .await;
        h
    }

    async fn request(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        idempotency_key: Option<&str>,
    ) -> (StatusCode, Value) {
        call(
            self.state.clone(),
            &self.key,
            method,
            uri,
            body,
            idempotency_key,
        )
        .await
    }

    async fn pay(&self, invoice_id: &str, token: &str, key: &str) -> (StatusCode, Value) {
        self.request(
            "POST",
            &format!("/invoices/{invoice_id}/pay"),
            Some(json!({ "card_token": token })),
            Some(key),
        )
        .await
    }

    async fn open_invoice(&self) -> String {
        let (_, customer) = self
            .request(
                "POST",
                "/customers",
                Some(json!({"name": "Ada", "email": "ada@example.com"})),
                None,
            )
            .await;
        let (status, invoice) = self
            .request(
                "POST",
                "/invoices",
                Some(json!({
                    "customer_id": customer["id"],
                    "due_date": "2026-12-31",
                    "line_items": [{"description": "Plan", "quantity": 1, "unit_amount_cents": 4200}]
                })),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{invoice}");
        invoice["id"].as_str().unwrap().to_string()
    }

    async fn attempt(&self, attempt_id: &str) -> Value {
        self.request(
            "GET",
            &format!("/payment_attempts/{attempt_id}"),
            None,
            None,
        )
        .await
        .1
    }

    async fn psp_stats(&self) -> Value {
        reqwest::get(format!("{}/stats", self.psp))
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
}

async fn count(db: &PgPool, sql: &str, invoice_id: &str) -> i64 {
    sqlx::query_scalar(sql)
        .bind(invoice_id)
        .fetch_one(db)
        .await
        .unwrap()
}

/// Requirement 1: N concurrent `POST /pay` on one invoice — at most one succeeds,
/// the PSP is charged once, and the final state is consistent.
#[sqlx::test(migrations = "../../migrations")]
async fn concurrent_payments_charge_at_most_once(db: PgPool) {
    const N: usize = 10;
    let h = Harness::new(&db, "reqconc1").await;
    let invoice = h.open_invoice().await;

    let barrier = Arc::new(Barrier::new(N));
    let mut tasks = Vec::with_capacity(N);
    for n in 0..N {
        let (h, invoice, barrier) = (h.clone(), invoice.clone(), barrier.clone());
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            h.pay(&invoice, "tok_success", &format!("race-{n}")).await
        }));
    }
    let mut responses = Vec::with_capacity(N);
    for task in tasks {
        responses.push(task.await.unwrap());
    }

    let successes: Vec<&Value> = responses
        .iter()
        .filter(|(status, _)| *status == StatusCode::OK)
        .map(|(_, body)| body)
        .collect();
    assert_eq!(successes.len(), 1, "{responses:?}");
    assert_eq!(successes[0]["status"], "succeeded");
    for (status, body) in responses.iter().filter(|(s, _)| *s != StatusCode::OK) {
        assert_eq!(*status, StatusCode::CONFLICT, "{body}");
        let code = body["error"]["code"].as_str().unwrap();
        assert!(
            code == "payment_in_progress" || code == "invalid_state_transition",
            "{body}"
        );
    }

    let attempts = count(
        &db,
        "SELECT count(*) FROM payment_attempts WHERE invoice_id = $1::uuid",
        &invoice,
    )
    .await;
    let paid_events = count(
        &db,
        "SELECT count(*) FROM webhook_events
         WHERE event_type = 'invoice.paid' AND payload->'data'->'invoice'->>'id' = $1",
        &invoice,
    )
    .await;
    assert_eq!((attempts, paid_events), (1, 1));
    let (_, invoice_now) = h
        .request("GET", &format!("/invoices/{invoice}"), None, None)
        .await;
    assert_eq!(invoice_now["status"], "paid");

    let stats = h.psp_stats().await;
    assert_eq!(stats["total_charge_requests"], 1, "{stats}");
    assert_eq!(stats["charges"].as_object().unwrap().len(), 1, "{stats}");
}

/// Requirement 2: retrying with the same Idempotency-Key returns the same
/// response and never reaches the PSP a second time.
#[sqlx::test(migrations = "../../migrations")]
async fn idempotent_retry_returns_same_response_without_second_psp_call(db: PgPool) {
    let h = Harness::new(&db, "reqidem1").await;
    let invoice = h.open_invoice().await;

    let first = h.pay(&invoice, "tok_success", "retry-me").await;
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    for _ in 0..2 {
        assert_eq!(h.pay(&invoice, "tok_success", "retry-me").await, first);
    }

    let attempt_id = first.1["id"].as_str().unwrap();
    let stats = h.psp_stats().await;
    assert_eq!(stats["charges"][attempt_id]["calls"], 1, "{stats}");
    assert_eq!(stats["total_charge_requests"], 1, "{stats}");
    let attempts = count(
        &db,
        "SELECT count(*) FROM payment_attempts WHERE invoice_id = $1::uuid",
        &invoice,
    )
    .await;
    assert_eq!(attempts, 1);
}

/// Requirement 3: `tok_network_error` and `tok_timeout` return promptly and the
/// invoice always reaches a valid state without a second charge.
#[sqlx::test(migrations = "../../migrations")]
async fn psp_failures_do_not_leave_invoice_stuck(db: PgPool) {
    let h = Harness::new(&db, "reqfail1").await;

    // tok_network_error: ambiguous 500, reconciled to a definitive failure.
    let invoice = h.open_invoice().await;
    let (status, attempt) = h.pay(&invoice, "tok_network_error", "neterr-1").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{attempt}");
    assert_eq!(
        (
            attempt["status"].as_str(),
            attempt["invoice_status"].as_str()
        ),
        (Some("unknown"), Some("processing"))
    );
    sweeper::run_once(&h.state).await.unwrap();
    let settled = h.attempt(attempt["id"].as_str().unwrap()).await;
    assert_eq!(settled["status"], "failed");
    assert_eq!(settled["failure_code"], "processing_error");
    assert_eq!(settled["invoice_status"], "open");
    let (status, retry) = h.pay(&invoice, "tok_success", "neterr-2").await;
    assert_eq!(
        (status, retry["invoice_status"].as_str()),
        (StatusCode::OK, Some("paid")),
        "invoice must be payable again"
    );

    // tok_timeout: the endpoint must not hang; the late success is reconciled once.
    let invoice = h.open_invoice().await;
    let started = Instant::now();
    let (status, attempt) = h.pay(&invoice, "tok_timeout", "timeout-1").await;
    assert!(started.elapsed() < Duration::from_secs(2), "endpoint hung");
    assert_eq!(status, StatusCode::ACCEPTED, "{attempt}");
    let attempt_id = attempt["id"].as_str().unwrap();

    sweeper::run_once(&h.state).await.unwrap();
    assert_eq!(
        h.attempt(attempt_id).await["invoice_status"],
        "processing",
        "still pending at the PSP, so nothing may be decided yet"
    );

    tokio::time::sleep(TIMEOUT_TOKEN_DELAY + Duration::from_millis(300)).await;
    sweeper::run_once(&h.state).await.unwrap();
    let settled = h.attempt(attempt_id).await;
    assert_eq!(settled["status"], "succeeded");
    assert_eq!(settled["invoice_status"], "paid");
    assert_eq!(h.psp_stats().await["charges"][attempt_id]["calls"], 1);
}
