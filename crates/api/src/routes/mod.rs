use axum::{Router, middleware, routing::get};
use tower_http::trace::TraceLayer;

use crate::{auth, error::AppError, state::AppState};

pub mod health;

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), auth::require_api_key));

    Router::new()
        .route("/healthz", get(health::healthz))
        .merge(protected)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn not_found() -> AppError {
    AppError::not_found("route")
}
