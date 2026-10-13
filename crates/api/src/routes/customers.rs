use axum::{
    Extension, Json,
    extract::{Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ListParams, Page};
use crate::{
    auth::BusinessId,
    error::AppError,
    extract::{ApiJson, ApiPath},
    state::AppState,
};

const MAX_NAME_CHARS: usize = 200;
const MAX_EMAIL_CHARS: usize = 320;

#[derive(Serialize, sqlx::FromRow)]
pub struct Customer {
    pub id: Uuid,
    pub name: String,
    pub email: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCustomer {
    name: String,
    email: String,
}

fn validate(body: &CreateCustomer) -> Result<(String, String), AppError> {
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS || name.contains(char::is_control) {
        return Err(AppError::validation(format!(
            "name must be 1 to {MAX_NAME_CHARS} characters without control characters"
        )));
    }
    let email = body.email.trim();
    let well_formed = email
        .split_once('@')
        .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'));
    if !well_formed
        || email.len() > MAX_EMAIL_CHARS
        || email.contains(|c: char| c.is_whitespace() || c.is_control())
    {
        return Err(AppError::validation("email is not a valid address"));
    }
    Ok((name.to_string(), email.to_string()))
}

pub async fn create(
    State(state): State<AppState>,
    Extension(BusinessId(business_id)): Extension<BusinessId>,
    ApiJson(body): ApiJson<CreateCustomer>,
) -> Result<(StatusCode, Json<Customer>), AppError> {
    let (name, email) = validate(&body)?;
    let customer: Customer = sqlx::query_as(
        "INSERT INTO customers (business_id, name, email) VALUES ($1, $2, $3)
         RETURNING id, name, email, created_at",
    )
    .bind(business_id)
    .bind(name)
    .bind(email)
    .fetch_one(&state.db)
    .await?;
    Ok((StatusCode::CREATED, Json(customer)))
}

pub async fn get(
    State(state): State<AppState>,
    Extension(BusinessId(business_id)): Extension<BusinessId>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<Customer>, AppError> {
    sqlx::query_as(
        "SELECT id, name, email, created_at FROM customers WHERE id = $1 AND business_id = $2",
    )
    .bind(id)
    .bind(business_id)
    .fetch_optional(&state.db)
    .await?
    .map(Json)
    .ok_or_else(|| AppError::not_found("customer"))
}

pub async fn list(
    State(state): State<AppState>,
    Extension(BusinessId(business_id)): Extension<BusinessId>,
    Query(params): Query<ListParams>,
) -> Result<Json<Page<Customer>>, AppError> {
    let limit = params.limit()?;
    let rows: Vec<Customer> = sqlx::query_as(
        "SELECT id, name, email, created_at FROM customers
         WHERE business_id = $1
           AND ($2::uuid IS NULL OR (created_at, id) <
                (SELECT created_at, id FROM customers WHERE id = $2 AND business_id = $1))
         ORDER BY created_at DESC, id DESC
         LIMIT $3",
    )
    .bind(business_id)
    .bind(params.cursor()?)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |c| c.id)))
}
