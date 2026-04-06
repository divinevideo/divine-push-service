//! diVine Push Service
//!
//! Push notification service for the diVine mobile app.
//! Listens to Nostr events and sends FCM notifications.

use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use divine_push_service::cleanup_service;
use divine_push_service::config;
use divine_push_service::error::Result;
use divine_push_service::event_handler;
use divine_push_service::nostr_listener;
use divine_push_service::state;

use nostr_sdk::prelude::Event;

/// Simple task tracker for managing spawned tasks
struct SimpleTaskTracker {
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl SimpleTaskTracker {
    fn new() -> Self {
        Self {
            handles: Vec::new(),
        }
    }

    fn spawn<F>(&mut self, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.handles.push(tokio::spawn(future));
    }

    async fn wait(self) {
        for handle in self.handles {
            let _ = handle.await;
        }
    }
}

async fn health_check(
    State(app_state): State<Arc<state::AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let pubkey = app_state.service_pubkey_hex();
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "pubkey": pubkey,
        })),
    )
}

async fn run_server(app_state: Arc<state::AppState>, token: CancellationToken) {
    let app = Router::new()
        .route("/health", get(health_check))
        .with_state(app_state.clone());

    let listen_addr_str = &app_state.settings.server.listen_addr;
    let addr: SocketAddr = match listen_addr_str.parse() {
        Ok(addr) => addr,
        Err(e) => {
            tracing::error!(
                "Invalid server.listen_addr '{}': {}. Exiting server task.",
                listen_addr_str,
                e
            );
            token.cancel();
            return;
        }
    };

    tracing::info!("HTTP server listening on {}", addr);

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::error!("Failed to bind HTTP server: {}", e);
            tracing::error!("Cancelling token to trigger shutdown...");
            token.cancel();
            return;
        }
    };

    let shutdown_token = token.clone();
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_token.cancelled().await;
            tracing::info!("HTTP server shutting down.");
        })
        .await
    {
        tracing::error!("HTTP server error: {}", e);
        token.cancel();
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(true)
        .init();

    tracing::info!("Starting diVine Push Service...");

    let settings = config::Settings::new()?;
    tracing::info!("Configuration loaded successfully");

    let app_state = Arc::new(state::AppState::new(settings).await?);
    tracing::info!("Application state initialized (Redis Pool, FCM Client)");

    let mut tracker = SimpleTaskTracker::new();
    let token = CancellationToken::new();

    let (nostr_event_tx, nostr_event_rx) =
        tokio::sync::mpsc::channel::<(Box<Event>, event_handler::EventContext)>(1000);

    // Start Nostr listener
    let state_nostr = Arc::clone(&app_state);
    let token_nostr = token.clone();
    let tx_nostr = nostr_event_tx.clone();
    tracker.spawn(async move {
        let listener = nostr_listener::NostrListener::new(state_nostr);
        if let Err(e) = listener.run(tx_nostr, token_nostr).await {
            tracing::error!("Nostr listener failed: {}", e);
        }
        tracing::info!("Nostr listener task finished.");
    });
    tracing::info!("Nostr listener started");

    // Start event handler
    let state_event = Arc::clone(&app_state);
    let token_event = token.clone();
    tracker.spawn(async move {
        if let Err(e) = event_handler::run(state_event, nostr_event_rx, token_event).await {
            tracing::error!("Event handler failed: {}", e);
        }
        tracing::info!("Event handler task finished.");
    });
    tracing::info!("Event handler started");

    // Start cleanup service
    let state_cleanup = Arc::clone(&app_state);
    let token_cleanup = token.clone();
    tracker.spawn(async move {
        if let Err(e) = cleanup_service::run_cleanup_service(state_cleanup, token_cleanup).await {
            tracing::error!("Cleanup service failed: {}", e);
        }
        tracing::info!("Cleanup service task finished.");
    });
    tracing::info!("Cleanup service started");

    // Start HTTP server (health check only)
    let token_server = token.clone();
    let state_server = Arc::clone(&app_state);
    tracker.spawn(async move {
        run_server(state_server, token_server).await;
        tracing::info!("HTTP server task finished.");
    });
    tracing::info!("HTTP server started");

    // Create a future for token cancellation
    let token_cancelled = token.child_token();

    // Wait for shutdown signal
    tokio::select! {
        _ = signal::ctrl_c() => {
            tracing::info!("Received shutdown signal");
        }
        _ = token_cancelled.cancelled() => {
            tracing::info!("Shutdown triggered by task failure");
        }
    }

    tracing::info!("Shutting down services...");

    token.cancel();

    // Wait for all tracked tasks to complete
    tracker.wait().await;

    tracing::info!("diVine Push Service stopped.");
    Ok(())
}
