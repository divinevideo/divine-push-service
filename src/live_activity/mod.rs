//! iOS Live Activity support: registration payload parsing and validation,
//! routing of Live Activity registrations, the Redis store for activities and
//! their schedule, and the worker that sends each activity's updates.

pub mod content;
pub mod payload;
pub mod registration;
pub mod stats_client;
pub mod store;
pub mod worker;
