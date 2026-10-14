use sqlx::PgPool;

use crate::{
    config::PaymentSettings,
    services::{psp_client::PspClient, webhook_delivery::WebhookClient},
};

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub psp: PspClient,
    pub payments: PaymentSettings,
    pub webhooks: WebhookClient,
}

impl AppState {
    pub fn new(
        db: PgPool,
        psp: PspClient,
        payments: PaymentSettings,
        webhooks: WebhookClient,
    ) -> Self {
        Self {
            db,
            psp,
            payments,
            webhooks,
        }
    }
}
