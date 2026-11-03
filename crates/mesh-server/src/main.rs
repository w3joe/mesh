use mesh_server::{build_router, AppState, MeshEngine};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7474);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let public_base_url = format!("http://{addr}");

    let state = AppState {
        engine: Arc::new(MeshEngine::new()),
        public_base_url: public_base_url.clone(),
    };

    let app = build_router(state);
    tracing::info!("Mesh Git Smart HTTP v2 + Express Cache + Semantic Merge Server listening on {public_base_url}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
