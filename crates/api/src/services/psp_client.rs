use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

/// HTTP client for the PSP. The payment attempt id is the PSP idempotency key.
#[derive(Clone)]
pub struct PspClient {
    http: reqwest::Client,
    base_url: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ChargeOutcome {
    Succeeded {
        psp_ref: String,
    },
    Declined {
        code: String,
    },
    /// Timeout, transport error, 5xx or unexpected reply: the charge may or may not exist.
    Unknown {
        reason: String,
    },
}

#[derive(Debug, PartialEq)]
pub enum LookupOutcome {
    Succeeded {
        psp_ref: String,
    },
    Failed {
        code: String,
    },
    Pending,
    /// The PSP never received a charge with this key.
    NotFound,
    Unreachable {
        reason: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum PspStatus {
    Pending,
    Succeeded { psp_ref: String },
    Failed { code: String },
}

impl PspClient {
    pub fn new(
        base_url: &str,
        connect_timeout: Duration,
        timeout: Duration,
    ) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(timeout)
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    pub async fn charge(
        &self,
        attempt_id: Uuid,
        card_token: &str,
        amount_cents: i64,
    ) -> ChargeOutcome {
        let unknown = |reason: String| ChargeOutcome::Unknown { reason };
        let resp = match self
            .http
            .post(format!("{}/charges", self.base_url))
            .header("Idempotency-Key", attempt_id.to_string())
            .json(&json!({ "card_token": card_token, "amount_cents": amount_cents }))
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => return unknown(format!("request failed: {err}")),
        };
        let status = resp.status();
        if status != StatusCode::OK {
            return unknown(format!("PSP returned HTTP {status}"));
        }
        match resp.json::<PspStatus>().await {
            Ok(PspStatus::Succeeded { psp_ref }) => ChargeOutcome::Succeeded { psp_ref },
            Ok(PspStatus::Failed { code }) => ChargeOutcome::Declined { code },
            Ok(PspStatus::Pending) => unknown("PSP reported pending".into()),
            Err(err) => unknown(format!("unreadable PSP response: {err}")),
        }
    }

    pub async fn lookup(&self, attempt_id: Uuid) -> LookupOutcome {
        let unreachable = |reason: String| LookupOutcome::Unreachable { reason };
        let resp = match self
            .http
            .get(format!("{}/charges/{attempt_id}", self.base_url))
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => return unreachable(format!("request failed: {err}")),
        };
        match resp.status() {
            StatusCode::NOT_FOUND => LookupOutcome::NotFound,
            StatusCode::OK => match resp.json::<PspStatus>().await {
                Ok(PspStatus::Succeeded { psp_ref }) => LookupOutcome::Succeeded { psp_ref },
                Ok(PspStatus::Failed { code }) => LookupOutcome::Failed { code },
                Ok(PspStatus::Pending) => LookupOutcome::Pending,
                Err(err) => unreachable(format!("unreadable PSP response: {err}")),
            },
            other => unreachable(format!("PSP returned HTTP {other}")),
        }
    }
}
