//! Connectionless query handling (A2S_INFO / A2S_PLAYER / A2S_RULES).
//!
//! Only values that reflect the real state of this process are reported: the
//! player list is empty because srcds-rs does not accept game clients yet, VAC is
//! reported off, and no Steam ID is advertised because there is no Steam login.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use srcds_net::{ChallengeIssuer, PerIpLimiter, TokenBucket};
use srcds_protocol::a2s::{self, InfoResponse, PlayerResponse, RulesResponse};
use srcds_protocol::bytes::ByteWriter;
use srcds_protocol::connectionless::{parse_connectionless, Connectionless, QueryRequest};
use srcds_protocol::split::{self, DEFAULT_SPLIT_PAYLOAD, MAX_FRAGMENTS};
use srcds_protocol::MAX_ROUTABLE_PAYLOAD;
use tracing::{debug, trace};

use crate::config::ServerConfig;
use crate::metrics::Counters;

/// A2S_INFO "protocol" byte. VERIFIED: the reference GMod dedicated server
/// (PatchVersion 2026.04.29, garrysmod.ver 261104) reports 17
/// (fixture `tests/fixtures/a2s/ref_gmod_2026.04.29/info_1.bin`).
pub const A2S_PROTOCOL_VERSION: u8 = 17;

/// Server state visible to queries. Owned by the simulation thread.
#[derive(Debug, Clone)]
pub struct QueryState {
    pub hostname: String,
    pub map: String,
    pub game_folder: String,
    pub game_description: String,
    pub app_id: u32,
    pub version: String,
    /// First line of `garrysmod.ver`, when known; reported as `ver:` keyword.
    pub gmod_build: Option<u32>,
    pub port: u16,
    pub max_players: u8,
    pub password_protected: bool,
    pub players: Vec<a2s::PlayerEntry>,
    /// Kept sorted by name (the reference server reports rules alphabetically).
    pub rules: Vec<(String, String)>,
}

impl QueryState {
    pub fn from_config(c: &ServerConfig) -> Self {
        let password_protected = c.password.as_deref().is_some_and(|p| !p.is_empty());
        let mut s = Self {
            hostname: c.hostname.clone(),
            map: c.map.clone(),
            game_folder: c.game_folder.clone(),
            game_description: c.game_description.clone(),
            app_id: c.app_id,
            version: c.version.clone(),
            gmod_build: c.gmod_build,
            port: c.port,
            max_players: c.max_players,
            password_protected,
            players: Vec::new(),
            rules: vec![
                // FCVAR_PROTECTED-style reporting: "1"/"0", never the value.
                (
                    "sv_password".into(),
                    if password_protected { "1" } else { "0" }.into(),
                ),
                ("srcds_rs_version".into(), env!("CARGO_PKG_VERSION").into()),
            ],
        };
        s.rules.sort_by(|a, b| a2s::rule_name_cmp(&a.0, &b.0));
        s
    }

    /// Keyword string. The reference server sends `" gm:<gamemode> gmc:<category>
    /// ver:<build>"` (note the leading space). srcds-rs runs no gamemode, so `gm:`
    /// is omitted rather than invented; `ver:` is only sent when read from the
    /// installed game files.
    pub fn keywords(&self) -> String {
        let mut k = String::from(" gmc:other");
        if let Some(v) = self.gmod_build {
            k.push_str(&format!(" ver:{v}"));
        }
        k.push_str(" srcds-rs");
        k
    }

    pub fn info(&self) -> InfoResponse {
        InfoResponse {
            protocol: A2S_PROTOCOL_VERSION,
            name: self.hostname.clone(),
            map: self.map.clone(),
            folder: self.game_folder.clone(),
            game: self.game_description.clone(),
            app_id: (self.app_id & 0xFFFF) as u16,
            players: self.players.len().min(255) as u8,
            max_players: self.max_players,
            bots: 0,
            server_type: b'd',
            environment: if cfg!(windows) {
                b'w'
            } else if cfg!(target_os = "macos") {
                b'm'
            } else {
                b'l'
            },
            visibility: self.password_protected as u8,
            vac: 0,
            version: self.version.clone(),
            port: Some(self.port),
            steam_id: None,
            source_tv: None,
            keywords: Some(self.keywords()),
            game_id: Some(self.app_id as u64),
        }
    }
}

pub struct QueryHandler {
    challenges: ChallengeIssuer,
    global: TokenBucket,
    per_ip: PerIpLimiter,
    next_split_id: i32,
}

/// Responses produced for one request (usually 0 or 1 datagrams).
pub type Responses = Vec<Vec<u8>>;

