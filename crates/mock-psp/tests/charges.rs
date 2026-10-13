use std::time::Duration;

use mock_psp::{Config, app};
use serde_json::{Value, json};

async fn start(config: Config) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app(config)).await.unwrap() });
    format!("http://{addr}")
}

fn fast() -> Config {
    Config {
        latency: Duration::from_millis(10),
        timeout_token_delay: Duration::from_millis(600),
    }
}

async fn charge(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    token: &str,
) -> reqwest::Result<reqwest::Response> {
    client
        .post(format!("{base}/charges"))
        .header("Idempotency-Key", key)
        .json(&json!({ "card_token": token, "amount_cents": 3099 }))
        .send()
        .await
}

async fn charge_json(base: &str, key: &str, token: &str) -> (u16, Value) {
    let resp = charge(&reqwest::Client::new(), base, key, token)
        .await
        .unwrap();
    (resp.status().as_u16(), resp.json().await.unwrap())
}

async fn lookup(base: &str, key: &str) -> (u16, Value) {
    let resp = reqwest::get(format!("{base}/charges/{key}")).await.unwrap();
    (resp.status().as_u16(), resp.json().await.unwrap())
}

#[tokio::test]
async fn tokens_produce_their_documented_outcomes() {
    let base = start(fast()).await;

    let (status, body) = charge_json(&base, "k-success", "tok_success").await;
    assert_eq!((status, body["status"].as_str()), (200, Some("succeeded")));
    assert!(body["psp_ref"].as_str().is_some());

    for (token, code) in [
        ("tok_insufficient_funds", "insufficient_funds"),
        ("tok_card_declined", "card_declined"),
        ("tok_unknown", "card_declined"),
    ] {
        let (status, body) = charge_json(&base, token, token).await;
        assert_eq!(status, 200);
        assert_eq!(body, json!({ "status": "failed", "code": code }));
    }

    assert_eq!(lookup(&base, "never-sent").await.0, 404);
}

#[tokio::test]
async fn same_key_replays_the_stored_result_without_a_new_charge() {
    let base = start(fast()).await;
    let first = charge_json(&base, "k-replay", "tok_success").await;
    let second = charge_json(&base, "k-replay", "tok_success").await;
    assert_eq!(first, second, "same psp_ref on replay");

    let stats: Value = reqwest::get(format!("{base}/stats"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stats["charges"]["k-replay"]["calls"], 2);
    assert_eq!(stats["charges"].as_object().unwrap().len(), 1);
}

#[tokio::test]
async fn key_reuse_with_a_different_body_or_missing_key_is_rejected() {
    let base = start(fast()).await;
    charge_json(&base, "k-conflict", "tok_success").await;
    let (status, body) = charge_json(&base, "k-conflict", "tok_card_declined").await;
    assert_eq!(status, 409);
    assert_eq!(body["error"]["code"], "idempotency_conflict");

    let resp = reqwest::Client::new()
        .post(format!("{base}/charges"))
        .json(&json!({ "card_token": "tok_success", "amount_cents": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn timeout_charge_settles_after_the_caller_gives_up() {
    let base = start(fast()).await;
    let impatient = reqwest::Client::builder()
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap();

    let err = charge(&impatient, &base, "k-timeout", "tok_timeout")
        .await
        .unwrap_err();
    assert!(err.is_timeout());
    assert_eq!(lookup(&base, "k-timeout").await.1["status"], "pending");

    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(lookup(&base, "k-timeout").await.1["status"], "succeeded");
}

#[tokio::test]
async fn network_error_is_a_500_with_no_charge() {
    let base = start(fast()).await;
    let (status, body) = charge_json(&base, "k-neterr", "tok_network_error").await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (500, Some("processing_error"))
    );
    assert_eq!(
        lookup(&base, "k-neterr").await.1,
        json!({ "status": "failed", "code": "processing_error" })
    );
}

#[tokio::test]
async fn network_drop_charges_but_loses_the_response() {
    let base = start(fast()).await;
    let result = charge(&reqwest::Client::new(), &base, "k-drop", "tok_network_drop").await;
    let response_lost = match result {
        Err(_) => true,
        Ok(resp) => resp.bytes().await.is_err(),
    };
    assert!(response_lost, "caller must not receive a readable response");
    assert_eq!(lookup(&base, "k-drop").await.1["status"], "succeeded");
}
