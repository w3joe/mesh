use mesh_server::{build_router, AppState, MeshEngine};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let host: IpAddr = std::env::var("HOST")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| IpAddr::from([0, 0, 0, 0]));
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7474);
    let addr = SocketAddr::new(host, port);
    let public_base_url = std::env::var("MESH_PUBLIC_URL")
        .unwrap_or_else(|_| format!("http://127.0.0.1:{port}"));

    let upstream_url = std::env::var("MESH_UPSTREAM_GIT_URL").ok();
    let engine = if let Ok(data_dir) = std::env::var("MESH_DATA_DIR") {
        tracing::info!("Initializing durable MeshEngine at data_dir={data_dir}");
        Arc::new(MeshEngine::with_data_dir(data_dir, upstream_url)?)
    } else {
        Arc::new(MeshEngine::new())
    };

    let state = AppState {
        engine,
        public_base_url,
    };

    let app = build_router(state);
    tracing::info!(
        "Mesh Server listening on http://{addr} (accessible from other devices on LAN/Tailscale on port {port})"
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
