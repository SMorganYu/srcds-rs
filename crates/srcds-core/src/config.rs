use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

/// Static server configuration (from CLI today; config files later).
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub ip: IpAddr,
    pub port: u16,
    pub hostname: String,
    pub map: String,
    pub max_players: u8,
    /// Game folder reported in A2S_INFO.
    pub game_folder: String,
    /// Description reported in the A2S_INFO "game" field.
    pub game_description: String,
    /// Steam AppID (Garry's Mod = 4000).
    pub app_id: u32,
    /// Version string reported to query clients (normally `PatchVersion` from
    /// `steam.inf`).
    pub version: String,
    /// Build number from the first line of `garrysmod.ver`, if available.
    pub gmod_build: Option<u32>,
    pub password: Option<String>,
    pub tick_interval: Duration,
    pub metrics_log_interval: Option<Duration>,
    /// Global connectionless-response budget (packets/s) and per-IP budget.
    pub query_rate_global: u32,
    pub query_rate_per_ip: u32,
    /// Path to the `garrysmod` content directory (maps are read from `maps/`).
    /// Without it, game clients are refused with an explanatory message.
    pub game_dir: Option<PathBuf>,
    /// Gamemode name announced to clients.
    pub gamemode: String,
    /// Run the world signon (string tables, class info, entities) after ServerInfo so
    /// the client loads the map and reaches SignonState(FULL). On by default. When
    /// false, the server sends ServerInfo then a clean disconnect (useful for probing).
    pub full_signon: bool,
    /// File listing client Lua paths (relative to `game_dir`, one per line) to
    /// announce in `client_lua_files`. Stand-in until a GLua runtime decides this via
    /// `AddCSLuaFile`; hashes are always computed from this server's own files.
    pub client_lua_manifest: Option<PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 27015,
            hostname: "srcds-rs".into(),
            map: "gm_construct".into(),
            max_players: 32,
            game_folder: "garrysmod".into(),
            game_description: "srcds-rs".into(),
            app_id: 4000,
            version: env!("CARGO_PKG_VERSION").into(),
            gmod_build: None,
            password: None,
            // Garry's Mod default tick interval is 0.015 s (66.67 Hz).
            tick_interval: Duration::from_micros(15_000),
            metrics_log_interval: Some(Duration::from_secs(60)),
            query_rate_global: 2000,
            query_rate_per_ip: 30,
            game_dir: None,
            gamemode: "sandbox".into(),
            full_signon: true,
            client_lua_manifest: None,
        }
    }
}

impl ServerConfig {
    pub fn bind_addr(&self) -> SocketAddr {
        SocketAddr::new(self.ip, self.port)
    }
}
