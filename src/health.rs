//! Health check HTTP server.

use axum::{
    routing::get,
    Router,
    Json,
    extract::State,
};
use std::sync::Arc;
use std::net::SocketAddr;
use tracing::info;

use crate::chains::ChainManager;

pub struct HealthServer {
    port: u16,
    chain_manager: Arc<ChainManager>,
}

impl HealthServer {
    pub fn new(port: u16, chain_manager: Arc<ChainManager>) -> Self {
        Self { port, chain_manager }
    }
    
    pub async fn run(self) -> anyhow::Result<()> {
        let app = Router::new()
            .route("/", get(health_handler))
            .route("/health", get(health_handler))
            .route("/debug", get(debug_handler))
            .with_state(self.chain_manager);
        
        let addr = SocketAddr::from(([0, 0, 0, 0], self.port));
        let listener = tokio::net::TcpListener::bind(addr).await?;
        
        axum::serve(listener, app).await?;
        
        Ok(())
    }
}

async fn health_handler(
    State(chain_manager): State<Arc<ChainManager>>,
) -> Json<serde_json::Value> {
    let mut status = chain_manager.health_status();
    
    // Add uptime
    if let Some(obj) = status.as_object_mut() {
        obj.insert("timestamp".to_string(), serde_json::json!(chrono::Utc::now().to_rfc3339()));
    }
    
    Json(status)
}

async fn debug_handler(
    State(_chain_manager): State<Arc<ChainManager>>,
) -> Json<serde_json::Value> {
    // TODO: Return debug buffer
    Json(serde_json::json!({
        "message": "Debug endpoint",
        "entries": []
    }))
}
