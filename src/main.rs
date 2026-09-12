mod config;
mod domain;
mod file;
mod leases;
mod reload;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("kealight.toml"));
    let cfg = config::load(&config_path)?;
    let state = web::AppState::load(cfg.kea_config, cfg.backup_keep, cfg.password)?;
    let app = web::app(state).into_make_service_with_connect_info::<SocketAddr>();
    let addr = format!("{}:{}", cfg.bind, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("Kealight 監聽 http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
