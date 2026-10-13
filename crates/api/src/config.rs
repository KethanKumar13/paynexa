use std::net::SocketAddr;

pub struct Config {
    pub database_url: String,
    pub bind_addr: SocketAddr,
    pub dev_api_key: Option<String>,
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
        Ok(Self {
            database_url,
            bind_addr,
            dev_api_key,
        })
    }
}