impl QueryHandler {
    pub fn new(cfg: &ServerConfig, now: Instant) -> Self {
        Self {
            challenges: ChallengeIssuer::new(Duration::from_secs(30)),
            global: TokenBucket::new(cfg.query_rate_global, cfg.query_rate_global, now),
            per_ip: PerIpLimiter::new(16_384, cfg.query_rate_per_ip, cfg.query_rate_per_ip),
            next_split_id: 1,
        }
    }

    #[cfg(test)]
    pub fn with_issuer(mut self, issuer: ChallengeIssuer) -> Self {
        self.challenges = issuer;
        self
    }

    /// Handles a connectionless payload (bytes after the `-1` header).
    pub fn handle(
        &mut self,
        from: SocketAddr,
        payload: &[u8],
        state: &QueryState,
        now: Instant,
        c: &mut Counters,
    ) -> Responses {
        let req = match parse_connectionless(payload) {
            Ok(Connectionless::Query(q)) => q,
            Ok(Connectionless::Unhandled { kind, body }) => {
                c.unhandled_connectionless += 1;
                debug!(%from, kind = %char::from(kind), len = body.len(), "unhandled connectionless");
                return Vec::new();
            }
            Err(e) => {
                c.malformed += 1;
                trace!(%from, error = %e, "malformed connectionless");
                return Vec::new();
            }
        };
        if !self.per_ip.allow(from.ip(), now) || !self.global.try_take(now) {
            c.rate_limited += 1;
            return Vec::new();
        }
        let result = match req {
            QueryRequest::Info { challenge } => {
                c.queries_info += 1;
                match challenge {
                    Some(ch) if self.challenges.verify(&from, ch, now) => {
                        self.single(|w| state.info().encode(w))
                    }
                    other => {
                        if other.is_some() {
                            c.bad_challenges += 1;
                        }
                        self.challenge(from, now, c)
                    }
                }
            }
            QueryRequest::Player { challenge } => {
                c.queries_player += 1;
                if self.challenges.verify(&from, challenge, now) {
                    let resp = PlayerResponse {
                        players: state.players.clone(),
                    };
                    self.maybe_split(|w| resp.encode(w))
                } else {
                    if challenge != -1 {
                        c.bad_challenges += 1;
                    }
                    self.challenge(from, now, c)
                }
            }
            QueryRequest::Rules { challenge } => {
                c.queries_rules += 1;
                if self.challenges.verify(&from, challenge, now) {
                    let resp = RulesResponse {
                        rules: state.rules.clone(),
                    };
                    self.maybe_split(|w| resp.encode(w))
                } else {
                    if challenge != -1 {
                        c.bad_challenges += 1;
                    }
                    self.challenge(from, now, c)
                }
            }
            QueryRequest::ServerQueryGetChallenge => self.challenge(from, now, c),
        };
        match result {
            Ok(r) => r,
            Err(e) => {
                debug!(%from, error = %e, "failed to encode query response");
                Vec::new()
            }
        }
    }

    fn challenge(
        &mut self,
        from: SocketAddr,
        now: Instant,
        c: &mut Counters,
    ) -> srcds_protocol::error::Result<Responses> {
        c.challenges_issued += 1;
        let ch = self.challenges.issue(&from, now);
        self.single(|w| a2s::encode_challenge(w, ch))
    }

    fn single(
        &mut self,
        f: impl FnOnce(&mut ByteWriter) -> srcds_protocol::error::Result<()>,
    ) -> srcds_protocol::error::Result<Responses> {
        let mut w = ByteWriter::with_capacity(MAX_ROUTABLE_PAYLOAD);
        f(&mut w)?;
        Ok(vec![w.into_inner()])
    }

