//! diVine Push Service
//!
//! Push notification service for the diVine mobile app.
//! Listens to Nostr events and sends FCM notifications.

use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use divine_push_service::cleanup_service;
use divine_push_service::config;
use divine_push_service::error::Result;
use divine_push_service::event_handler;
use divine_push_service::health::{CriticalTask, OnReturn, TaskHealth, TaskTracker};
use divine_push_service::metrics;
use divine_push_service::nostr_listener;
use divine_push_service::server::run_server;
use divine_push_service::state;

use nostr_sdk::prelude::Event;

/// Resolves when the process is asked to stop.
///
/// `ctrl_c` alone covers SIGINT, which is what a local run sends. Kubernetes
/// stops a pod with SIGTERM, and an unhandled SIGTERM terminates the process
/// outright — so the shutdown path below it, and the non-zero exit it reports
/// after a task died, would never run in the one place they exist for.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => tracing::info!("Received SIGINT"),
                    _ = sigterm.recv() => tracing::info!("Received SIGTERM"),
                }
                return;
            }
            Err(e) => {
                tracing::error!(
                    "Failed to install the SIGTERM handler: {}. Falling back to SIGINT only.",
                    e
                );
            }
        }
    }

    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("Received SIGINT");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(true)
        .init();

    tracing::info!("Starting diVine Push Service...");

    let metrics_handle = metrics::init();

    let settings = config::Settings::new()?;
    tracing::info!("Configuration loaded successfully");

    let app_state = Arc::new(state::AppState::new(settings).await?);
    tracing::info!("Application state initialized (Redis Pool, FCM Client)");

    let token = CancellationToken::new();
    let health = Arc::new(TaskHealth::new());
    let mut tracker = TaskTracker::new(Arc::clone(&health), token.clone());

    let (nostr_event_tx, nostr_event_rx) =
        tokio::sync::mpsc::channel::<(Box<Event>, event_handler::EventContext)>(1000);

    // Start Nostr listener
    let state_nostr = Arc::clone(&app_state);
    let token_nostr = token.clone();
    let tx_nostr = nostr_event_tx.clone();
    tracker.spawn(
        "nostr_listener",
        Some(CriticalTask::NostrListener),
        OnReturn::Fatal,
        async move {
            let listener = nostr_listener::NostrListener::new(state_nostr);
            if let Err(e) = listener.run(tx_nostr, token_nostr).await {
                tracing::error!("Nostr listener failed: {}", e);
            }
            tracing::info!("Nostr listener task finished.");
        },
    );
    tracing::info!("Nostr listener started");

    // Start event handler
    let state_event = Arc::clone(&app_state);
    let token_event = token.clone();
    tracker.spawn(
        "event_handler",
        Some(CriticalTask::EventHandler),
        OnReturn::Fatal,
        async move {
            if let Err(e) = event_handler::run(state_event, nostr_event_rx, token_event).await {
                tracing::error!("Event handler failed: {}", e);
            }
            tracing::info!("Event handler task finished.");
        },
    );
    tracing::info!("Event handler started");

    // Start durable new-post fan-out worker
    let state_fanout = Arc::clone(&app_state);
    let token_fanout = token.clone();
    tracker.spawn(
        "new_post_fanout",
        Some(CriticalTask::NewPostFanout),
        OnReturn::Fatal,
        async move {
            if let Err(e) = event_handler::run_new_post_fanout(state_fanout, token_fanout).await {
                tracing::error!("New-post fan-out worker failed: {}", e);
            }
            tracing::info!("New-post fan-out worker task finished.");
        },
    );
    tracing::info!("New-post fan-out worker started");

    // Start cleanup service
    let state_cleanup = Arc::clone(&app_state);
    let token_cleanup = token.clone();
    tracker.spawn("cleanup_service", None, OnReturn::Tolerated, async move {
        if let Err(e) = cleanup_service::run_cleanup_service(state_cleanup, token_cleanup).await {
            tracing::error!("Cleanup service failed: {}", e);
        }
        tracing::info!("Cleanup service task finished.");
    });
    tracing::info!("Cleanup service started");

    // Start HTTP server
    let token_server = token.clone();
    let state_server = Arc::clone(&app_state);
    let health_server = Arc::clone(&health);
    tracker.spawn("http_server", None, OnReturn::Fatal, async move {
        run_server(state_server, health_server, metrics_handle, token_server).await;
        tracing::info!("HTTP server task finished.");
    });
    tracing::info!("HTTP server started");

    // Create a future for token cancellation
    let token_cancelled = token.child_token();

    // Wait for shutdown signal
    tokio::select! {
        _ = shutdown_signal() => {
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

    if health.had_unexpected_exit() {
        tracing::error!(
            "diVine Push Service stopped after a task terminated unexpectedly - exiting non-zero."
        );
        std::process::exit(1);
    }

    tracing::info!("diVine Push Service stopped.");
    Ok(())
}
