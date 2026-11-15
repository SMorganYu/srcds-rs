//! Signon: the reliable message batches that move a client from CONNECTED to FULL.
//!
//! The sequence mirrors what the reference GMod DS sends (captured 2026-11-14):
//!
//! 1. client `SignonState(2)` → `svc_Print`, `svc_ServerInfo`; then a second batch
//!    with `net_Tick`, the string tables, GMod message 33, replicated convars and
//!    `SignonState(3)`;
//! 2. client `ClientInfo` + `SignonState(3)` → srcds-rs' own `svc_SendTable`s and
//!    `svc_ClassInfo` (the reference sends `svc_ClassInfo(create on client)` instead,
//!    which makes the client use its local game DLL's tables), `svc_VoiceInit`,
//!    `svc_GameEventList`, `SignonState(4)`;
//! 3. client `SignonState(4)` → `svc_SetView`, `net_Tick`, `SignonState(5)`;
//! 4. client `SignonState(5)` → user info, `net_Tick`, first `svc_PacketEntities`
//!    (world + player entities, see [`crate::entities`]), `svc_FixAngle`; the client
//!    then considers itself fully connected.
//!
//! Content is generated from this server's own state (map file, config); nothing is
//! replayed from captures.

use srcds_protocol::bitbuf::BitWriter;
use srcds_protocol::error::Result;
use srcds_protocol::netmsg::{self, ServerInfo};
use srcds_protocol::stringtable::{StringTable, UserData};

use crate::bsp::MapInfo;
use crate::entities::NetClasses;
use crate::movement::MoveParams;

pub const PROTOCOL_VERSION: i16 = 24;

/// Model given to player entities (precached after the map's brush models).
pub const PLAYER_MODEL: &str = "models/player/kleiner.mdl";

/// Instance baseline of every class: an empty property list (one 0 bit), i.e. all
/// properties at their client defaults; snapshots then send every networked value.
const EMPTY_BASELINE: [u8; 1] = [0];

/// Index of the `userinfo` table in creation order (see [`string_tables`]).
pub const TABLE_USERINFO: u32 = 7;

/// Standard light-style patterns (engine defaults; identical in the reference
/// server's `lightstyles` table).
const LIGHT_STYLES: &[(usize, &str)] = &[
    (0, "m"),
    (1, "mmnmmommommnonmmonqnmmo"),
    (2, "abcdefghijklmnopqrstuvwxyzyxwvutsrqponmlkjihgfedcba"),
    (3, "mmmmmaaaaammmmmaaaaaabcdefgabcdefg"),
    (4, "mamamamamama"),
    (5, "jklmnopqrstuvwxyzyxwvutsrqponmlkj"),
    (6, "nmonqnmomnmomomno"),
    (7, "mmmaaaabcdefgmmmmaaaammmaamm"),
    (8, "mmmaaammmaaammmabcdefaaaammmmabcdefmmmaaaa"),
    (9, "aaaaaaaazzzzzzzz"),
    (10, "mmamammmmammamamaaamammma"),
    (11, "abcdefghijklmnopqrrqponmlkjihgfedcba"),
    (12, "mmnnmmnnnmmnn"),
    (32, "m"),
    (33, "m"),
    (63, "a"),
];

/// Lua search paths announced as entry 0 (`paths`) of `client_lua_files`, in the
/// order the reference server sends for the sandbox gamemode.
pub const CLIENT_LUA_PATHS: &str = "/workshop/lua/;/workshop/gamemodes/;/lua/;/gamemodes/;/workshop/gamemodes/base/entities/;/gamemodes/base/entities/;/workshop/gamemodes/sandbox/entities/;/gamemodes/sandbox/entities/";

