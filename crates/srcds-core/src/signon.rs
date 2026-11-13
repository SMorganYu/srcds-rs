//! Signon: the reliable message batches that move a client from CONNECTED to FULL.
//!
//! The sequence mirrors what the reference GMod DS sends (captured 2026-11-14):
//!
//! 1. client `SignonState(2)` → `svc_Print`, `svc_ServerInfo`; then a second batch
//!    with `net_Tick`, the string tables, GMod message 33, replicated convars and
//!    `SignonState(3)`;
//! 2. client `ClientInfo` + `SignonState(3)` → `svc_ClassInfo(create on client)`,
//!    `svc_VoiceInit`, `svc_GameEventList`, `SignonState(4)`;
//! 3. client `SignonState(4)` → `svc_SetView`, `net_Tick`, `SignonState(5)`;
//! 4. client `SignonState(5)` → user info, `net_Tick`, first `svc_PacketEntities`,
//!    `svc_FixAngle`; the client then considers itself fully connected.
//!
//! Content is generated from this server's own state (map file, config); nothing is
//! replayed from captures.

use srcds_protocol::bitbuf::BitWriter;
use srcds_protocol::error::Result;
use srcds_protocol::netmsg::{self, ServerInfo};
use srcds_protocol::stringtable::{StringTable, UserData};

use crate::bsp::MapInfo;

/// Number of server classes announced in `svc_ServerInfo` / `svc_ClassInfo`.
/// Value observed from the reference GMod DS 2026.04.29; the client builds its class
/// table from its own `client.dll` (create-on-client), so the count must match the
/// client's class list.
pub const GMOD_SERVER_CLASSES: u16 = 250;
pub const PROTOCOL_VERSION: i16 = 24;

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
    /// Attempt the full (experimental, incomplete) world signon after ServerInfo.
    /// When false, the server sends ServerInfo then a clean disconnect.
    pub full_signon: bool,
}

/// `player_info_t` as networked in the GMod `userinfo` table (324 bytes; layout
/// confirmed from the reference capture: name[128], userID, guid[33], friendsID,
/// friendsName[128], fakeplayer, ishltv, customFiles[4], filesDownloaded).
pub fn player_info(name: &str, user_id: i32, guid: &str, fake: bool) -> Vec<u8> {
    let mut b = vec![0u8; 324];
    let put_str = |b: &mut [u8], at: usize, max: usize, s: &str| {
        let bytes: Vec<u8> = s.bytes().filter(|&c| c != 0).take(max - 1).collect();
        b[at..at + bytes.len()].copy_from_slice(&bytes);
    };
    put_str(&mut b, 0, 128, name);
    b[128..132].copy_from_slice(&user_id.to_le_bytes());
    put_str(&mut b, 132, 33, guid);
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
    t.push(models);

    let mut generic = StringTable::new("genericprecache", 9, Some((1, 2)));
    generic.push("", UserData::Fixed(0))?;
    t.push(generic);

    let mut sounds = StringTable::new("soundprecache", 14, Some((1, 2)));
    sounds.push("", UserData::Fixed(0))?;
    t.push(sounds);

    t.push(StringTable::new("decalprecache", 9, Some((1, 2))));
    t.push(StringTable::new("instancebaseline", 10, None));

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
            max_classes: GMOD_SERVER_CLASSES,
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
    netmsg::write_gmod_server_to_client(&mut w, &[3], 8)?;
    netmsg::write_set_convars(
        &mut w,
        &[
            ("sv_allow_voice_from_file", "0"),
            ("sv_skyname", &g.map.sky_name),
            ("sv_client_predict", "1"),
            ("think_limit", "0"),
        ],
    )?;
    netmsg::write_signon_state(&mut w, netmsg::SIGNONSTATE_NEW, g.server_count)?;
    Ok(w)
}

/// Batch 2: class info, voice, game events, `SignonState(PRESPAWN)`.
pub fn prespawn_batch(g: &GameInfo) -> Result<BitWriter> {
    let mut w = writer();
    netmsg::write_class_info_create_on_client(&mut w, GMOD_SERVER_CLASSES)?;
    netmsg::write_voice_init(&mut w, "vaudio_speex", 5)?;
    netmsg::write_empty_game_event_list(&mut w)?;
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
        }
    }

    #[test]
    fn tables_match_reference_shape() {
        let g = game();
        let pi = player_info("p", 2, "STEAM_0:0:1", false);
        let t = string_tables(&g, &[(0, pi)]).unwrap();
        let names: Vec<_> = t.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names[TABLE_USERINFO as usize], "userinfo");
        assert_eq!(names.len(), 19);
        // 129 model entries for gm_construct (128 brush models), as in the reference.
        assert_eq!(t[1].entries.len(), 129);
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
        assert_eq!(player_info("x", 3, "g", false).len(), 324);
    }
}
