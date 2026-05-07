//! Frankweiler HTTP server entrypoint.

use frankweiler_core::config::{default_config_path, load_config, BackendConfig};
use frankweiler_http::router;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bind = match load_config(Some(&default_config_path())) {
        Ok(cfg) => cfg.backend.bind,
        Err(e) => {
            eprintln!("note: using default bind (config error: {e})");
            BackendConfig::default().bind
        }
    };
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!("frankweiler-http listening on http://{}", listener.local_addr()?);
    axum::serve(listener, router()).await?;
    Ok(())
}
