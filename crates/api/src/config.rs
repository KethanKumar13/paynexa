use std::{net::SocketAddr, time::Duration};

pub struct Config {
    pub database_url: String,
    pub bind_addr: SocketAddr,
    pub dev_api_key: Option<String>,
    pub psp_url: String,
    pub psp_connect_timeout: Duration,
    pub psp_timeout: Duration,
    pub payments: PaymentSettings,
}

#[derive(Clone, Debug)]
pub struct PaymentSettings {
    pub sweeper_interval: Duration,
    /// Attempts younger than this are left to the request that created them.
    pub sweeper_min_age: Duration,
    /// A PSP 404 only means "never received" once the attempt is at least this old.
    pub not_found_grace: Duration,
    pub alert_age: Duration,
}

impl Default for PaymentSettings {
    fn default() -> Self {
        Self {
            sweeper_interval: Duration::from_secs(10),
            sweeper_min_age: Duration::from_secs(15),
            not_found_grace: Duration::from_secs(60),
            alert_age: Duration::from_secs(600),
        }
    }
}

fn env_ms(name: &str, default: Duration) -> Result<Duration, String> {
    match std::env::var(name) {
        Ok(raw) => raw
            .parse()
            .map(Duration::from_millis)
            .map_err(|e| format!("invalid {name}: {e}")),
        Err(_) => Ok(default),
    }
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let database_url =
            std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL must be set".to_string())?;
        let bind_addr = std::env::var("BIND_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8080".into())
            .parse()
            .map_err(|e| format!("invalid BIND_ADDR: {e}"))?;
        let dev_api_key = std::env::var("DEV_API_KEY").ok().filter(|k| !k.is_empty());
        let psp_url = std::env::var("PSP_URL").unwrap_or_else(|_| "http://127.0.0.1:8089".into());
        let defaults = PaymentSettings::default();
        Ok(Self {
            database_url,
            bind_addr,
            dev_api_key,
            psp_url,
            psp_connect_timeout: env_ms("PSP_CONNECT_TIMEOUT_MS", Duration::from_secs(2))?,
            psp_timeout: env_ms("PSP_TIMEOUT_MS", Duration::from_secs(5))?,
            payments: PaymentSettings {
                sweeper_interval: env_ms("SWEEPER_INTERVAL_MS", defaults.sweeper_interval)?,
                sweeper_min_age: env_ms("SWEEPER_MIN_AGE_MS", defaults.sweeper_min_age)?,
                not_found_grace: env_ms("PSP_NOT_FOUND_GRACE_MS", defaults.not_found_grace)?,
                alert_age: env_ms("SWEEPER_ALERT_AGE_MS", defaults.alert_age)?,
            },
        })
    }
}
