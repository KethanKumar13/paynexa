use axum::{
    Extension, Json,
    extract::{Query, State},
    http::StatusCode,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{ListParams, Page, webhooks};
use crate::{
    auth::BusinessId,
    domain::invoice_state::{InvoiceStatus, transition},
    error::AppError,
    extract::{ApiJson, ApiPath},
    money::{LineInput, price_lines},
    state::AppState,
};

const INVOICE_COLUMNS: &str =
    "id, customer_id, status, total_cents, currency, due_date, created_at, updated_at";

#[derive(Serialize, sqlx::FromRow)]
pub struct Invoice {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub status: String,
    pub total_cents: i64,
    pub currency: String,
    pub due_date: NaiveDate,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[sqlx(skip)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_items: Option<Vec<LineItem>>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct LineItem {
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
    pub line_total_cents: i64,
}

/// Unknown fields (e.g. a client-supplied total) are rejected, never trusted.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateInvoice {
    customer_id: Uuid,
    due_date: NaiveDate,
    line_items: Vec<LineInput>,
}

async fn load_line_items(
    conn: &mut PgConnection,
    invoice_id: Uuid,
) -> Result<Vec<LineItem>, AppError> {
    Ok(sqlx::query_as(
        "SELECT description, quantity, unit_amount_cents, line_total_cents
         FROM invoice_line_items WHERE invoice_id = $1 ORDER BY position",
    )
    .bind(invoice_id)
    .fetch_all(conn)
    .await?)
}

pub async fn create(
    State(state): State<AppState>,
    Extension(BusinessId(business_id)): Extension<BusinessId>,
    ApiJson(body): ApiJson<CreateInvoice>,
) -> Result<(StatusCode, Json<Invoice>), AppError> {
    let (lines, total_cents) = price_lines(&body.line_items).map_err(AppError::validation)?;

    let mut tx = state.db.begin().await?;
    let customer: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM customers WHERE id = $1 AND business_id = $2")
            .bind(body.customer_id)
            .bind(business_id)
            .fetch_optional(&mut *tx)
            .await?;
    if customer.is_none() {
        return Err(AppError::not_found("customer"));
    }

    let mut invoice: Invoice = sqlx::query_as(&format!(
        "INSERT INTO invoices (business_id, customer_id, status, total_cents, due_date)
         VALUES ($1, $2, $3, $4, $5) RETURNING {INVOICE_COLUMNS}"
    ))
    .bind(business_id)
    .bind(body.customer_id)
    .bind(InvoiceStatus::Open.as_str())
    .bind(total_cents)
    .bind(body.due_date)
    .fetch_one(&mut *tx)
    .await?;

    for (position, line) in lines.iter().enumerate() {
        sqlx::query(
            "INSERT INTO invoice_line_items
                 (invoice_id, position, description, quantity, unit_amount_cents, line_total_cents)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(invoice.id)
        .bind(position as i32)
        .bind(&line.description)
        .bind(line.quantity)
        .bind(line.unit_amount_cents)
        .bind(line.line_total_cents)
        .execute(&mut *tx)
        .await?;
    }
    invoice.line_items = Some(
        lines
            .into_iter()
            .map(|l| LineItem {
                description: l.description,
                quantity: l.quantity,
                unit_amount_cents: l.unit_amount_cents,
                line_total_cents: l.line_total_cents,
            })
            .collect(),
    );

    webhooks::enqueue_invoice_event(&mut tx, business_id, "invoice.created", &invoice).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(invoice)))
}

pub async fn get(
    State(state): State<AppState>,
    Extension(BusinessId(business_id)): Extension<BusinessId>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<Invoice>, AppError> {
    let mut conn = state.db.acquire().await?;
    let mut invoice: Invoice = sqlx::query_as(&format!(
        "SELECT {INVOICE_COLUMNS} FROM invoices WHERE id = $1 AND business_id = $2"
    ))
    .bind(id)
    .bind(business_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| AppError::not_found("invoice"))?;
    invoice.line_items = Some(load_line_items(&mut conn, invoice.id).await?);
    Ok(Json(invoice))
}

pub async fn list(
    State(state): State<AppState>,
    Extension(BusinessId(business_id)): Extension<BusinessId>,
    Query(params): Query<ListParams>,
) -> Result<Json<Page<Invoice>>, AppError> {
    let limit = params.limit()?;
    let status = params
        .state
        .as_deref()
        .map(|raw| {
            InvoiceStatus::parse(raw).ok_or_else(|| {
                AppError::validation(
                    "state must be one of open, processing, paid, void, uncollectible",
                )
            })
        })
        .transpose()?;

    let rows: Vec<Invoice> = sqlx::query_as(&format!(
        "SELECT {INVOICE_COLUMNS} FROM invoices
         WHERE business_id = $1
           AND ($2::text IS NULL OR status = $2)
           AND ($3::uuid IS NULL OR (created_at, id) <
                (SELECT created_at, id FROM invoices WHERE id = $3 AND business_id = $1))
         ORDER BY created_at DESC, id DESC
         LIMIT $4"
    ))
    .bind(business_id)
    .bind(status.map(InvoiceStatus::as_str))
    .bind(params.cursor()?)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |i| i.id)))
}

pub async fn void(
    state: State<AppState>,
    business: Extension<BusinessId>,
    id: ApiPath<Uuid>,
) -> Result<Json<Invoice>, AppError> {
    change_status(state, business, id, InvoiceStatus::Void).await
}

pub async fn mark_uncollectible(
    state: State<AppState>,
    business: Extension<BusinessId>,
    id: ApiPath<Uuid>,
) -> Result<Json<Invoice>, AppError> {
    change_status(state, business, id, InvoiceStatus::Uncollectible).await
}

async fn change_status(
    State(state): State<AppState>,
    Extension(BusinessId(business_id)): Extension<BusinessId>,
    ApiPath(id): ApiPath<Uuid>,
    target: InvoiceStatus,
) -> Result<Json<Invoice>, AppError> {
    let mut tx = state.db.begin().await?;
    let (current,): (String,) =
        sqlx::query_as("SELECT status FROM invoices WHERE id = $1 AND business_id = $2 FOR UPDATE")
            .bind(id)
            .bind(business_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::not_found("invoice"))?;
    let current = InvoiceStatus::parse(&current)
        .ok_or_else(|| AppError::internal(format!("unknown invoice status {current:?}")))?;
    let next = transition(current, target)?;

    let mut invoice: Invoice = sqlx::query_as(&format!(
        "UPDATE invoices SET status = $1, updated_at = now() WHERE id = $2 RETURNING {INVOICE_COLUMNS}"
    ))
    .bind(next.as_str())
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    invoice.line_items = Some(load_line_items(&mut tx, id).await?);
    tx.commit().await?;
    Ok(Json(invoice))
}
