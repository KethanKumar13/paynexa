#![expect(
    clippy::unwrap_used,
    reason = "test helpers: a failed setup step should abort the test"
)]

use std::time::Duration;

use api::{
    auth::seed_dev_key,
    config::{PaymentSettings, WebhookSettings},
    routes::router,
    services::{psp_client::PspClient, webhook_delivery::WebhookClient},
    state::AppState,
    workers::webhook_dispatcher,
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
    psp: String,
}

async fn start_psp() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, mock_psp::app(mock_psp::Config::default()))
            .await
            .unwrap()
    });
    format!("http://{addr}")
}

impl Harness {
    async fn new(db: &PgPool, prefix: &str) -> Self {
        let psp = start_psp().await;
        let psp_client =
            PspClient::new(&psp, Duration::from_millis(500), Duration::from_millis(500)).unwrap();
        // Two immediate retries keep the give-up path fast.
        let webhooks = WebhookClient::new(WebhookSettings {
            timeout: Duration::from_secs(2),
            retry_schedule: vec![Duration::ZERO, Duration::ZERO],
            ..WebhookSettings::default()
        })
        .unwrap();
        let key = format!("pnx_live_{prefix}_testsecret");
        seed_dev_key(db, &key).await.unwrap();
        Self {
            state: AppState::new(db.clone(), psp_client, PaymentSettings::default(), webhooks),
            key,
            psp,
        }
    }

    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {}", self.key))
            .header("content-type", "application/json");
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

    async fn register_endpoint(&self, path: &str) -> String {
        let (status, endpoint) = self
            .call(
                "POST",
                "/webhook_endpoints",
                Some(json!({ "url": format!("{}{path}", self.psp) })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{endpoint}");
        endpoint["signing_secret"].as_str().unwrap().to_string()
    }

    async fn create_invoice(&self) -> String {
        let (_, customer) = self
            .call(
                "POST",
                "/customers",
                Some(json!({"name": "Ada", "email": "ada@example.com"})),
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
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{invoice}");
        invoice["id"].as_str().unwrap().to_string()
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn events_are_delivered_signed_and_verified_by_the_receiver(db: PgPool) {
    let h = Harness::new(&db, "whdeliv1").await;
    let secret = h.register_endpoint("/sink").await;
    let resp = reqwest::Client::new()
        .put(format!("{}/sink/secret", h.psp))
        .json(&json!({ "secret": secret }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);
    h.create_invoice().await;

    assert_eq!(webhook_dispatcher::run_once(&h.state).await.unwrap(), 1);

    let received: Value = reqwest::get(format!("{}/sink/events", h.psp))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let (_, delivered) = h
        .call("GET", "/webhook_events?status=delivered", None)
        .await;
    let event = &delivered["data"][0];
    assert_eq!(event["event_type"], "invoice.created");
    assert_eq!(event["attempt_count"], 1);
    assert_eq!(received[0]["event_type"], "invoice.created");
    assert_eq!(received[0]["event_id"], event["id"]);
    assert_eq!(received[0]["signature_valid"], true);

    assert_eq!(
        webhook_dispatcher::run_once(&h.state).await.unwrap(),
        0,
        "delivered events are not sent again"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn failing_receiver_is_retried_then_marked_failed(db: PgPool) {
    let h = Harness::new(&db, "whfail01").await;
    h.register_endpoint("/sink/fail").await;
    h.create_invoice().await;

    for expected_attempts in 1..=2 {
        assert_eq!(webhook_dispatcher::run_once(&h.state).await.unwrap(), 0);
        let (_, pending) = h.call("GET", "/webhook_events?status=pending", None).await;
        assert_eq!(pending["data"][0]["attempt_count"], expected_attempts);
    }
    webhook_dispatcher::run_once(&h.state).await.unwrap();

    let (_, failed) = h.call("GET", "/webhook_events?status=failed", None).await;
    let event = &failed["data"][0];
    assert_eq!(event["attempt_count"], 3);
    assert!(
        event["last_error"].as_str().unwrap().contains("503"),
        "{event}"
    );
    assert_eq!(webhook_dispatcher::run_once(&h.state).await.unwrap(), 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn event_listing_is_tenant_scoped_and_validated(db: PgPool) {
    let a = Harness::new(&db, "whtena01").await;
    let b = Harness::new(&db, "whtenb01").await;
    a.register_endpoint("/sink").await;
    a.create_invoice().await;

    let (_, mine) = a.call("GET", "/webhook_events", None).await;
    assert_eq!(mine["data"].as_array().unwrap().len(), 1);
    let (_, theirs) = b.call("GET", "/webhook_events", None).await;
    assert_eq!(theirs["data"], json!([]));
    let (status, _) = a.call("GET", "/webhook_events?status=bogus", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}
