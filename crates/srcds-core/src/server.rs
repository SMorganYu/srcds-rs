//! Server main loop.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use srcds_net::{RecvOutcome, UdpTransport};
use srcds_protocol::connectionless::{classify, Framing};
use tracing::{debug, info, warn};

use crate::config::ServerConfig;
use crate::metrics::{Metrics, MetricsSnapshot};
use crate::query::{QueryHandler, QueryState};

/// Cloneable handle that asks the server loop to stop.
#[derive(Debug, Clone, Default)]
pub struct ShutdownHandle(Arc<AtomicBool>);

impl ShutdownHandle {
    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

pub struct Server {
    cfg: ServerConfig,
    transport: UdpTransport,
    query: QueryHandler,
    state: QueryState,
    metrics: Metrics,
    shutdown: ShutdownHandle,
}

impl Server {
    pub fn bind(cfg: ServerConfig) -> io::Result<Self> {
        let transport = UdpTransport::bind(cfg.bind_addr())?;
        let now = Instant::now();
        Ok(Self {
            query: QueryHandler::new(&cfg, now),
            state: QueryState::from_config(&cfg),
            transport,
            metrics: Metrics::default(),
            shutdown: ShutdownHandle::default(),
            cfg,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.transport.local_addr()
    }

    pub fn shutdown_handle(&self) -> ShutdownHandle {
        self.shutdown.clone()
    }

    pub fn metrics(&self) -> MetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Runs until shutdown is requested. Single-threaded: network input is drained
    /// between fixed-interval ticks, so all state mutation happens on this thread
    /// in a deterministic order (packets received before tick N are processed
    /// before tick N runs).
    pub fn run(&mut self) -> io::Result<MetricsSnapshot> {
        let interval = self.cfg.tick_interval;
        let mut next_tick = Instant::now() + interval;
        let mut next_report = self.cfg.metrics_log_interval.map(|d| Instant::now() + d);
        info!(addr = %self.local_addr()?, tick_ms = interval.as_secs_f64() * 1e3, "server loop started");

        while !self.shutdown.is_requested() {
            let now = Instant::now();
            if now >= next_tick {
                let started = Instant::now();
                self.tick();
                let took = started.elapsed();
                self.metrics.record_tick(took);
                next_tick += interval;
                // If we fell far behind (e.g. process was suspended), resynchronise
                // instead of running a burst of catch-up ticks.
                if Instant::now() > next_tick + interval * 4 {
                    self.metrics.counters.tick_overruns += 1;
                    next_tick = Instant::now() + interval;
                }
                if let Some(at) = next_report {
                    if now >= at {
                        self.log_metrics();
                        next_report = self.cfg.metrics_log_interval.map(|d| now + d);
                    }
                }
                continue;
            }
            self.poll_network(next_tick - now)?;
        }
        info!("shutdown requested; leaving server loop");
        self.log_metrics();
        Ok(self.metrics.snapshot())
    }

    fn tick(&mut self) {
        // Game simulation goes here (M4+). Intentionally empty: there is no world
        // or entity state yet, and we do not pretend otherwise.
    }

    fn poll_network(&mut self, timeout: std::time::Duration) -> io::Result<()> {
        let (from, data) = match self.transport.recv(timeout)? {
            RecvOutcome::Idle => return Ok(()),
            RecvOutcome::Datagram { from, data } => (from, data),
        };
        let c = &mut self.metrics.counters;
        c.packets_in += 1;
        c.bytes_in += data.len() as u64;
        let responses = match classify(data) {
            Ok(Framing::Connectionless(payload)) => {
                self.query
                    .handle(from, payload, &self.state, Instant::now(), c)
            }
            Ok(Framing::Split(_)) => {
                // Clients do not send split connectionless packets to servers in
                // the query protocol; reassembly for client traffic is M2 work.
                c.split_dropped += 1;
                Vec::new()
            }
            Ok(Framing::Netchannel(_)) => {
                c.netchannel_dropped += 1;
                debug!(%from, len = data.len(), "netchannel packet without connection; dropped");
                Vec::new()
            }
            Err(_) => {
                c.malformed += 1;
                Vec::new()
            }
        };
        for r in responses {
            match self.transport.send_to(&r, from) {
                Ok(n) => {
                    let c = &mut self.metrics.counters;
                    c.packets_out += 1;
                    c.bytes_out += n as u64;
                }
                Err(e) => {
                    self.metrics.counters.send_errors += 1;
                    warn!(%from, error = %e, "send failed");
                }
            }
        }
        Ok(())
    }

    fn log_metrics(&self) {
        let s = self.metrics.snapshot();
        let c = &s.counters;
        info!(
            ticks = c.ticks,
            pkts_in = c.packets_in,
            pkts_out = c.packets_out,
            bytes_in = c.bytes_in,
            bytes_out = c.bytes_out,
            malformed = c.malformed,
            rate_limited = c.rate_limited,
            q_info = c.queries_info,
            q_player = c.queries_player,
            q_rules = c.queries_rules,
            unhandled = c.unhandled_connectionless,
            tick_p50_us = s.tick_p50.as_micros() as u64,
            tick_p99_us = s.tick_p99.as_micros() as u64,
            "metrics"
        );
    }
}
