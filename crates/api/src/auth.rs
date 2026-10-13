use axum::{
    extract::{Request, State},
    http::header::AUTHORIZATION,
    middleware::Next,
    response::Response,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{error::AppError, state::AppState};

const KEY_SCHEME: &str = "pnx_live_";
const PREFIX_LEN: usize = 8;

/// Business authenticated by the request's API key; inserted by `require_api_key`.
#[derive(Clone, Copy, Debug)]
pub struct BusinessId(pub Uuid);

fn hash_key(raw: &str) -> Vec<u8> {
    Sha256::digest(raw.as_bytes()).to_vec()
}

/// Key format: `pnx_live_<prefix8>_<secret>`; the prefix is stored in plaintext for lookup.
fn key_prefix(raw: &str) -> Option<&str> {
    let rest = raw.strip_prefix(KEY_SCHEME)?;
    let (prefix, secret) = rest.split_at_checked(PREFIX_LEN)?;
    let secret = secret.strip_prefix('_')?;
    (prefix.chars().all(|c| c.is_ascii_alphanumeric()) && !secret.is_empty()).then_some(prefix)
}

pub async fn require_api_key(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let raw = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(AppError::unauthorized)?;
    let prefix = key_prefix(raw).ok_or_else(AppError::unauthorized)?;

    let row: Option<(Uuid, Vec<u8>)> = sqlx::query_as(
        "SELECT business_id, key_hash FROM api_keys WHERE key_prefix = $1 AND revoked_at IS NULL",
    )
    .bind(prefix)
    .fetch_optional(&state.db)
    .await?;
    let (business_id, stored_hash) = row.ok_or_else(AppError::unauthorized)?;
    if !bool::from(hash_key(raw).ct_eq(&stored_hash)) {
        return Err(AppError::unauthorized());
    }

    req.extensions_mut().insert(BusinessId(business_id));
    Ok(next.run(req).await)
}

/// Creates the demo business and key once; a no-op if the key prefix already exists.
pub async fn seed_dev_key(db: &PgPool, raw: &str) -> Result<(), String> {
    let prefix = key_prefix(raw).ok_or("DEV_API_KEY must look like pnx_live_<8 alnum>_<secret>")?;
    sqlx::query(
        "WITH b AS (
             INSERT INTO businesses (name)
             SELECT 'Dev Business (DEV ONLY)'
             WHERE NOT EXISTS (SELECT 1 FROM api_keys WHERE key_prefix = $1)
             RETURNING id
         )
         INSERT INTO api_keys (business_id, key_prefix, key_hash) SELECT id, $1, $2 FROM b",
    )
    .bind(prefix)
    .bind(hash_key(raw))
    .execute(db)
    .await
    .map_err(|e| format!("seeding dev key failed: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::key_prefix;

    #[test]
    fn parses_well_formed_keys_only() {
        assert_eq!(key_prefix("pnx_live_abcd1234_secret"), Some("abcd1234"));
        assert_eq!(key_prefix("pnx_live_abcd1234_"), None);
        assert_eq!(key_prefix("pnx_live_abcd123_secret"), None);
        assert_eq!(key_prefix("pnx_test_abcd1234_secret"), None);
        assert_eq!(key_prefix("pnx_live_abcd-234_secret"), None);
    }
}
