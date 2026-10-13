use axum::{Extension, Json, extract::State, http::StatusCode};
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use super::invoices::Invoice;
use crate::{auth::BusinessId, error::AppError, extract::ApiJson, state::AppState};

const MAX_URL_LEN: usize = 2048;

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

/// Outbox write: call inside the transaction that changed the invoice.
pub async fn enqueue_invoice_event(
    conn: &mut PgConnection,
    business_id: Uuid,
    event_type: &str,
    invoice: &Invoice,
) -> Result<(), AppError> {
    let payload = json!({
        "type": event_type,
        "created_at": Utc::now(),
        "data": { "invoice": invoice },
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
