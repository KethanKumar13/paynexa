use std::{net::SocketAddr, time::Duration};

pub struct Config {
    pub database_url: String,
    pub bind_addr: SocketAddr,
    pub dev_api_key: Option<String>,
    pub psp_url: String,
    pub psp_connect_timeout: Duration,
    pub psp_timeout: Duration,
    pub payments: PaymentSettings,
    pub webhooks: WebhookSettings,
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

#[derive(Clone, Debug)]
pub struct WebhookSettings {
    pub poll_interval: Duration,
    pub timeout: Duration,
    /// How long a claimed event is hidden from other dispatcher runs while it is delivered.
    pub lease: Duration,
    /// Delay before each retry; the first delivery is immediate.
    pub retry_schedule: Vec<Duration>,
}

impl WebhookSettings {
    pub fn max_attempts(&self) -> i32 {
        i32::try_from(self.retry_schedule.len())
            .unwrap_or(i32::MAX - 1)
            .saturating_add(1)
    }

    /// A lease that can expire mid-delivery would let a second run send the same event concurrently.
    pub fn validate(&self) -> Result<(), String> {
        if self.lease <= self.timeout {
            return Err("WEBHOOK_LEASE_MS must be greater than WEBHOOK_TIMEOUT_MS".into());
        }
        Ok(())
    }
}

impl Default for WebhookSettings {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(1),
            timeout: Duration::from_secs(5),
            lease: Duration::from_secs(30),
            retry_schedule: [60, 300, 900, 3600, 21_600]
                .into_iter()
                .map(Duration::from_secs)
                .collect(),
        }
    }
}

fn env_ms_list(name: &str, default: &[Duration]) -> Result<Vec<Duration>, String> {
    match std::env::var(name) {
        Ok(raw) => raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse()
                    .map(Duration::from_millis)
                    .map_err(|e| format!("invalid {name}: {e}"))
            })
            .collect(),
        Err(_) => Ok(default.to_vec()),
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
        let webhook_defaults = WebhookSettings::default();
        let webhooks = WebhookSettings {
            poll_interval: env_ms("WEBHOOK_POLL_INTERVAL_MS", webhook_defaults.poll_interval)?,
            timeout: env_ms("WEBHOOK_TIMEOUT_MS", webhook_defaults.timeout)?,
            lease: env_ms("WEBHOOK_LEASE_MS", webhook_defaults.lease)?,
            retry_schedule: env_ms_list(
                "WEBHOOK_RETRY_SCHEDULE_MS",
                &webhook_defaults.retry_schedule,
            )?,
        };
        webhooks.validate()?;
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
            webhooks,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::WebhookSettings;

    fn with_lease(lease: Duration) -> WebhookSettings {
        WebhookSettings {
            lease,
            ..WebhookSettings::default()
        }
    }

    #[test]
    fn defaults_are_valid() {
        assert!(WebhookSettings::default().validate().is_ok());
    }

    #[test]
    fn lease_must_outlast_the_delivery_timeout() {
        let timeout = WebhookSettings::default().timeout;
        assert!(with_lease(timeout).validate().is_err());
        assert!(with_lease(timeout / 2).validate().is_err());
    }
}
