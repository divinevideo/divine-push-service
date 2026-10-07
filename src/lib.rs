//! diVine Push Service Library Crate

// Clippy's `double_must_use` fires on `#[async_trait]` trait methods that return
// `Result`, attributing a `#[must_use]` to the macro-expanded method. There is
// no `#[must_use]` in the source to remove, so allow the lint crate-wide.
#![allow(clippy::double_must_use)]

// Declare modules as public to be accessible from the binary crate and integration tests
pub mod campaign_delivery;
pub mod cleanup_service;
pub mod coalesce;
pub mod config;
pub mod crypto;
pub mod error;
pub mod event_handler;
pub mod fcm_sender;
pub mod health;
pub mod metrics;
pub mod models;
pub mod nostr_listener;
pub mod preferences;
pub mod redis_store;
pub mod roster_publisher;
pub mod server;
pub mod services;
pub mod state;
