//! Mock payment processor. Outcomes are chosen by `card_token`; charges are
//! idempotent per `Idempotency-Key` and kept in memory only.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;
use uuid::Uuid;

const MAX_KEY_LEN: usize = 255;

pub mod sink;

#[derive(Clone, Debug)]
pub struct Config {
    /// Delay before every token except `tok_timeout` settles.
    pub latency: Duration,
    /// Delay before `tok_timeout` settles (30 s in the spec).
    pub timeout_token_delay: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            latency: Duration::from_millis(100),
            timeout_token_delay: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ChargeStatus {
    Pending,
    Succeeded { psp_ref: Uuid },
    Failed { code: String },
}

struct Charge {
    card_token: String,
    amount_cents: i64,
    status: ChargeStatus,
    calls: u32,
}

#[derive(Default)]
struct Store {
    charges: HashMap<String, Charge>,
    total_requests: u64,
}

#[derive(Clone)]
struct AppState {
    config: Config,
    store: Arc<Mutex<Store>>,
}

impl AppState {
    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().expect("charge store mutex poisoned")
    }
}

/// How the original request is answered once its charge settles.
enum Reply {
    Result(ChargeStatus),
    ServerError,
    DropConnection,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChargeRequest {
    card_token: String,
    amount_cents: i64,
}

pub fn app(config: Config) -> Router {
    let state = AppState {
        config,
        store: Arc::default(),
    };
    Router::new()
        .route(
            "/healthz",
            get(|| async { Json(json!({ "status": "ok" })) }),
        )
        .route("/charges", post(create_charge))
        .route("/charges/{key}", get(get_charge))
        .route("/stats", get(stats))
        .with_state(state)
        .merge(sink::router())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
}

fn error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    let body = json!({ "error": { "code": code, "message": message.into() } });
    (status, Json(body)).into_response()
}

async fn create_charge(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<ChargeRequest>, JsonRejection>,
) -> Response {
    let req = match body {
        Ok(Json(req)) => req,
        Err(rejection) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                rejection.body_text(),
            );
        }
    };
    let Some(key) = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty() && k.len() <= MAX_KEY_LEN)
        .map(str::to_owned)
    else {
        return error(
            StatusCode::BAD_REQUEST,
            "missing_idempotency_key",
            "Idempotency-Key header is required",
        );
    };
    if req.amount_cents < 0 {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_amount",
            "amount_cents must not be negative",
        );
    }

    {
        let mut store = state.store();
        store.total_requests += 1;
        if let Some(existing) = store.charges.get_mut(&key) {
            existing.calls += 1;
            if existing.card_token != req.card_token || existing.amount_cents != req.amount_cents {
                return error(
                    StatusCode::CONFLICT,
                    "idempotency_conflict",
                    "Idempotency-Key was already used with a different request",
                );
            }
            let status = existing.status.clone();
            let code = if status == ChargeStatus::Pending {
                StatusCode::ACCEPTED
            } else {
                StatusCode::OK
            };
            tracing::info!(%key, ?status, "replayed charge");
            return (code, Json(status)).into_response();
        }
        // Recorded before any work so a lookup can tell "received" from "never arrived".
        store.charges.insert(
            key.clone(),
            Charge {
                card_token: req.card_token.clone(),
                amount_cents: req.amount_cents,
                status: ChargeStatus::Pending,
                calls: 1,
            },
        );
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    // Spawned so the charge still settles if the caller times out and disconnects.
    tokio::spawn(settle(state, key, req.card_token, reply_tx));

    match reply_rx.await {
        Ok(Reply::Result(status)) => (StatusCode::OK, Json(status)).into_response(),
        Ok(Reply::ServerError) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "processing_error",
            "upstream processor error",
        ),
        Ok(Reply::DropConnection) => dropped_connection(),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "charge worker stopped",
        ),
    }
}

async fn settle(state: AppState, key: String, card_token: String, reply: oneshot::Sender<Reply>) {
    let succeeded = || ChargeStatus::Succeeded {
        psp_ref: Uuid::new_v4(),
    };
    let failed = |code: &str| ChargeStatus::Failed { code: code.into() };
    let latency = state.config.latency;

    let (delay, status) = match card_token.as_str() {
        "tok_success" | "tok_network_drop" => (latency, succeeded()),
        "tok_timeout" => (state.config.timeout_token_delay, succeeded()),
        "tok_insufficient_funds" => (latency, failed("insufficient_funds")),
        "tok_network_error" => (latency, failed("processing_error")),
        _ => (latency, failed("card_declined")),
    };
    tokio::time::sleep(delay).await;

    if let Some(charge) = state.store().charges.get_mut(&key) {
        charge.status = status.clone();
    }
    tracing::info!(%key, %card_token, ?status, "charge settled");

    let outcome = match card_token.as_str() {
        "tok_network_error" => Reply::ServerError,
        // The charge succeeded but the caller never sees the response.
        "tok_network_drop" => Reply::DropConnection,
        _ => Reply::Result(status),
    };
    // The caller may already have disconnected; the charge is recorded either way.
    let _ = reply.send(outcome);
}

fn dropped_connection() -> Response {
    let failing_body = futures_util::stream::once(async {
        Err::<Bytes, _>(std::io::Error::other("mock PSP dropped the connection"))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from_stream(failing_body))
        .expect("static response parts are valid")
}

async fn get_charge(State(state): State<AppState>, Path(key): Path<String>) -> Response {
    match state.store().charges.get(&key) {
        Some(charge) => (StatusCode::OK, Json(charge.status.clone())).into_response(),
        None => error(
            StatusCode::NOT_FOUND,
            "not_found",
            "no charge with this key",
        ),
    }
}

async fn stats(State(state): State<AppState>) -> Json<Value> {
    let store = state.store();
    let charges: Map<String, Value> = store
        .charges
        .iter()
        .map(|(key, c)| (key.clone(), json!({ "calls": c.calls, "result": c.status })))
        .collect();
    Json(json!({ "total_charge_requests": store.total_requests, "charges": charges }))
}
