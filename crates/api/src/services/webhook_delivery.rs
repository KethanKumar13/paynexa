use chrono::Utc;
use reqwest::header::CONTENT_TYPE;
use uuid::Uuid;

use super::webhook_signing::signature_header;
use crate::config::WebhookSettings;

#[derive(Clone)]
pub struct WebhookClient {
    http: reqwest::Client,
    pub settings: WebhookSettings,
}

impl WebhookClient {
    pub fn new(settings: WebhookSettings) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(settings.timeout)
            // A redirect could send the signed payload to a host the business never registered.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("Paynexa-Webhooks/1")
            .build()?;
        Ok(Self { http, settings })
    }

    /// Sends one signed delivery; any 2xx counts as delivered.
    pub async fn deliver(
        &self,
        url: &str,
        secret: &str,
        event_id: Uuid,
        event_type: &str,
        body: Vec<u8>,
    ) -> Result<(), String> {
        let signature = signature_header(secret, Utc::now().timestamp(), &body);
        let resp = self
            .http
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header("Paynexa-Event-Id", event_id.to_string())
            .header("Paynexa-Event-Type", event_type)
            .header("Paynexa-Signature", signature)
            .body(body)
            .send()
            .await
            .map_err(|err| format!("request failed: {err}"))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("receiver returned HTTP {}", resp.status()))
        }
    }
}
