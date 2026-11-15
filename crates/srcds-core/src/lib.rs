//! srcds-rs runtime: configuration, server state, the tick loop and connectionless
//! query handling.
//!
//! Prototype architecture (see `docs/ARCHITECTURE.md`): a single simulation thread
//! owns all state and alternates between draining the UDP socket and running fixed
//! ticks. No locks are needed; shutdown is signalled through an atomic flag.

pub mod bsp;
pub mod client;
pub mod clientlua;
pub mod collision;
pub mod config;
pub mod displacement;
pub mod entities;
pub mod game_lua;
pub mod gameevents;
pub mod metrics;
pub mod movement;
pub mod physics;
pub mod query;
pub mod server;
pub mod signon;
pub mod status;
pub mod steaminf;
pub mod vpk;
pub mod weapon_sim;
pub mod weapons;

pub use config::ServerConfig;
pub use metrics::{Metrics, MetricsSnapshot};
pub use server::{Server, ShutdownHandle};
