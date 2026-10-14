use std::time::Duration;

use api::{
    auth::seed_dev_key, config::PaymentSettings, routes::router, services::psp_client::PspClient,
    state::AppState, workers::sweeper,
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

struct Harness {
    state: AppState,
    key: String,
}

async fn start_psp() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = mock_psp::Config {
        latency: Duration::from_millis(10),
        timeout_token_delay: Duration::from_millis(1500),
    };
    tokio::spawn(async move { axum::serve(listener, mock_psp::app(config)).await.unwrap() });
    format!("http://{addr}")
}

impl Harness {
    async fn new(db: &PgPool, prefix: &str) -> Self {
        let psp = PspClient::new(
            &start_psp().await,
            Duration::from_millis(500),
            Duration::from_millis(500),
        )
        .unwrap();
        // Zero ages let a test drive the sweeper immediately.
        let settings = PaymentSettings {
            sweeper_min_age: Duration::ZERO,
            not_found_grace: Duration::ZERO,
            ..PaymentSettings::default()
        };
        let key = format!("pnx_live_{prefix}_testsecret");
        seed_dev_key(db, &key).await.unwrap();
        Self {
            state: AppState::new(db.clone(), psp, settings),
            key,
        }
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        idempotency_key: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {}", self.key))
            .header("content-type", "application/json");
        if let Some(k) = idempotency_key {
            req = req.header("idempotency-key", k);
        }
        let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
        let resp = router(self.state.clone())
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

    async fn pay(&self, invoice_id: &str, token: &str, key: &str) -> (StatusCode, Value) {
        self.call(
            "POST",
            &format!("/invoices/{invoice_id}/pay"),
            Some(json!({ "card_token": token })),
            Some(key),
        )
        .await
    }

    async fn open_invoice(&self) -> String {
        let (_, customer) = self
            .call(
                "POST",
                "/customers",
                Some(json!({"name": "Ada", "email": "ada@example.com"})),
                None,
            )
            .await;
        let (status, invoice) = self
            .call(
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

    async fn invoice_status(&self, invoice_id: &str) -> String {
        let (_, invoice) = self
            .call("GET", &format!("/invoices/{invoice_id}"), None, None)
            .await;
        invoice["status"].as_str().unwrap().to_string()
    }
}

async fn event_types(db: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT event_type FROM webhook_events ORDER BY created_at, event_type")
        .fetch_all(db)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn successful_payment_marks_invoice_paid(db: PgPool) {
    let h = Harness::new(&db, "paysucc1").await;
    h.call(
        "POST",
        "/webhook_endpoints",
        Some(json!({"url": "http://receiver.test/hook"})),
        None,
    )
    .await;
    let invoice = h.open_invoice().await;

    let (status, attempt) = h.pay(&invoice, "tok_success", "k1").await;
    assert_eq!(status, StatusCode::OK, "{attempt}");
    assert_eq!(attempt["status"], "succeeded");
    assert_eq!(attempt["invoice_status"], "paid");
    assert_eq!(attempt["amount_cents"], 4200);
    assert!(attempt["psp_reference"].is_string());
    assert_eq!(h.invoice_status(&invoice).await, "paid");
    assert_eq!(event_types(&db).await, ["invoice.created", "invoice.paid"]);

    let (status, body) = h.pay(&invoice, "tok_success", "k2").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "invalid_state_transition");
    assert_eq!(body["error"]["details"]["current_state"], "paid");
}

#[sqlx::test(migrations = "../../migrations")]
async fn decline_reopens_invoice_for_another_card(db: PgPool) {
    let h = Harness::new(&db, "paydecl1").await;
    h.call(
        "POST",
        "/webhook_endpoints",
        Some(json!({"url": "http://receiver.test/hook"})),
        None,
    )
    .await;
    let invoice = h.open_invoice().await;

    let (status, attempt) = h.pay(&invoice, "tok_card_declined", "k1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(attempt["status"], "failed");
    assert_eq!(attempt["failure_code"], "card_declined");
    assert_eq!(attempt["invoice_status"], "open");
    assert!(
        event_types(&db)
            .await
            .contains(&"invoice.payment_failed".to_string())
    );

    let (status, attempt) = h.pay(&invoice, "tok_success", "k2").await;
    assert_eq!(
        (status, attempt["invoice_status"].as_str()),
        (StatusCode::OK, Some("paid"))
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn idempotency_key_rules(db: PgPool) {
    let h = Harness::new(&db, "payidem1").await;
    let invoice = h.open_invoice().await;

    let first = h.pay(&invoice, "tok_success", "same-key").await;
    let replay = h.pay(&invoice, "tok_success", "same-key").await;
    assert_eq!(first, replay, "same key + same body replays the response");

    let (status, body) = h.pay(&invoice, "tok_card_declined", "same-key").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "idempotency_conflict");

    let (status, body) = h
        .call(
            "POST",
            &format!("/invoices/{invoice}/pay"),
            Some(json!({"card_token": "tok_success"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn attempts_are_tenant_scoped(db: PgPool) {
    let a = Harness::new(&db, "paytena1").await;
    let b = Harness::new(&db, "paytenb1").await;
    let invoice = a.open_invoice().await;
    let (_, attempt) = a.pay(&invoice, "tok_success", "k1").await;
    let attempt_id = attempt["id"].as_str().unwrap();

    let (status, _) = b
        .call(
            "GET",
            &format!("/payment_attempts/{attempt_id}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = b.pay(&invoice, "tok_success", "k1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = a
        .call(
            "GET",
            &format!("/payment_attempts/{attempt_id}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "../../migrations")]
async fn sweeper_reconciles_a_lost_response_without_recharging(db: PgPool) {
    let h = Harness::new(&db, "paydrop1").await;
    let invoice = h.open_invoice().await;

    let (status, attempt) = h.pay(&invoice, "tok_network_drop", "k1").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{attempt}");
    assert_eq!(attempt["status"], "unknown");
    assert_eq!(attempt["invoice_status"], "processing");

    let (status, body) = h.pay(&invoice, "tok_success", "k2").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "payment_in_progress");

    assert_eq!(sweeper::run_once(&h.state).await.unwrap(), 1);
    let attempt_id = attempt["id"].as_str().unwrap();
    let (_, settled) = h
        .call(
            "GET",
            &format!("/payment_attempts/{attempt_id}"),
            None,
            None,
        )
        .await;
    assert_eq!(settled["status"], "succeeded");
    assert_eq!(settled["invoice_status"], "paid");

    let (status, replayed) = h.pay(&invoice, "tok_network_drop", "k1").await;
    assert_eq!((status, &replayed), (StatusCode::OK, &settled));
    assert_eq!(sweeper::run_once(&h.state).await.unwrap(), 0);
}
