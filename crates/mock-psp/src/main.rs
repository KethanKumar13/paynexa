use std::{net::SocketAddr, time::Duration};

use mock_psp::{Config, app};
use tracing_subscriber::EnvFilter;

fn env_ms(name: &str, default: u64) -> Result<Duration, String> {
    match std::env::var(name) {
        Ok(raw) => raw
            .parse()
            .map(Duration::from_millis)
            .map_err(|e| format!("invalid {name}: {e}")),
        Err(_) => Ok(Duration::from_millis(default)),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let addr: SocketAddr = std::env::var("BIND_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8081".into())
        .parse()?;
    let config = Config {
        latency: env_ms("PSP_LATENCY_MS", 100)?,
        timeout_token_delay: env_ms("PSP_TIMEOUT_TOKEN_MS", 30_000)?,
    };

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, ?config, "mock PSP listening");
    axum::serve(listener, app(config))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut sig) = signal(SignalKind::terminate()) {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
