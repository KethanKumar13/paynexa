use axum::{Json, extract::State};
use serde_json::{Value, json};

use crate::{error::AppError, state::AppState};

pub async fn healthz(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    sqlx::query("SELECT 1").execute(&state.db).await?;
    Ok(Json(json!({ "status": "ok" })))
}
