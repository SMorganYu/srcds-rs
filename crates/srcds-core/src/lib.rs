//! srcds-rs runtime: configuration, server state, the tick loop and connectionless
//! query handling.
//!
//! Prototype architecture (see `docs/ARCHITECTURE.md`): a single simulation thread
//! owns all state and alternates between draining the UDP socket and running fixed
//! ticks. No locks are needed; shutdown is signalled through an atomic flag.

pub mod bsp;
pub mod client;
pub mod config;
pub mod metrics;
pub mod query;
pub mod server;
pub mod signon;
pub mod steaminf;

pub use config::ServerConfig;
pub use metrics::{Metrics, MetricsSnapshot};
pub use server::{Server, ShutdownHandle};
