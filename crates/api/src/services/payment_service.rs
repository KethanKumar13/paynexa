use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::psp_client::ChargeOutcome;
use crate::{
    domain::invoice_state::{InvoiceStatus, transition},
    error::AppError,
    routes::{
        invoices::{INVOICE_COLUMNS, Invoice},
        webhooks,
    },
};

#[derive(Clone, Serialize, sqlx::FromRow)]
pub struct PaymentAttempt {
    pub id: Uuid,
    pub invoice_id: Uuid,
    pub status: String,
    pub amount_cents: i64,
    pub psp_reference: Option<String>,
    pub failure_code: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub invoice_status: String,
}

impl PaymentAttempt {
    pub fn is_in_flight(&self) -> bool {
        matches!(self.status.as_str(), "pending" | "unknown")
    }
}

/// Loads an attempt; `business_id = None` skips the tenant filter (workers only).
pub async fn load_attempt(
    conn: &mut PgConnection,
    attempt_id: Uuid,
    business_id: Option<Uuid>,
) -> Result<Option<PaymentAttempt>, AppError> {
    Ok(sqlx::query_as(
        "SELECT a.id, a.invoice_id, a.status, a.amount_cents, a.psp_reference, a.failure_code,
                a.created_at, a.updated_at, i.status AS invoice_status
         FROM payment_attempts a JOIN invoices i ON i.id = a.invoice_id
         WHERE a.id = $1 AND ($2::uuid IS NULL OR a.business_id = $2)",
    )
    .bind(attempt_id)
    .bind(business_id)
    .fetch_optional(conn)
    .await?)
}

/// Applies a PSP outcome. Only an in-flight attempt changes, so the request
/// handler and the sweeper can both call this; the second call is a no-op.
pub async fn finalize(
    db: &PgPool,
    attempt_id: Uuid,
    outcome: &ChargeOutcome,
) -> Result<PaymentAttempt, AppError> {
    let mut tx = db.begin().await?;

    let (status, psp_ref, failure_code, invoice_target, event_type) = match outcome {
        ChargeOutcome::Unknown { .. } => {
            sqlx::query(
                "UPDATE payment_attempts SET status = 'unknown', updated_at = now()
                 WHERE id = $1 AND status = 'pending'",
            )
            .bind(attempt_id)
            .execute(&mut *tx)
            .await?;
            let attempt = load_attempt(&mut tx, attempt_id, None)
                .await?
                .ok_or_else(|| AppError::internal("payment attempt vanished"))?;
            tx.commit().await?;
            return Ok(attempt);
        }
        ChargeOutcome::Succeeded { psp_ref } => (
            "succeeded",
            Some(psp_ref.as_str()),
            None,
            InvoiceStatus::Paid,
            "invoice.paid",
        ),
        ChargeOutcome::Declined { code } => (
            "failed",
            None,
            Some(code.as_str()),
            InvoiceStatus::Open,
            "invoice.payment_failed",
        ),
    };

    let settled: Option<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE payment_attempts
         SET status = $2, psp_reference = $3, failure_code = $4, updated_at = now()
         WHERE id = $1 AND status IN ('pending', 'unknown')
         RETURNING invoice_id, business_id",
    )
    .bind(attempt_id)
    .bind(status)
    .bind(psp_ref)
    .bind(failure_code)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some((invoice_id, business_id)) = settled {
        let (current,): (String,) =
            sqlx::query_as("SELECT status FROM invoices WHERE id = $1 FOR UPDATE")
                .bind(invoice_id)
                .fetch_one(&mut *tx)
                .await?;
        let current = InvoiceStatus::parse(&current)
            .ok_or_else(|| AppError::internal(format!("unknown invoice status {current:?}")))?;
        let next = transition(current, invoice_target)?;

        let invoice: Invoice = sqlx::query_as(&format!(
            "UPDATE invoices SET status = $1, updated_at = now() WHERE id = $2
             RETURNING {INVOICE_COLUMNS}"
        ))
        .bind(next.as_str())
        .bind(invoice_id)
        .fetch_one(&mut *tx)
        .await?;
        let attempt = load_attempt(&mut tx, attempt_id, None)
            .await?
            .ok_or_else(|| AppError::internal("payment attempt vanished"))?;

        webhooks::enqueue_event(
            &mut tx,
            business_id,
            event_type,
            json!({ "invoice": invoice, "payment_attempt": &attempt }),
        )
        .await?;
        // Retries with the original Idempotency-Key replay this final response.
        sqlx::query(
            "UPDATE idempotency_records SET response_status = 200, response_body = $2
             WHERE payment_attempt_id = $1",
        )
        .bind(attempt_id)
        .bind(json!(&attempt))
        .execute(&mut *tx)
        .await?;
        tracing::info!(%attempt_id, %invoice_id, status, "payment attempt settled");
    }

    let attempt = load_attempt(&mut tx, attempt_id, None)
        .await?
        .ok_or_else(|| AppError::internal("payment attempt vanished"))?;
    tx.commit().await?;
    Ok(attempt)
}
