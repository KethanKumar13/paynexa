use axum::{
    Extension, Json,
    extract::{Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{ListParams, Page};
use crate::{auth::BusinessId, error::AppError, extract::ApiJson, state::AppState};

const MAX_URL_LEN: usize = 2048;
const EVENT_STATUSES: [&str; 3] = ["pending", "delivered", "failed"];

#[derive(Serialize, sqlx::FromRow)]
pub struct WebhookEvent {
    pub id: Uuid,
    pub endpoint_id: Uuid,
    pub event_type: String,
    pub status: String,
    pub attempt_count: i32,
    pub next_attempt_at: DateTime<Utc>,
    pub last_error: Option<String>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub payload: Value,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct WebhookEndpoint {
    pub id: Uuid,
    pub url: String,
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

/// Returned only from registration; the secret is never shown again.
#[derive(Serialize)]
pub struct CreatedWebhookEndpoint {
    #[serde(flatten)]
    endpoint: WebhookEndpoint,
    signing_secret: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateWebhookEndpoint {
    url: String,
}

fn validate_url(raw: &str) -> Result<String, AppError> {
    let url = raw.trim();
    let host = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    let has_host = host.is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/'));
    if !has_host
        || url.len() > MAX_URL_LEN
        || url.contains(|c: char| c.is_whitespace() || c.is_control())
    {
        return Err(AppError::validation("url must be an absolute http(s) URL"));
    }
    Ok(url.to_string())
}

fn generate_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("whsec_{hex}")
}

pub async fn create(
    State(state): State<AppState>,
    Extension(BusinessId(business_id)): Extension<BusinessId>,
    ApiJson(body): ApiJson<CreateWebhookEndpoint>,
) -> Result<(StatusCode, Json<CreatedWebhookEndpoint>), AppError> {
    let url = validate_url(&body.url)?;
    let signing_secret = generate_secret();
    let endpoint: WebhookEndpoint = sqlx::query_as(
        "INSERT INTO webhook_endpoints (business_id, url, signing_secret) VALUES ($1, $2, $3)
         RETURNING id, url, active, created_at",
    )
    .bind(business_id)
    .bind(url)
    .bind(&signing_secret)
    .fetch_one(&state.db)
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedWebhookEndpoint {
            endpoint,
            signing_secret,
        }),
    ))
}

pub async fn list(
    State(state): State<AppState>,
    Extension(BusinessId(business_id)): Extension<BusinessId>,
) -> Result<Json<Vec<WebhookEndpoint>>, AppError> {
    let endpoints = sqlx::query_as(
        "SELECT id, url, active, created_at FROM webhook_endpoints
         WHERE business_id = $1 ORDER BY created_at",
    )
    .bind(business_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(endpoints))
}

/// Lets a business reconcile deliveries, e.g. `?status=failed` after retries run out.
pub async fn list_events(
    State(state): State<AppState>,
    Extension(BusinessId(business_id)): Extension<BusinessId>,
    Query(params): Query<ListParams>,
) -> Result<Json<Page<WebhookEvent>>, AppError> {
    let limit = params.limit()?;
    let status = params
        .status
        .as_deref()
        .map(|raw| {
            EVENT_STATUSES
                .into_iter()
                .find(|s| *s == raw)
                .ok_or_else(|| {
                    AppError::validation("status must be one of pending, delivered, failed")
                })
        })
        .transpose()?;

    let rows: Vec<WebhookEvent> = sqlx::query_as(
        "SELECT id, endpoint_id, event_type, status, attempt_count, next_attempt_at,
                last_error, delivered_at, created_at, payload
         FROM webhook_events
         WHERE business_id = $1
           AND ($2::text IS NULL OR status = $2)
           AND ($3::uuid IS NULL OR (created_at, id) <
                (SELECT created_at, id FROM webhook_events WHERE id = $3 AND business_id = $1))
         ORDER BY created_at DESC, id DESC
         LIMIT $4",
    )
    .bind(business_id)
    .bind(status)
    .bind(params.cursor()?)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |e| e.id)))
}

/// Outbox write: call inside the transaction that made the state change.
pub async fn enqueue_event(
    conn: &mut PgConnection,
    business_id: Uuid,
    event_type: &str,
    data: Value,
) -> Result<(), AppError> {
    let payload = json!({
        "type": event_type,
        "created_at": Utc::now(),
        "data": data,
    });
    sqlx::query(
        "INSERT INTO webhook_events (business_id, endpoint_id, event_type, payload)
         SELECT $1, id, $2, $3 FROM webhook_endpoints WHERE business_id = $1 AND active",
    )
    .bind(business_id)
    .bind(event_type)
    .bind(payload)
    .execute(conn)
    .await?;
    Ok(())
}
