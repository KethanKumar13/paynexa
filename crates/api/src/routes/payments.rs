use axum::{
    Extension, Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    auth::BusinessId,
    domain::invoice_state::{InvoiceStatus, transition},
    error::AppError,
    extract::{ApiJson, ApiPath},
    services::{
        payment_service::{self, PaymentAttempt, load_attempt},
        psp_client::ChargeOutcome,
    },
    state::AppState,
};

const OPERATION: &str = "pay_invoice";
const MAX_KEY_LEN: usize = 255;
const MAX_TOKEN_LEN: usize = 100;

#[derive(sqlx::FromRow)]
struct IdempotencyRecord {
    request_hash: Vec<u8>,
    payment_attempt_id: Option<Uuid>,
    response_status: Option<i32>,
    response_body: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayInvoice {
    card_token: String,
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, AppError> {
    headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty() && k.len() <= MAX_KEY_LEN)
        .map(str::to_owned)
        .ok_or_else(|| {
            AppError::validation(format!(
                "Idempotency-Key header is required (1 to {MAX_KEY_LEN} characters)"
            ))
        })
}

/// 200 once the outcome is final, 202 while it is still being reconciled.
fn attempt_response(attempt: PaymentAttempt) -> Response {
    let code = if attempt.is_in_flight() {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    (code, Json(attempt)).into_response()
}

pub async fn pay(
    State(state): State<AppState>,
    Extension(BusinessId(business_id)): Extension<BusinessId>,
    ApiPath(invoice_id): ApiPath<Uuid>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<PayInvoice>,
) -> Result<Response, AppError> {
    let key = idempotency_key(&headers)?;
    let card_token = body.card_token.trim();
    if card_token.is_empty()
        || card_token.len() > MAX_TOKEN_LEN
        || card_token.contains(|c: char| c.is_whitespace() || c.is_control())
    {
        return Err(AppError::validation(format!(
            "card_token must be 1 to {MAX_TOKEN_LEN} characters without whitespace"
        )));
    }
    let request_hash = Sha256::digest(format!("{invoice_id}:{card_token}")).to_vec();

    // Txn 1: claim the key, lock the invoice, record the attempt, commit before any PSP call.
    let mut tx = state.db.begin().await?;
    let claimed: Option<(Uuid,)> = sqlx::query_as(
        "INSERT INTO idempotency_records (business_id, operation, key, request_hash)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (business_id, operation, key) DO NOTHING RETURNING id",
    )
    .bind(business_id)
    .bind(OPERATION)
    .bind(&key)
    .bind(&request_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((record_id,)) = claimed else {
        tx.rollback().await?;
        return replay(&state, business_id, &key, &request_hash).await;
    };

    let (status, total_cents): (String, i64) = sqlx::query_as(
        "SELECT status, total_cents FROM invoices WHERE id = $1 AND business_id = $2 FOR UPDATE",
    )
    .bind(invoice_id)
    .bind(business_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::not_found("invoice"))?;
    let current = InvoiceStatus::parse(&status)
        .ok_or_else(|| AppError::internal(format!("unknown invoice status {status:?}")))?;
    if current == InvoiceStatus::Processing {
        return Err(AppError::payment_in_progress());
    }
    let next = transition(current, InvoiceStatus::Processing)?;

    let attempt_id: Uuid = match sqlx::query_scalar(
        "INSERT INTO payment_attempts (invoice_id, business_id, card_token, amount_cents, status)
         VALUES ($1, $2, $3, $4, 'pending') RETURNING id",
    )
    .bind(invoice_id)
    .bind(business_id)
    .bind(card_token)
    .bind(total_cents)
    .fetch_one(&mut *tx)
    .await
    {
        Ok(id) => id,
        // Backstop for the row lock: the one-in-flight-attempt index rejected a second attempt.
        Err(sqlx::Error::Database(err)) if err.is_unique_violation() => {
            return Err(AppError::payment_in_progress());
        }
        Err(err) => return Err(err.into()),
    };
    sqlx::query("UPDATE invoices SET status = $1, updated_at = now() WHERE id = $2")
        .bind(next.as_str())
        .bind(invoice_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE idempotency_records SET payment_attempt_id = $1 WHERE id = $2")
        .bind(attempt_id)
        .bind(record_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(%attempt_id, %invoice_id, "payment attempt created");

    let outcome = state.psp.charge(attempt_id, card_token, total_cents).await;
    if let ChargeOutcome::Unknown { reason } = &outcome {
        tracing::warn!(%attempt_id, %reason, "PSP outcome unknown; sweeper will reconcile");
    }
    let attempt = payment_service::finalize(&state.db, attempt_id, &outcome).await?;
    Ok(attempt_response(attempt))
}

async fn replay(
    state: &AppState,
    business_id: Uuid,
    key: &str,
    request_hash: &[u8],
) -> Result<Response, AppError> {
    let record: IdempotencyRecord = sqlx::query_as(
        "SELECT request_hash, payment_attempt_id, response_status, response_body
         FROM idempotency_records WHERE business_id = $1 AND operation = $2 AND key = $3",
    )
    .bind(business_id)
    .bind(OPERATION)
    .bind(key)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::internal("idempotency record vanished"))?;
    if record.request_hash != request_hash {
        return Err(AppError::idempotency_conflict());
    }
    if let (Some(code), Some(body)) = (record.response_status, record.response_body) {
        let code = u16::try_from(code)
            .ok()
            .and_then(|c| StatusCode::from_u16(c).ok())
            .ok_or_else(|| AppError::internal("stored response status is invalid"))?;
        return Ok((code, Json(body)).into_response());
    }
    let attempt_id = record
        .payment_attempt_id
        .ok_or_else(|| AppError::internal("idempotency record has no attempt"))?;
    let mut conn = state.db.acquire().await?;
    let attempt = load_attempt(&mut conn, attempt_id, Some(business_id))
        .await?
        .ok_or_else(|| AppError::internal("payment attempt vanished"))?;
    Ok(attempt_response(attempt))
}

pub async fn get_attempt(
    State(state): State<AppState>,
    Extension(BusinessId(business_id)): Extension<BusinessId>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Response, AppError> {
    let mut conn = state.db.acquire().await?;
    let attempt = load_attempt(&mut conn, id, Some(business_id))
        .await?
        .ok_or_else(|| AppError::not_found("payment attempt"))?;
    Ok((StatusCode::OK, Json(attempt)).into_response())
}
