use api::{auth::seed_dev_key, routes::router, state::AppState};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

async fn business(db: &PgPool, prefix: &str) -> String {
    let key = format!("pnx_live_{prefix}_testsecret");
    seed_dev_key(db, &key).await.unwrap();
    key
}

async fn call(db: &PgPool, key: &str, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json");
    let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
    let resp = router(AppState { db: db.clone() })
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
    (status, json)
}

async fn create_customer(db: &PgPool, key: &str) -> String {
    let (status, body) = call(db, key, "POST", "/customers", Some(json!({"name": "Ada", "email": "ada@example.com"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_string()
}

async fn create_invoice(db: &PgPool, key: &str, customer_id: &str) -> Value {
    let (status, body) = call(
        db,
        key,
        "POST",
        "/invoices",
        Some(json!({
            "customer_id": customer_id,
            "due_date": "2026-12-31",
            "line_items": [
                {"description": "Consulting", "quantity": 2, "unit_amount_cents": 1500},
                {"description": "Hosting", "quantity": 1, "unit_amount_cents": 99}
            ]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

#[sqlx::test(migrations = "../../migrations")]
async fn tenants_cannot_see_or_use_each_others_data(db: PgPool) {
    let a = business(&db, "tenanta1").await;
    let b = business(&db, "tenantb1").await;
    let customer = create_customer(&db, &a).await;
    let invoice = create_invoice(&db, &a, &customer).await;
    let invoice_id = invoice["id"].as_str().unwrap();

    assert_eq!(call(&db, &b, "GET", &format!("/customers/{customer}"), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&db, &b, "GET", &format!("/invoices/{invoice_id}"), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&db, &b, "POST", &format!("/invoices/{invoice_id}/void"), None).await.0, StatusCode::NOT_FOUND);

    let (status, body) = call(
        &db,
        &b,
        "POST",
        "/invoices",
        Some(json!({"customer_id": customer, "due_date": "2026-12-31",
                    "line_items": [{"description": "x", "quantity": 1, "unit_amount_cents": 1}]})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (_, list) = call(&db, &b, "GET", "/invoices", None).await;
    assert_eq!(list["data"], json!([]));
    let (_, list) = call(&db, &b, "GET", "/customers", None).await;
    assert_eq!(list["data"], json!([]));
}

#[sqlx::test(migrations = "../../migrations")]
async fn server_computes_total_and_rejects_client_money_fields(db: PgPool) {
    let key = business(&db, "money001").await;
    let customer = create_customer(&db, &key).await;
    let invoice = create_invoice(&db, &key, &customer).await;
    assert_eq!(invoice["total_cents"], 3099);
    assert_eq!(invoice["status"], "open");

    let (stored_total, line_sum): (i64, i64) = sqlx::query_as(
        "SELECT i.total_cents, SUM(l.line_total_cents)::bigint
         FROM invoices i JOIN invoice_line_items l ON l.invoice_id = i.id
         WHERE i.id = $1::uuid GROUP BY i.total_cents",
    )
    .bind(invoice["id"].as_str().unwrap())
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!((stored_total, line_sum), (3099, 3099));

    let bad_bodies = [
        json!({"customer_id": customer, "due_date": "2026-12-31", "total_cents": 1,
               "line_items": [{"description": "x", "quantity": 1, "unit_amount_cents": 100}]}),
        json!({"customer_id": customer, "due_date": "2026-12-31",
               "line_items": [{"description": "x", "quantity": 1, "unit_amount_cents": 100, "line_total_cents": 1}]}),
        json!({"customer_id": customer, "due_date": "2026-12-31",
               "line_items": [{"description": "x", "quantity": 0, "unit_amount_cents": 100}]}),
        json!({"customer_id": customer, "due_date": "2026-12-31",
               "line_items": [{"description": "x", "quantity": 1.5, "unit_amount_cents": 100}]}),
        json!({"customer_id": customer, "due_date": "2026-12-31",
               "line_items": [{"description": "x", "quantity": 1, "unit_amount_cents": 10.5}]}),
    ];
    for bad in bad_bodies {
        let (status, body) = call(&db, &key, "POST", "/invoices", Some(bad.clone())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad} -> {body}");
        assert_eq!(body["error"]["code"], "validation_error");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn invoice_created_is_written_to_the_outbox(db: PgPool) {
    let key = business(&db, "outbox01").await;
    let (status, endpoint) =
        call(&db, &key, "POST", "/webhook_endpoints", Some(json!({"url": "http://receiver.test/hook"}))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(endpoint["signing_secret"].as_str().unwrap().starts_with("whsec_"));

    let (_, listed) = call(&db, &key, "GET", "/webhook_endpoints", None).await;
    assert!(listed[0].get("signing_secret").is_none(), "secret must not be listed");

    let customer = create_customer(&db, &key).await;
    let invoice = create_invoice(&db, &key, &customer).await;

    let events: Vec<(String, String)> = sqlx::query_as(
        "SELECT event_type, payload->'data'->'invoice'->>'id' FROM webhook_events WHERE status = 'pending'",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(events, [("invoice.created".to_string(), invoice["id"].as_str().unwrap().to_string())]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn invalid_transitions_are_rejected_with_current_state(db: PgPool) {
    let key = business(&db, "states01").await;
    let customer = create_customer(&db, &key).await;
    let invoice = create_invoice(&db, &key, &customer).await;
    let id = invoice["id"].as_str().unwrap();

    let (status, voided) = call(&db, &key, "POST", &format!("/invoices/{id}/void"), None).await;
    assert_eq!((status, voided["status"].as_str()), (StatusCode::OK, Some("void")));

    for action in ["void", "uncollectible"] {
        let (status, body) = call(&db, &key, "POST", &format!("/invoices/{id}/{action}"), None).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "invalid_state_transition");
        assert_eq!(body["error"]["details"]["current_state"], "void");
    }

    let (_, voids) = call(&db, &key, "GET", "/invoices?state=void", None).await;
    assert_eq!(voids["data"][0]["id"], id);
    let (_, opens) = call(&db, &key, "GET", "/invoices?state=open", None).await;
    assert_eq!(opens["data"], json!([]));
    let (status, _) = call(&db, &key, "GET", "/invoices?state=draft", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "../../migrations")]
async fn customer_list_pages_with_cursor(db: PgPool) {
    let key = business(&db, "paging01").await;
    for _ in 0..3 {
        create_customer(&db, &key).await;
    }
    let (_, first) = call(&db, &key, "GET", "/customers?limit=2", None).await;
    assert_eq!(first["data"].as_array().unwrap().len(), 2);
    let cursor = first["next_cursor"].as_str().unwrap();
    let (_, second) = call(&db, &key, "GET", &format!("/customers?limit=2&cursor={cursor}"), None).await;
    assert_eq!(second["data"].as_array().unwrap().len(), 1);
    assert!(second["next_cursor"].is_null());
}