/// Static description of the running game, shared by all clients.
#[derive(Debug, Clone)]
pub struct GameInfo {
    pub map: MapInfo,
    pub host_name: String,
    pub game_dir: String,
    pub gamemode: String,
    pub max_clients: u8,
    pub tick_interval: f32,
    pub server_count: i32,
    pub query_port: u16,
    /// Client Lua files offered to clients: path and SHA-256 of `content + NUL`.
    pub client_lua_files: Vec<(String, [u8; 32])>,
    /// Run the world signon after ServerInfo. When false, the server sends ServerInfo
    /// then a clean disconnect.
    pub full_signon: bool,
    /// Networked classes and their send tables.
    pub net: NetClasses,
    /// Player movement parameters (replicated to clients).
    pub movement: MoveParams,
    /// World collision (brushes of the map).
    pub world: std::sync::Arc<crate::collision::CollisionMap>,
    /// String-table entries added while running (Lua precache, network strings).
    pub dynamic: std::cell::RefCell<DynamicTables>,
    /// World boxes of solid entities (props) players collide with, refreshed each tick.
    pub solids: std::cell::RefCell<Vec<crate::movement::SolidBox>>,
    /// Decal materials for `decalprecache` (set when the Lua game starts).
    pub decals: std::cell::RefCell<Vec<String>>,
    /// The map's HL2 useable ladders (set when the Lua game starts).
    pub ladders: std::cell::RefCell<Vec<crate::movement::Ladder>>,
    /// Game event descriptors (`resource/*events.res`).
    pub events: std::sync::Arc<crate::gameevents::GameEvents>,
}

/// Entries appended to the static string tables at run time.
#[derive(Debug, Clone, Default)]
pub struct DynamicTables {
    /// `modelprecache` entries after [`PLAYER_MODEL`] (normalised: lower case, `/`).
    pub models: Vec<String>,
    /// `networkstring` entries after the placeholder `x` (id = position + 1).
    pub network_strings: Vec<String>,
}

/// Normalised model path as stored in `modelprecache`.
pub fn normalize_model(m: &str) -> String {
    m.replace('\\', "/").to_ascii_lowercase()
}

impl GameInfo {
    /// `modelprecache` index of [`PLAYER_MODEL`] (after "", the map and `*1..`).
    pub fn player_model_index(&self) -> i32 {
        self.map.brush_models.max(1) as i32 + 1
    }

    /// `modelprecache` index of `model`, if precached.
    pub fn model_index(&self, model: &str) -> Option<i32> {
        let m = normalize_model(model);
        if m.is_empty() {
            return Some(0);
        }
        if let Some(n) = m.strip_prefix('*').and_then(|n| n.parse::<i32>().ok()) {
            return (n >= 1 && (n as usize) < self.map.brush_models).then_some(1 + n);
        }
        if m == normalize_model(PLAYER_MODEL) {
            return Some(self.player_model_index());
        }
        let d = self.dynamic.borrow();
        d.models
            .iter()
            .position(|x| *x == m)
            .map(|i| self.player_model_index() + 1 + i as i32)
    }

    /// Adds `model` to `modelprecache` (if new) and returns its index.
    pub fn precache_model(&self, model: &str) -> Option<i32> {
        if let Some(i) = self.model_index(model) {
            return Some(i);
        }
        let m = normalize_model(model);
        // studio models, and materials (ropes precache their material as a "model")
        if !(m.ends_with(".mdl") || m.ends_with(".vmt")) || m.contains("..") {
            return None;
        }
        let mut d = self.dynamic.borrow_mut();
        // 13-bit table: keep room below the limit
        if self.player_model_index() as usize + 1 + d.models.len() >= (1 << 13) - 1 {
            return None;
        }
        d.models.push(m);
        Some(self.player_model_index() + d.models.len() as i32)
    }

    /// Adds a network string (if new); id as on the client.
    pub fn add_network_string(&self, name: &str) -> usize {
        let mut d = self.dynamic.borrow_mut();
        if let Some(i) = d.network_strings.iter().position(|n| n.eq_ignore_ascii_case(name)) {
            return i + 1;
        }
        d.network_strings.push(name.to_string());
        d.network_strings.len()
    }
}

/// `player_info_t` as networked in the GMod `userinfo` table (324 bytes; layout
/// confirmed from the reference capture: name[128], userID, guid[33], friendsID,
/// friendsName[128], fakeplayer, ishltv, customFiles[4], filesDownloaded).
///
/// `friends_id` is the Steam account id (low 32 bits of the SteamID64); the client
/// derives `Player:SteamID64()` from it (avatar, profile link on the scoreboard).
pub fn player_info(name: &str, user_id: i32, guid: &str, friends_id: u32, fake: bool) -> Vec<u8> {
    let mut b = vec![0u8; 324];
    let put_str = |b: &mut [u8], at: usize, max: usize, s: &str| {
        let bytes: Vec<u8> = s.bytes().filter(|&c| c != 0).take(max - 1).collect();
        b[at..at + bytes.len()].copy_from_slice(&bytes);
    };
    put_str(&mut b, 0, 128, name);
    b[128..132].copy_from_slice(&user_id.to_le_bytes());
    put_str(&mut b, 132, 33, guid);
    // guid[33] ends at 165; friendsID is 4-byte aligned at 168, friendsName follows.
    b[168..172].copy_from_slice(&friends_id.to_le_bytes());
    b[300] = fake as u8;
    b
}

