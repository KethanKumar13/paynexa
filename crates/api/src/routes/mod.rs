use axum::{Router, middleware, routing::get, routing::post};
use serde::{Deserialize, Serialize};
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;
use uuid::Uuid;

use crate::{auth, error::AppError, state::AppState};

pub mod customers;
pub mod health;
pub mod invoices;
pub mod payments;
pub mod webhooks;

pub fn router(state: AppState) -> Router {
    // Every business route must live here so the API-key layer covers it.
    let protected = Router::new()
        .route("/customers", post(customers::create).get(customers::list))
        .route("/customers/{id}", get(customers::get))
        .route("/invoices", post(invoices::create).get(invoices::list))
        .route("/invoices/{id}", get(invoices::get))
        .route("/invoices/{id}/void", post(invoices::void))
        .route(
            "/invoices/{id}/uncollectible",
            post(invoices::mark_uncollectible),
        )
        .route("/invoices/{id}/pay", post(payments::pay))
        .route("/payment_attempts/{id}", get(payments::get_attempt))
        .route(
            "/webhook_endpoints",
            post(webhooks::create).get(webhooks::list),
        )
        .route("/webhook_events", get(webhooks::list_events))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_api_key,
        ));

    Router::new()
        .route("/healthz", get(health::healthz))
        .merge(protected)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .with_state(state)
}

async fn not_found() -> AppError {
    AppError::not_found("route")
}

async fn method_not_allowed() -> AppError {
    AppError::method_not_allowed()
}

const DEFAULT_PAGE_SIZE: i64 = 20;
const MAX_PAGE_SIZE: i64 = 100;

#[derive(Deserialize)]
pub struct ListParams {
    pub limit: Option<String>,
    pub cursor: Option<String>,
    pub state: Option<String>,
    pub status: Option<String>,
}

impl ListParams {
    pub fn limit(&self) -> Result<i64, AppError> {
        match self.limit.as_deref() {
            None => Ok(DEFAULT_PAGE_SIZE),
            Some(raw) => raw
                .parse::<i64>()
                .ok()
                .filter(|n| (1..=MAX_PAGE_SIZE).contains(n))
                .ok_or_else(|| {
                    AppError::validation(format!(
                        "limit must be an integer from 1 to {MAX_PAGE_SIZE}"
                    ))
                }),
        }
    }

    pub fn cursor(&self) -> Result<Option<Uuid>, AppError> {
        self.cursor
            .as_deref()
            .map(|raw| Uuid::parse_str(raw).map_err(|_| AppError::validation("cursor is invalid")))
            .transpose()
    }
}

/// Keyset page: `next_cursor` is the id of the last item, or null on the last page.
#[derive(Serialize)]
pub struct Page<T> {
    pub data: Vec<T>,
    pub next_cursor: Option<Uuid>,
}

impl<T> Page<T> {
    /// `rows` must be fetched with `LIMIT limit + 1` to detect a following page.
    pub fn from_rows(mut rows: Vec<T>, limit: i64, id_of: impl Fn(&T) -> Uuid) -> Self {
        let limit = usize::try_from(limit).unwrap_or(0);
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = if has_more {
            rows.last().map(id_of)
        } else {
            None
        };
        Self {
            data: rows,
            next_cursor,
        }
    }
}
