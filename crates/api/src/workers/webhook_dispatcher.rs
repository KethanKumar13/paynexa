//! Delivers outbox rows from `webhook_events`. A short statement claims a batch
//! and leases it; delivery then happens with no database lock held. If the
//! process dies mid-delivery the lease simply expires and the event is retried.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::{task::JoinSet, time::MissedTickBehavior};
use uuid::Uuid;

use crate::{error::AppError, state::AppState};

const BATCH_SIZE: i64 = 20;
const MAX_ERROR_CHARS: usize = 500;

#[derive(sqlx::FromRow)]
struct ClaimedEvent {
    id: Uuid,
    event_type: String,
    payload: Value,
    attempt_count: i32,
    url: String,
    signing_secret: String,
    active: bool,
}

pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(state.webhooks.settings.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(err) = run_once(&state).await {
                tracing::error!(error = ?err, "webhook dispatch run failed");
            }
        }
    })
}

/// One dispatch pass; returns how many events were delivered.
pub async fn run_once(state: &AppState) -> Result<usize, AppError> {
    let claimed: Vec<ClaimedEvent> = sqlx::query_as(
        "WITH due AS (
             SELECT id FROM webhook_events
             WHERE status = 'pending' AND next_attempt_at <= now()
             ORDER BY next_attempt_at
             LIMIT $1
             FOR UPDATE SKIP LOCKED
         )
         UPDATE webhook_events e
         SET next_attempt_at = now() + ($2::bigint * interval '1 millisecond')
         FROM due, webhook_endpoints w
         WHERE e.id = due.id AND w.id = e.endpoint_id
         RETURNING e.id, e.event_type, e.payload, e.attempt_count, w.url, w.signing_secret, w.active",
    )
    .bind(BATCH_SIZE)
    .bind(millis(state.webhooks.settings.lease))
    .fetch_all(&state.db)
    .await?;

    let mut deliveries = JoinSet::new();
    for event in claimed {
        let state = state.clone();
        deliveries.spawn(async move { attempt(&state, event).await });
    }
    let mut delivered = 0;
    while let Some(joined) = deliveries.join_next().await {
        match joined {
            Ok(Ok(true)) => delivered += 1,
            Ok(Ok(false)) => {}
            Ok(Err(err)) => tracing::error!(error = ?err, "recording webhook outcome failed"),
            Err(err) => tracing::error!(error = %err, "webhook delivery task panicked"),
        }
    }
    Ok(delivered)
}

async fn attempt(state: &AppState, event: ClaimedEvent) -> Result<bool, AppError> {
    if !event.active {
        mark_failed(state, &event, "endpoint inactive").await?;
        return Ok(false);
    }

    let mut body = event.payload.clone();
    if let Value::Object(map) = &mut body {
        map.insert("id".into(), json!(event.id));
    }
    let bytes = serde_json::to_vec(&body).map_err(AppError::internal)?;
    let result = state
        .webhooks
        .deliver(
            &event.url,
            &event.signing_secret,
            event.id,
            &event.event_type,
            bytes,
        )
        .await;

    let attempts = event.attempt_count.saturating_add(1);
    match result {
        Ok(()) => {
            sqlx::query(
                "UPDATE webhook_events
                 SET status = 'delivered', delivered_at = now(), attempt_count = $2, last_error = NULL
                 WHERE id = $1 AND status = 'pending'",
            )
            .bind(event.id)
            .bind(attempts)
            .execute(&state.db)
            .await?;
            tracing::info!(event_id = %event.id, event_type = %event.event_type, attempts, "webhook delivered");
            Ok(true)
        }
        Err(error) if attempts >= state.webhooks.settings.max_attempts() => {
            mark_failed(state, &event, &error).await?;
            Ok(false)
        }
        Err(error) => {
            let retry_index = usize::try_from(attempts - 1).unwrap_or(0);
            let delay = state
                .webhooks
                .settings
                .retry_schedule
                .get(retry_index)
                .copied()
                .unwrap_or_default();
            sqlx::query(
                "UPDATE webhook_events
                 SET attempt_count = $2, last_error = $3,
                     next_attempt_at = now() + ($4::bigint * interval '1 millisecond')
                 WHERE id = $1 AND status = 'pending'",
            )
            .bind(event.id)
            .bind(attempts)
            .bind(truncate(&error))
            .bind(millis(delay))
            .execute(&state.db)
            .await?;
            tracing::warn!(event_id = %event.id, attempts, %error, retry_in_secs = delay.as_secs(), "webhook delivery failed; will retry");
            Ok(false)
        }
    }
}

async fn mark_failed(state: &AppState, event: &ClaimedEvent, error: &str) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE webhook_events
         SET status = 'failed', attempt_count = attempt_count + 1, last_error = $2
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(event.id)
    .bind(truncate(error))
    .execute(&state.db)
    .await?;
    tracing::error!(event_id = %event.id, event_type = %event.event_type, %error, "webhook delivery gave up");
    Ok(())
}

fn truncate(error: &str) -> String {
    error.chars().take(MAX_ERROR_CHARS).collect()
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