/// Classic textual SteamID for a SteamID64 (unverified claim from the client).
pub fn steam2_id(steam_id64: u64) -> String {
    let account = (steam_id64 & 0xFFFF_FFFF) as u32;
    format!("STEAM_0:{}:{}", account & 1, account >> 1)
}

/// The string tables in creation order, with this server's contents.
pub fn string_tables(g: &GameInfo, players: &[(u8, Vec<u8>)]) -> Result<Vec<StringTable>> {
    let mut t = Vec::new();

    let mut downloadables = StringTable::new("downloadables", 13, None);
    downloadables.push(&format!("maps\\{}.bsp", g.map.name), UserData::None)?;
    t.push(downloadables);

    let mut models = StringTable::new("modelprecache", 13, Some((1, 2)));
    models.push("", UserData::Fixed(0))?;
    models.push(&format!("maps/{}.bsp", g.map.name), UserData::Fixed(3))?;
    for i in 1..g.map.brush_models {
        models.push(&format!("*{i}"), UserData::Fixed(3))?;
    }
    models.push(PLAYER_MODEL, UserData::Fixed(1))?;
    for m in &g.dynamic.borrow().models {
        models.push(m, UserData::Fixed(1))?;
    }
    t.push(models);

    let mut generic = StringTable::new("genericprecache", 9, Some((1, 2)));
    generic.push("", UserData::Fixed(0))?;
    t.push(generic);

    let mut sounds = StringTable::new("soundprecache", 14, Some((1, 2)));
    sounds.push("", UserData::Fixed(0))?;
    t.push(sounds);

    // decal materials ([факт] live client: impacts printed "Bad decal index 0" with an
    // empty table — the client looks decals up by material name)
    let mut decals = StringTable::new("decalprecache", 9, Some((1, 2)));
    for d in g.decals.borrow().iter().take(511) {
        decals.push(d, UserData::Fixed(1))?;
    }
    t.push(decals);
    let mut baselines = StringTable::new("instancebaseline", 10, None);
    for class in 0..g.net.classes.len() {
        baselines.push(&class.to_string(), UserData::Bytes(EMPTY_BASELINE.to_vec()))?;
    }
    t.push(baselines);

    let mut styles = StringTable::new("lightstyles", 7, None);
    for i in 0..128usize {
        let data = LIGHT_STYLES
            .iter()
            .find(|(n, _)| *n == i)
            .map(|(_, s)| {
                let mut v = s.as_bytes().to_vec();
                v.push(0);
                UserData::Bytes(v)
            })
            .unwrap_or(UserData::None);
        styles.push(&i.to_string(), data)?;
    }
    t.push(styles);

    let mut userinfo = StringTable::new("userinfo", 8, None);
    for slot in 0..g.max_clients {
        let data = players
            .iter()
            .find(|(s, _)| *s == slot)
            .map(|(_, d)| UserData::Bytes(d.clone()))
            .unwrap_or(UserData::None);
        userinfo.push(&slot.to_string(), data)?;
    }
    t.push(userinfo);

    t.push(StringTable::new("DynamicModels", 11, Some((1, 1))));

    let mut query = StringTable::new("server_query_info", 2, None);
    query.push(
        "QueryPort",
        UserData::Bytes((g.query_port as u32).to_le_bytes().to_vec()),
    )?;
    t.push(query);

    // Index 0 is the engine's fallback effect name in the reference table.
    let mut particles = StringTable::new("ParticleEffectNames", 12, None);
    particles.push("error", UserData::None)?;
    t.push(particles);
    t.push(StringTable::new("EffectDispatch", 10, None));
    t.push(StringTable::new("VguiScreen", 8, None));
    let mut materials = StringTable::new("Materials", 10, None);
    materials.push("debug/debugempty", UserData::None)?;
    t.push(materials);
    t.push(StringTable::new("Scenes", 12, None));

    let mut rules = StringTable::new("GameRulesCreation", 0, None);
    rules.push("classname", UserData::Bytes(b"CGMODRules\0".to_vec()))?;
    t.push(rules);

    // Index 0 of `networkstring` is a placeholder ("x") in the reference table, so
    // that network-string id 0 never names a real message.
    let mut netstrings = StringTable::new("networkstring", 12, None);
    netstrings.push("x", UserData::None)?;
    for n in &g.dynamic.borrow().network_strings {
        netstrings.push(n, UserData::None)?;
    }
    t.push(netstrings);
    t.push(StringTable::new("networkvars", 12, None));
    let mut lua = StringTable::new("client_lua_files", 13, None);
    let mut paths = CLIENT_LUA_PATHS.as_bytes().to_vec();
    paths.push(0);
    lua.push("paths", UserData::Bytes(paths))?;
    for (name, hash) in &g.client_lua_files {
        lua.push(name, UserData::Bytes(hash.to_vec()))?;
    }
    t.push(lua);
    Ok(t)
}

