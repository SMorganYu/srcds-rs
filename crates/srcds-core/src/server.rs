//! Server main loop.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use srcds_net::challenge::ChallengeIssuer;
use srcds_net::ratelimit::PerIpLimiter;
use srcds_net::{RecvOutcome, UdpTransport};
use srcds_protocol::connectionless::{classify, Framing};
use srcds_protocol::handshake::{self, ChallengeResponse, ConnectRequest, GetChallenge};
use tracing::{debug, info, warn};

use crate::bsp;
use crate::client::Client;
use crate::config::ServerConfig;
use crate::metrics::{Metrics, MetricsSnapshot};
use crate::query::{QueryHandler, QueryState};
use crate::signon::GameInfo;

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
    game: Option<GameInfo>,
    clients: HashMap<SocketAddr, Client>,
    conn_challenge: ChallengeIssuer,
    conn_limiter: PerIpLimiter,
    tick_count: i32,
    next_user_id: i32,
}

/// True for addresses a no-authentication (LAN) server may accept, mirroring the
/// engine's `sv_lan` policy of loopback and private networks only.
pub fn is_lan_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_loopback() || v.is_private() || v.is_link_local(),
        IpAddr::V6(v) => v.is_loopback(),
    }
}

impl Server {
    pub fn bind(cfg: ServerConfig) -> io::Result<Self> {
        let transport = UdpTransport::bind(cfg.bind_addr())?;
        let now = Instant::now();
        let game = match &cfg.game_dir {
            Some(dir) => match bsp::load(dir, &cfg.map) {
                Ok(map) => {
                    info!(
                        map = %map.name,
                        brush_models = map.brush_models,
                        sky = %map.sky_name,
                        md5 = %hex(&map.md5),
                        "map loaded"
                    );
                    Some(GameInfo {
                        map,
                        host_name: cfg.hostname.clone(),
                        game_dir: cfg.game_folder.clone(),
                        gamemode: cfg.gamemode.clone(),
                        max_clients: cfg.max_players,
                        tick_interval: cfg.tick_interval.as_secs_f32(),
                        server_count: 1,
                        query_port: cfg.port,
                        client_lua_files: Vec::new(),
                        full_signon: cfg.experimental_full_signon,
                    })
                }
                Err(e) => {
                    warn!(dir = %dir.display(), map = %cfg.map, error = %e, "could not load map; game clients will be refused");
                    None
                }
            },
            None => {
                warn!("no --game-dir given; game clients will be refused (queries still work)");
                None
            }
        };
        Ok(Self {
            query: QueryHandler::new(&cfg, now),
            state: QueryState::from_config(&cfg),
            transport,
            metrics: Metrics::default(),
            shutdown: ShutdownHandle::default(),
            game,
            clients: HashMap::new(),
            conn_challenge: ChallengeIssuer::new(Duration::from_secs(30)),
            conn_limiter: PerIpLimiter::new(4096, 20, 10),
            tick_count: 0,
            next_user_id: 2,
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
        // No world simulation yet (M4+); the tick drives client traffic only.
        self.tick_count = self.tick_count.wrapping_add(1);
        let now = Instant::now();
        let Some(game) = self.game.as_ref() else {
            return;
        };
        let mut gone = Vec::new();
        let mut out: Vec<(Vec<u8>, SocketAddr)> = Vec::new();
        for (addr, c) in self.clients.iter_mut() {
            if let Some(reason) = c.drop_reason.clone() {
                if !reason.is_empty() {
                    if let Some(d) = c.disconnect_datagram(&reason) {
                        out.push((d, *addr));
                    }
                }
                gone.push(*addr);
                continue;
            }
            if c.timed_out(now) {
                info!(%addr, name = %c.name, "client timed out");
                gone.push(*addr);
                continue;
            }
            for d in c.transmit(game, self.tick_count) {
                out.push((d, *addr));
            }
        }
        for a in gone {
            if let Some(c) = self.clients.remove(&a) {
                info!(addr = %a, name = %c.name, "client removed");
            }
        }
        for (d, to) in out {
            self.send(&d, to);
        }
        self.state.players = self
            .clients
            .values()
            .map(|c| srcds_protocol::a2s::PlayerEntry {
                index: c.slot,
                name: c.name.clone(),
                score: 0,
                duration: now.duration_since(c.connected_at).as_secs_f32(),
            })
            .collect();
    }

    fn send(&mut self, d: &[u8], to: SocketAddr) {
        match self.transport.send_to(d, to) {
            Ok(n) => {
                let c = &mut self.metrics.counters;
                c.packets_out += 1;
                c.bytes_out += n as u64;
            }
            Err(e) => {
                self.metrics.counters.send_errors += 1;
                warn!(%to, error = %e, "send failed");
            }
        }
    }

    /// Handles `A2S_GETCHALLENGE` / `C2S_CONNECT`; returns the response datagram.
    fn handshake(
        &mut self,
        from: SocketAddr,
        kind: u8,
        body: &[u8],
        now: Instant,
    ) -> Option<Vec<u8>> {
        if !self.conn_limiter.allow(from.ip(), now) {
            self.metrics.counters.rate_limited += 1;
            return None;
        }
        match kind {
            handshake::A2S_GETCHALLENGE => {
                let req = GetChallenge::parse(body).ok()?;
                let ch = self.conn_challenge.issue(&from, now) as u32;
                debug!(%from, "connection challenge issued");
                ChallengeResponse {
                    server_challenge: ch,
                    client_challenge: req.client_challenge,
                    auth_protocol: handshake::AUTH_PROTOCOL_STEAM,
                    server_steam_id: 1,
                    secure: false,
                }
                .encode()
                .ok()
            }
            handshake::C2S_CONNECT => {
                let req = match ConnectRequest::parse(body) {
                    Ok(r) => r,
                    Err(e) => {
                        debug!(%from, error = %e, "malformed connect");
                        self.metrics.counters.malformed += 1;
                        return None;
                    }
                };
                let client_challenge = req.client_challenge;
                let reject = |reason: &str| {
                    info!(%from, %reason, "connection refused");
                    handshake::encode_reject(client_challenge, reason).ok()
                };
                if req.protocol != handshake::PROTOCOL_VERSION {
                    return reject(&format!(
                        "Server uses protocol version {}, client uses {}.",
                        handshake::PROTOCOL_VERSION,
                        req.protocol
                    ));
                }
                if !self
                    .conn_challenge
                    .verify(&from, req.server_challenge as i32, now)
                {
                    return reject("Bad challenge.");
                }
                if !is_lan_address(from.ip()) {
                    return reject("This srcds-rs build has no Steam authentication and only accepts LAN clients.");
                }
                if let Some(pw) = &self.cfg.password {
                    if &req.password != pw {
                        return reject("Bad password.");
                    }
                }
                if self.game.is_none() {
                    return reject(
                        "Server has no map data loaded (start srcds-rs with --game-dir).",
                    );
                }
                // A reconnect from the same address replaces the old connection.
                self.clients.remove(&from);
                let used: Vec<u8> = self.clients.values().map(|c| c.slot).collect();
                let Some(slot) = (0..self.cfg.max_players).find(|s| !used.contains(s)) else {
                    return reject("Server is full.");
                };
                let user_id = self.next_user_id;
                self.next_user_id += 1;
                info!(
                    %from,
                    name = %req.name,
                    slot,
                    user_id,
                    product = %req.product_version,
                    steam_ticket_bytes = req.steam_cookie.len(),
                    "client accepted (LAN mode: Steam ticket not validated)"
                );
                let client = Client::new(
                    from,
                    slot,
                    user_id,
                    req.name.clone(),
                    req.claimed_steam_id(),
                    req.server_challenge,
                    now,
                );
                self.clients.insert(from, client);
                handshake::encode_connection(client_challenge).ok()
            }
            _ => None,
        }
    }

    fn poll_network(&mut self, timeout: std::time::Duration) -> io::Result<()> {
        let (from, data) = match self.transport.recv(timeout)? {
            RecvOutcome::Idle => return Ok(()),
            RecvOutcome::Datagram { from, data } => (from, data),
        };
        let c = &mut self.metrics.counters;
        c.packets_in += 1;
        c.bytes_in += data.len() as u64;
        let now = Instant::now();
        let data = data.to_vec();
        let mut responses = Vec::new();
        match classify(&data) {
            Ok(Framing::Connectionless(payload)) => {
                let kind = payload.first().copied();
                if let Some(k @ (handshake::A2S_GETCHALLENGE | handshake::C2S_CONNECT)) = kind {
                    if let Some(r) = self.handshake(from, k, &payload[1..], now) {
                        responses.push(r);
                    }
                } else {
                    responses = self.query.handle(from, payload, &self.state, now, c);
                }
            }
            Ok(Framing::Split(_)) | Ok(Framing::Netchannel(_)) => {
                match (self.clients.get_mut(&from), self.game.as_ref()) {
                    (Some(client), Some(game)) => {
                        let players: Vec<(u8, Vec<u8>)> = vec![(client.slot, client.player_info())];
                        client.on_datagram(&data, game, self.tick_count, &players, now);
                    }
                    _ => {
                        let c = &mut self.metrics.counters;
                        c.netchannel_dropped += 1;
                        debug!(%from, len = data.len(), "netchannel packet without connection; dropped");
                    }
                }
            }
            Err(_) => {
                c.malformed += 1;
            }
        }
        for r in responses {
            self.send(&r, from);
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

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
