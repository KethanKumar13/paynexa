use sqlx::PgPool;

use crate::{config::PaymentSettings, services::psp_client::PspClient};

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub psp: PspClient,
    pub payments: PaymentSettings,
}

impl AppState {
    pub fn new(db: PgPool, psp: PspClient, payments: PaymentSettings) -> Self {
        Self { db, psp, payments }
    }
}