fn writer() -> BitWriter {
    BitWriter::with_capacity(4 << 20)
}

/// Batch 1a: banner + `svc_ServerInfo`.
pub fn server_info_batch(g: &GameInfo, player_slot: u8) -> Result<BitWriter> {
    let mut w = writer();
    let banner = format!(
        "\n{}\nGamemode: {}\nMap: {}\nServer: srcds-rs {} (native Rust prototype)\n\n",
        g.host_name,
        g.gamemode,
        g.map.name,
        env!("CARGO_PKG_VERSION")
    );
    netmsg::write_print(&mut w, &banner)?;
    netmsg::write_server_info(
        &mut w,
        &ServerInfo {
            protocol: PROTOCOL_VERSION,
            server_count: g.server_count,
            is_hltv: false,
            is_dedicated: true,
            client_crc: 0xFFFF_FFFF,
            max_classes: g.net.classes.len() as u16,
            map_md5: g.map.md5,
            player_slot,
            max_clients: g.max_clients,
            tick_interval: g.tick_interval,
            os: if cfg!(windows) { b'w' } else { b'l' },
            game_dir: &g.game_dir,
            map_name: &g.map.name,
            sky_name: &g.map.sky_name,
            host_name: &g.host_name,
            gmod_extra: "",
            gamemode: &g.gamemode,
        },
    )?;
    Ok(w)
}

/// `svc_UpdateStringTable` messages for the dynamic entries a client does not have
/// yet: `known` = (models, network strings) it already has. Returns the new counts.
pub fn table_updates(g: &GameInfo, known: (usize, usize), w: &mut BitWriter) -> Result<(usize, usize)> {
    let (models, nets) = {
        let d = g.dynamic.borrow();
        (d.models.len(), d.network_strings.len())
    };
    if models == known.0 && nets == known.1 {
        return Ok(known);
    }
    let tables = string_tables(g, &[])?;
    for (id, t) in tables.iter().enumerate() {
        if t.name == "modelprecache" && models > known.0 {
            let from = t.entries.len() - (models - known.0);
            t.write_update(id as u32, from, w)?;
        }
        if t.name == "networkstring" && nets > known.1 {
            let from = t.entries.len() - (nets - known.1);
            t.write_update(id as u32, from, w)?;
        }
    }
    Ok((models, nets))
}

