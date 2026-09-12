mod config;
mod domain;
mod file;
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
    let file = file::KeaFile::load(&cfg.kea_config)?;
    let state = web::AppState {
        kea_path: cfg.kea_config,
        backup_keep: cfg.backup_keep,
        file,
        saves_since_apply: 0,
    };
    let app = web::app(state).into_make_service_with_connect_info::<SocketAddr>();
    let addr = format!("{}:{}", cfg.bind, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("Kealight 監聽 http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
