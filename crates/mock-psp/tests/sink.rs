#![expect(
    clippy::unwrap_used,
    reason = "test helpers: a failed setup step should abort the test"
)]

use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use mock_psp::{Config, app};
use serde_json::{Value, json};
use sha2::Sha256;

async fn start() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app(Config::default())).await.unwrap() });
    format!("http://{addr}")
}

fn sign(secret: &str, t: u64, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{t}.{body}").as_bytes());
    let hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("t={t},v1={hex}")
}

async fn post(base: &str, signature: &str, body: &str) {
    let resp = reqwest::Client::new()
        .post(format!("{base}/sink"))
        .header("Paynexa-Signature", signature)
        .header("Paynexa-Event-Type", "invoice.paid")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

#[tokio::test]
async fn sink_reports_valid_invalid_and_unverified_signatures() {
    let base = start().await;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let body = r#"{"id":"evt_1"}"#;

    post(&base, &sign("whsec_x", now, body), body).await;
    reqwest::Client::new()
        .put(format!("{base}/sink/secret"))
        .json(&json!({ "secret": "whsec_x" }))
        .send()
        .await
        .unwrap();
    post(&base, &sign("whsec_x", now, body), body).await;
    post(&base, &sign("whsec_x", now, body), r#"{"id":"evt_2"}"#).await;
    post(&base, &sign("whsec_x", now - 600, body), body).await;

    let events: Value = reqwest::get(format!("{base}/sink/events"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let verdicts: Vec<Value> = events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["signature_valid"].clone())
        .collect();
    assert_eq!(
        verdicts,
        [Value::Null, json!(true), json!(false), json!(false)]
    );

    let resp = reqwest::Client::new()
        .post(format!("{base}/sink/fail"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 503);
}
