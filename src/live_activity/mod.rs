//! iOS Live Activity support: registration payload parsing and validation,
//! routing of Live Activity registrations, and the Redis store for activities
//! and their schedule.

pub mod payload;
pub mod registration;
pub mod stats_client;
pub mod store;