    fn maybe_split(
        &mut self,
        f: impl FnOnce(&mut ByteWriter) -> srcds_protocol::error::Result<()>,
    ) -> srcds_protocol::error::Result<Responses> {
        let mut w = ByteWriter::with_capacity(MAX_FRAGMENTS * DEFAULT_SPLIT_PAYLOAD);
        f(&mut w)?;
        let data = w.into_inner();
        if data.len() <= MAX_ROUTABLE_PAYLOAD {
            return Ok(vec![data]);
        }
        let id = self.next_split_id;
        self.next_split_id = self.next_split_id.wrapping_add(1) & 0x7FFF_FFFF;
        split::split(&data, id, DEFAULT_SPLIT_PAYLOAD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use srcds_protocol::connectionless::INFO_QUERY_STRING;

    fn setup() -> (QueryHandler, QueryState, Counters, Instant) {
        let cfg = ServerConfig::default();
        let now = Instant::now();
        (
            QueryHandler::new(&cfg, now),
            QueryState::from_config(&cfg),
            Counters::default(),
            now,
        )
    }

    fn info_req(ch: Option<i32>) -> Vec<u8> {
        let mut p = vec![b'T'];
        p.extend_from_slice(INFO_QUERY_STRING);
        if let Some(ch) = ch {
            p.extend_from_slice(&ch.to_le_bytes());
        }
        p
    }

    #[test]
    fn info_challenge_flow() {
        let (mut h, st, mut c, now) = setup();
        let from: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        let r = h.handle(from, &info_req(None), &st, now, &mut c);
        assert_eq!(r.len(), 1);
        let ch = a2s::decode_challenge(&r[0][4..]).unwrap();
        let r = h.handle(from, &info_req(Some(ch)), &st, now, &mut c);
        let info = InfoResponse::decode(&r[0][4..]).unwrap();
        assert_eq!(info.map, "gm_construct");
        assert_eq!(info.players, 0);
        assert_eq!(info.app_id, 4000);
        // Wrong challenge -> new challenge, not info.
        let r = h.handle(from, &info_req(Some(ch ^ 1)), &st, now, &mut c);
        assert!(a2s::decode_challenge(&r[0][4..]).is_ok());
        assert_eq!(c.bad_challenges, 1);
    }

    #[test]
    fn player_and_rules_flow() {
        let (mut h, st, mut c, now) = setup();
        let from: SocketAddr = "127.0.0.1:40001".parse().unwrap();
        let r = h.handle(from, b"U\xff\xff\xff\xff", &st, now, &mut c);
        let ch = a2s::decode_challenge(&r[0][4..]).unwrap();
        let mut req = vec![b'U'];
        req.extend_from_slice(&ch.to_le_bytes());
        let r = h.handle(from, &req, &st, now, &mut c);
        assert!(PlayerResponse::decode(&r[0][4..])
            .unwrap()
            .players
            .is_empty());
        req[0] = b'V';
        let r = h.handle(from, &req, &st, now, &mut c);
        let rules = RulesResponse::decode(&r[0][4..]).unwrap();
        assert!(rules
            .rules
            .iter()
            .any(|(k, v)| k == "sv_password" && v == "0"));
    }

    #[test]
    fn big_rules_are_split() {
        let (mut h, mut st, mut c, now) = setup();
        st.rules = (0..300)
            .map(|i| (format!("cvar_{i:04}"), "x".repeat(20)))
            .collect();
        let from: SocketAddr = "127.0.0.1:40002".parse().unwrap();
        let ch =
            a2s::decode_challenge(&h.handle(from, b"V\xff\xff\xff\xff", &st, now, &mut c)[0][4..])
                .unwrap();
        let mut req = vec![b'V'];
        req.extend_from_slice(&ch.to_le_bytes());
        let frags = h.handle(from, &req, &st, now, &mut c);
        assert!(frags.len() > 1);
        assert!(frags.iter().all(|f| f.len() <= MAX_ROUTABLE_PAYLOAD));
        let mut ra = split::Reassembler::new();
        let mut full = None;
        for f in &frags {
            full = ra.push(f).unwrap();
        }
        let full = full.unwrap();
        assert_eq!(RulesResponse::decode(&full[4..]).unwrap().rules.len(), 300);
    }

    #[test]
    fn malformed_and_unhandled_produce_nothing() {
        let (mut h, st, mut c, now) = setup();
        let from: SocketAddr = "127.0.0.1:40003".parse().unwrap();
        assert!(h.handle(from, b"", &st, now, &mut c).is_empty());
        assert!(h.handle(from, b"TSource", &st, now, &mut c).is_empty());
        assert!(h.handle(from, b"qconnect", &st, now, &mut c).is_empty());
        assert_eq!(c.malformed, 2);
        assert_eq!(c.unhandled_connectionless, 1);
    }

    #[test]
    fn per_ip_rate_limit() {
        let (mut h, st, mut c, now) = setup();
        let from: SocketAddr = "127.0.0.1:40004".parse().unwrap();
        let mut answered = 0;
        for _ in 0..100 {
            if !h.handle(from, &info_req(None), &st, now, &mut c).is_empty() {
                answered += 1;
            }
        }
        assert_eq!(answered, ServerConfig::default().query_rate_per_ip);
        assert_eq!(c.rate_limited, 100 - answered as u64);
    }
}
