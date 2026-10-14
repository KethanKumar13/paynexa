//! Demo webhook receiver. `PUT /sink/secret` hands it an endpoint's signing
//! secret; every `POST /sink` is then verified and logged as valid or invalid.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post, put},
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;

const MAX_EVENTS: usize = 100;
const TOLERANCE_SECS: u64 = 300;

#[derive(Clone, Serialize)]
pub struct ReceivedEvent {
    pub event_id: Option<String>,
    pub event_type: Option<String>,
    /// `None` when no secret has been configured yet.
    pub signature_valid: Option<bool>,
    pub received_at: i64,
}

#[derive(Default)]
struct Sink {
    secret: Option<String>,
    events: VecDeque<ReceivedEvent>,
}

type Shared = Arc<Mutex<Sink>>;

fn lock(sink: &Shared) -> std::sync::MutexGuard<'_, Sink> {
    sink.lock().expect("sink mutex poisoned")
}

pub fn router() -> Router {
    Router::new()
        .route("/sink", post(receive))
        .route("/sink/secret", put(set_secret))
        .route("/sink/events", get(list))
        .route("/sink/fail", post(fail))
        .with_state(Shared::default())
}

#[derive(Deserialize)]
struct SecretBody {
    secret: String,
}

async fn set_secret(State(sink): State<Shared>, Json(body): Json<SecretBody>) -> StatusCode {
    lock(&sink).secret = Some(body.secret);
    StatusCode::NO_CONTENT
}

async fn receive(State(sink): State<Shared>, headers: HeaderMap, body: Bytes) -> Json<Value> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let now = unix_now();
    let mut sink = lock(&sink);
    let signature_valid = sink.secret.as_deref().map(|secret| {
        header("paynexa-signature").is_some_and(|sig| verify(secret, &sig, &body, now))
    });
    let event = ReceivedEvent {
        event_id: header("paynexa-event-id"),
        event_type: header("paynexa-event-type"),
        signature_valid,
        received_at: now,
    };
    let signature = match signature_valid {
        Some(true) => "valid",
        Some(false) => "invalid",
        None => "unverified",
    };
    tracing::info!(event_id = ?event.event_id, event_type = ?event.event_type, signature, "webhook received");
    if sink.events.len() == MAX_EVENTS {
        sink.events.pop_front();
    }
    sink.events.push_back(event);
    Json(json!({ "received": true }))
}

async fn list(State(sink): State<Shared>) -> Json<Vec<ReceivedEvent>> {
    Json(lock(&sink).events.iter().cloned().collect())
}

async fn fail() -> (StatusCode, Json<Value>) {
    let body = json!({ "error": { "code": "unavailable", "message": "sink configured to fail" } });
    (StatusCode::SERVICE_UNAVAILABLE, Json(body))
}

/// Checks `t=<unix>,v1=<hex>` against HMAC-SHA256(secret, "<t>.<body>") within a 5-minute window.
pub fn verify(secret: &str, header: &str, body: &[u8], now: i64) -> bool {
    let mut timestamp = None;
    let mut signature = None;
    for part in header.split(',') {
        if let Some(t) = part.strip_prefix("t=") {
            timestamp = t.parse::<i64>().ok();
        } else if let Some(v1) = part.strip_prefix("v1=") {
            signature = decode_hex(v1);
        }
    }
    let (Some(timestamp), Some(signature)) = (timestamp, signature) else {
        return false;
    };
    if now.abs_diff(timestamp) > TOLERANCE_SECS {
        return false;
    }
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    // verify_slice compares in constant time.
    mac.verify_slice(&signature).is_ok()
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
