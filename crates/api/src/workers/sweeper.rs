//! Reconciles payment attempts whose outcome the request path never learned
//! (timeouts, dropped connections, crashes). It only *looks up* charges at the
//! PSP and never re-sends them, so it cannot cause a second charge.

use std::time::Duration;

use tokio::{task::JoinHandle, time::MissedTickBehavior};
use uuid::Uuid;

use crate::{
    error::AppError,
    services::{
        payment_service::finalize,
        psp_client::{ChargeOutcome, LookupOutcome},
    },
    state::AppState,
};

const BATCH_SIZE: i64 = 50;

pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(state.payments.sweeper_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(err) = run_once(&state).await {
                tracing::error!(error = ?err, "sweeper run failed");
            }
        }
    })
}

/// One reconciliation pass; returns how many attempts reached a final state.
pub async fn run_once(state: &AppState) -> Result<usize, AppError> {
    let settings = &state.payments;
    // No row lock is held across the PSP lookup; finalize() is status-conditional instead.
    let candidates: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT id, (EXTRACT(EPOCH FROM now() - created_at) * 1000)::bigint
         FROM payment_attempts
         WHERE status IN ('pending', 'unknown')
           AND created_at <= now() - ($1::bigint * interval '1 millisecond')
         ORDER BY created_at
         LIMIT $2",
    )
    .bind(millis(settings.sweeper_min_age))
    .bind(BATCH_SIZE)
    .fetch_all(&state.db)
    .await?;

    let mut resolved = 0;
    for (attempt_id, age_ms) in candidates {
        let age = Duration::from_millis(u64::try_from(age_ms).unwrap_or(0));
        let outcome = match state.psp.lookup(attempt_id).await {
            LookupOutcome::Succeeded { psp_ref } => Some(ChargeOutcome::Succeeded { psp_ref }),
            LookupOutcome::Failed { code } => Some(ChargeOutcome::Declined { code }),
            LookupOutcome::NotFound if age >= settings.not_found_grace => {
                Some(ChargeOutcome::Declined {
                    code: "not_received_by_psp".into(),
                })
            }
            LookupOutcome::NotFound | LookupOutcome::Pending => None,
            LookupOutcome::Unreachable { reason } => {
                tracing::warn!(%attempt_id, %reason, "PSP lookup failed");
                None
            }
        };

        match outcome {
            Some(outcome) => match finalize(&state.db, attempt_id, &outcome).await {
                Ok(attempt) => {
                    resolved += 1;
                    tracing::info!(%attempt_id, status = %attempt.status, "sweeper reconciled attempt");
                }
                Err(err) => tracing::error!(%attempt_id, error = ?err, "sweeper finalize failed"),
            },
            None if age >= settings.alert_age => {
                tracing::error!(%attempt_id, age_secs = age.as_secs(), "payment attempt still unresolved");
            }
            None => {}
        }
    }
    Ok(resolved)
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