/// Batch 1b: string tables + replicated convars + `SignonState(NEW)`.
pub fn tables_batch(g: &GameInfo, tick: i32, players: &[(u8, Vec<u8>)]) -> Result<BitWriter> {
    let mut w = writer();
    netmsg::write_tick(&mut w, tick, 0.0, 0.0)?;
    // Diagnostic: SRCDS_RS_MAX_TABLES=N limits how many string tables are created
    // (used to bisect which table a client rejects). Unset = all tables.
    let limit = std::env::var("SRCDS_RS_MAX_TABLES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(usize::MAX);
    for t in string_tables(g, players)?.into_iter().take(limit) {
        t.write_create(&mut w)?;
    }
    // GMod message 33 with a single byte 0x03 precedes the convars in the reference
    // signon (meaning not established; reproduced as an observed protocol step).
    // Diagnostic: SRCDS_RS_SKIP_GMOD33=1 omits it (bisection of signon failures).
    if std::env::var_os("SRCDS_RS_SKIP_GMOD33").is_none() {
        netmsg::write_gmod_server_to_client(&mut w, &[3], 8)?;
    }
    let movement = g.movement.replicated_convars();
    let mut vars: Vec<(&str, &str)> = vec![
        ("sv_allow_voice_from_file", "0"),
        ("sv_skyname", &g.map.sky_name),
        ("sv_client_predict", "1"),
        ("think_limit", "0"),
    ];
    vars.extend(movement.iter().map(|(k, v)| (*k, v.as_str())));
    netmsg::write_set_convars(&mut w, &vars)?;
    netmsg::write_signon_state(&mut w, netmsg::SIGNONSTATE_NEW, g.server_count)?;
    Ok(w)
}

/// Batch 2: send tables + class info, voice, game events, `SignonState(PRESPAWN)`.
pub fn prespawn_batch(g: &GameInfo) -> Result<BitWriter> {
    let mut w = writer();
    g.net.write_signon(&mut w)?;
    netmsg::write_voice_init(&mut w, "vaudio_speex", 5)?;
    g.events.write_list(&mut w)?;
    netmsg::write_signon_state(&mut w, netmsg::SIGNONSTATE_PRESPAWN, g.server_count)?;
    Ok(w)
}

/// Batch 3: view entity + `SignonState(SPAWN)`.
pub fn spawn_batch(g: &GameInfo, player_slot: u8, tick: i32) -> Result<BitWriter> {
    let mut w = writer();
    netmsg::write_set_view(&mut w, player_slot as u32 + 1)?;
    netmsg::write_tick(&mut w, tick, g.tick_interval, 0.0)?;
    netmsg::write_signon_state(&mut w, netmsg::SIGNONSTATE_SPAWN, g.server_count)?;
    Ok(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game() -> GameInfo {
        GameInfo {
            map: MapInfo {
                name: "gm_construct".into(),
                md5: [7; 16],
                brush_models: 128,
                sky_name: "painted".into(),
                world_mins: [0.0; 3],
                world_maxs: [0.0; 3],
                spawn_points: Vec::new(),
                entities: Vec::new(),
            },
            host_name: "test".into(),
            game_dir: "garrysmod".into(),
            gamemode: "sandbox".into(),
            max_clients: 8,
            tick_interval: 0.015,
            server_count: 1,
            query_port: 27015,
            client_lua_files: Vec::new(),
            full_signon: false,
            net: NetClasses::new().unwrap(),
            movement: MoveParams::default(),
            world: std::sync::Arc::new(crate::collision::CollisionMap::empty()),
            dynamic: Default::default(),
            solids: Default::default(),
            ladders: Default::default(),
            decals: Default::default(),
            events: Default::default(),
        }
    }

    #[test]
    fn tables_match_reference_shape() {
        let g = game();
        let pi = player_info("p", 2, "STEAM_0:0:1", 2, false);
        let t = string_tables(&g, &[(0, pi)]).unwrap();
        let names: Vec<_> = t.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names[TABLE_USERINFO as usize], "userinfo");
        assert_eq!(names.len(), 19);
        // 129 model entries for gm_construct (128 brush models), as in the reference,
        // plus the player model.
        assert_eq!(t[1].entries.len(), 130);
        assert_eq!(
            t[1].entries[g.player_model_index() as usize].string,
            PLAYER_MODEL
        );
        assert_eq!(t[5].entries.len(), g.net.classes.len());
        assert_eq!(t[6].entries.len(), 128);
        assert_eq!(t[7].entries.len(), 8);
    }

    #[test]
    fn batches_encode() {
        let g = game();
        assert!(server_info_batch(&g, 0).unwrap().bit_len() > 700);
        assert!(tables_batch(&g, 1, &[]).unwrap().bit_len() > 1000);
        assert!(prespawn_batch(&g).unwrap().bit_len() > 0);
        assert!(spawn_batch(&g, 0, 5).unwrap().bit_len() > 0);
    }

    #[test]
    fn steam2() {
        // synthetic SteamID64 (account 3)
        assert_eq!(steam2_id(76561197960265731), "STEAM_0:1:1");
        assert_eq!(steam2_id(76561197960265728), "STEAM_0:0:0");
        let pi = player_info("x", 3, "g", 7, false);
        assert_eq!(pi.len(), 324);
        assert_eq!(&pi[168..172], &7u32.to_le_bytes());
    }
}
