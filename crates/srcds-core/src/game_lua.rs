//! The GLua gamemode attached to the server: boots the gamemode, mirrors players
//! between the network side (pawns) and the Lua entity store each tick, and routes
//! client console commands and chat to Lua.

use std::path::Path;
use std::sync::Arc;

use srcds_lua::entity::{store, EntRef};
use srcds_lua::lib_ents::{TraceHit, WorldTrace, WorldTracer};
use srcds_lua::GLua;
use tracing::{info, warn};

use crate::client::Client;
use crate::collision::{CollisionMap, MASK_PLAYERSOLID};
use crate::entities;
use crate::signon::GameInfo;
#[allow(unused_imports)]
use srcds_lua::entity::EntData;

struct MapTracer(Arc<CollisionMap>);

struct ContentModels(Arc<crate::vpk::Content>);

impl srcds_lua::entity::ModelInfo for ContentModels {
    fn bounds(&self, model: &str) -> Option<([f32; 3], [f32; 3])> {
        self.0.model_bounds(model)
    }
    fn exists(&self, model: &str) -> bool {
        self.0.exists(model)
    }
    fn read(&self, path: &str) -> Option<Vec<u8>> {
        self.0.read(path, 256 * 1024)
    }
    fn sequences(&self, model: &str) -> Vec<(String, String)> {
        self.0.model_sequences(model).into_iter().map(|s| (s.label, s.activity)).collect()
    }
}

impl WorldTrace for MapTracer {
    fn trace(&self, start: [f32; 3], end: [f32; 3], mins: [f32; 3], maxs: [f32; 3]) -> TraceHit {
        let t = self.0.trace_box(start, end, mins, maxs, MASK_PLAYERSOLID);
        TraceHit {
            fraction: t.fraction,
            end: t.end,
            normal: t.normal,
            start_solid: t.start_solid,
            all_solid: t.all_solid,
            hit_sky: false,
        }
    }
}

pub struct LuaGame {
    pub glua: GLua,
    started: std::time::Instant,
    world: Arc<CollisionMap>,
    gravity: f32,
    dt: f32,
    weapons: std::cell::RefCell<crate::weapons::WeaponSystem>,
    /// Game events to broadcast at the end of the tick.
    events: std::cell::RefCell<Vec<(&'static str, Vec<(&'static str, crate::gameevents::EventValue)>)>>,
    weapon_models: std::cell::RefCell<crate::weapons::WeaponModels>,
}

/// Map brush entities srcds-rs networks: `Some(solid)` for the visible ones (by
/// class; disabled or non-solid keyvalues respected), `None` for server-only brushes
/// (triggers, area portals, …).
pub fn brush_class(e: &srcds_lua::entity::EntData) -> Option<bool> {
    if !e.model.starts_with('*') {
        return None;
    }
    let kv = |k: &str| e.keyvalues.iter().rev().find(|(a, _)| a.eq_ignore_ascii_case(k)).map(|(_, v)| v.trim().to_string());
    let c = e.class.as_str();
    let solid = match c {
        "func_illusionary" | "func_lod" | "func_reflective_glass" | "func_monitor" => false,
        // Solidity: 0 toggle (with the enabled state), 1 never, 2 always
        "func_brush" => kv("Solidity").as_deref() != Some("1") && kv("StartDisabled").as_deref() != Some("1"),
        "func_wall" | "func_wall_toggle" | "func_door" | "func_door_rotating" | "func_movelinear" | "func_breakable"
        | "func_physbox" | "func_physbox_multiplayer" | "func_rotating" | "func_tracktrain" | "func_tanktrain" | "func_button"
        | "func_rot_button" | "func_conveyor" | "func_platrot" | "func_plat" | "func_train" | "func_guntarget"
        | "func_pushable" => true,
        _ => return None,
    };
    Some(solid)
}

/// An entity's NetworkVars as networked values.
pub fn dtvars(e: &srcds_lua::entity::EntData) -> Vec<entities::DtVar> {
    use srcds_lua::entity::NetValue as N;
    use srcds_protocol::sendtable::PropValue as P;
    e.dt.iter()
        .filter_map(|((ty, slot), v)| {
            let pv = match v {
                N::Bool(b) => P::Int(*b as i32),
                N::Int(i) => P::Int(*i as i32),
                N::Float(f) if *ty == 1 => P::Int(*f as i32),
                N::Float(f) => P::Float(*f as f32),
                N::Vec(v) | N::Ang(v) => P::Vector(*v),
                N::Ent(r) if r.is_null() => P::Int(entities::INVALID_EHANDLE),
                N::Ent(r) => P::Int(entities::ehandle(r.id, r.serial)),
                N::Str(s) => P::String(s.chars().take(511).collect()),
            };
            Some((*ty, *slot, pv))
        })
        .collect()
}

impl LuaGame {
    /// Boots `gamemode` from `<game_dir>` (the `garrysmod` folder) and creates the
    /// map entities. Returns `None` (logged) when Lua cannot start.
    pub fn start(game_dir: &Path, game: &GameInfo) -> Option<LuaGame> {
        let glua = match GLua::new(game_dir) {
            Ok(g) => g,
            Err(e) => {
                warn!(error = %e, "could not create the Lua state");
                return None;
            }
        };
        glua.lua.set_app_data(WorldTracer(Box::new(MapTracer(game.world.clone()))));
        let content = Arc::new(crate::vpk::Content::mount(game_dir));
        if let Ok(mut s) = store(&glua.lua) {
            s.models = Some(Box::new(ContentModels(content.clone())));
        }
        if let Some(mut e) = glua.lua.app_data_mut::<srcds_lua::lib_misc::Engine>() {
            e.map = game.map.name.clone();
            e.max_players = game.max_clients as i64;
            e.gamemode = game.gamemode.clone();
            e.tick_interval = game.tick_interval as f64;
            e.frame_time = game.tick_interval as f64;
        }
        // the server database (sv.db) is srcds-rs' own file, never the game install's
        let db = std::env::var_os("SRCDS_RS_SV_DB").map(std::path::PathBuf::from).unwrap_or_else(|| "sv.db".into());
        if let Err(e) = srcds_lua::lib_sql::install(&glua.lua, &db) {
            warn!(error = %e, "sql database unavailable");
        }
        let t0 = std::time::Instant::now();
        if let Err(e) = glua.boot(&game.gamemode) {
            warn!(error = %e, "gamemode boot failed");
            return None;
        }
        glua.start_game(
            game.max_clients as u32,
            vec![entities::player_resource_index(game.max_clients)],
            &game.map.entities,
        );
        for n in glua.network_strings() {
            game.add_network_string(&n);
        }
        // rope materials go into the model table before any client signs on: the
        // client resolves a rope's material once, when the rope is created
        // sandbox's rope material list (client-side `list.Set("RopeMaterials", name, mat)`)
        let mut materials: Vec<String> = content
            .read("gamemodes/sandbox/gamemode/spawnmenu/controls/ropematerial.lua", 64 * 1024)
            .map(|b| {
                String::from_utf8_lossy(&b)
                    .lines()
                    .filter(|l| l.contains("\"RopeMaterials\""))
                    .filter_map(|l| l.split('"').nth(5).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        materials.push("cable/rope".into());
        materials.push("cable/cable".into());
        for kv in &game.map.entities {
            if kv.iter().any(|(k, v)| k == "classname" && (v == "keyframe_rope" || v == "move_rope")) {
                if let Some((_, m)) = kv.iter().find(|(k, _)| k.eq_ignore_ascii_case("RopeMaterial")) {
                    materials.push(m.clone());
                }
            }
        }
        for m in materials {
            let _ = game.precache_model(&format!("{}.vmt", m.trim_end_matches(".vmt")));
        }
        // decal materials: every material of the decal groups in
        // scripts/decals_subrect.txt (`"group" { "decals/…" "weight" … }`)
        if let Some(b) = content.read("scripts/decals_subrect.txt", 1 << 20) {
            let text = String::from_utf8_lossy(&b);
            let mut list: Vec<String> = Vec::new();
            for line in text.lines() {
                let line = line.split("//").next().unwrap_or("");
                let q: Vec<&str> = line.split('"').collect();
                if q.len() >= 4 {
                    let name = q[1].trim().replace('\\', "/").to_ascii_lowercase();
                    if name.contains('/') && !list.contains(&name) {
                        list.push(name);
                    }
                }
            }
            info!(decals = list.len(), "decal materials");
            *game.decals.borrow_mut() = list;
        }
        // HL2 useable ladders: ends from point0/point1 (world space), dismount points
        // within reach of either end
        if let Ok(s) = store(&glua.lua) {
            let vec3 = |v: &str| {
                let mut it = v.split_whitespace().map(|t| t.parse::<f32>().unwrap_or(0.0));
                [it.next().unwrap_or(0.0), it.next().unwrap_or(0.0), it.next().unwrap_or(0.0)]
            };
            let dismounts: Vec<[f32; 3]> = s.ents.values().filter(|e| e.class == "info_ladder_dismount").map(|e| e.pos).collect();
            let mut ladders = Vec::new();
            for e in s.ents.values().filter(|e| e.class == "func_useableladder") {
                let kv = |k: &str| e.keyvalues.iter().find(|(a, _)| a.eq_ignore_ascii_case(k)).map(|(_, v)| vec3(v));
                let (Some(a), Some(b)) = (kv("point0"), kv("point1")) else { continue };
                let (bottom, top) = if a[2] <= b[2] { (a, b) } else { (b, a) };
                let near = |d: &[f32; 3], end: [f32; 3]| (0..3).map(|k| (d[k] - end[k]).powi(2)).sum::<f32>() < 128.0 * 128.0;
                ladders.push(crate::movement::Ladder {
                    bottom,
                    top,
                    handle: entities::ehandle(e.r.id, e.r.serial),
                    dismounts: dismounts.iter().filter(|d| near(d, bottom) || near(d, top)).copied().collect(),
                });
            }
            info!(ladders = ladders.len(), "useable ladders");
            *game.ladders.borrow_mut() = ladders;
        }
        let s = glua.state.borrow();
        info!(
            gamemode = %game.gamemode,
            ms = t0.elapsed().as_millis() as u64,
            errors = s.errors,
            stubbed = s.stubbed,
            "Lua gamemode started"
        );
        drop(s);
        Some(LuaGame {
            glua,
            started: std::time::Instant::now(),
            world: game.world.clone(),
            gravity: game.movement.gravity,
            dt: game.tick_interval,
            weapons: Default::default(),
            events: Default::default(),
            weapon_models: std::cell::RefCell::new(crate::weapons::WeaponModels::new(content)),
        })
    }

    pub fn cur_time(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// One tick: players in, commands, think, players out.
    pub fn tick<'a>(&self, _game: &GameInfo, clients: impl Iterator<Item = &'a mut Client>, tick_count: i32) {
        let lua = &self.glua.lua;
        if let Some(mut e) = lua.app_data_mut::<srcds_lua::lib_misc::Engine>() {
            e.tick_count = tick_count as i64;
        }
        let mut clients: Vec<&mut Client> = clients.collect();
        // new players
        for c in clients.iter_mut() {
            // [факт] sandbox creates cl_playercolor / cl_weaponcolor in client Lua: the
            // player spawns in Lua once the client is fully in (its Lua has run and sent
            // them), as the engine does — earlier, GetInfo was empty and the player black
            if c.pawn.is_some() && c.lua_ent.is_none() && c.drop_reason.is_none() && c.signon >= srcds_protocol::netmsg::SIGNONSTATE_FULL {
                c.user_info_dirty = false;
                let r = self.glua.player_connect_with_serial(
                    c.slot as u32,
                    c.user_id,
                    &c.name,
                    c.claimed_steam_id.unwrap_or(0),
                    false,
                    &c.addr.to_string(),
                    Some(c.serial),
                    &c.user_info,
                );
                if !r.is_null() {
                    info!(name = %c.name, ent = r.id, "player entity created in Lua");
                    use crate::gameevents::EventValue as V;
                    let sid = c.claimed_steam_id.map(crate::signon::steam2_id).unwrap_or_else(|| "STEAM_ID_LAN".into());
                    for name in ["player_connect", "player_connect_client"] {
                        self.queue_event(
                            name,
                            vec![
                                ("name", V::Str(c.name.clone())),
                                ("index", V::Int(c.slot as i64)),
                                ("userid", V::Int(c.user_id as i64)),
                                ("networkid", V::Str(sid.clone())),
                                ("address", V::Str("none".into())),
                            ],
                        );
                    }
                    self.queue_event("player_activate", vec![("userid", V::Int(c.user_id as i64))]);
                    self.queue_event("player_spawn", vec![("userid", V::Int(c.user_id as i64))]);
                    c.lua_ent = Some(r);
                }
            }
        }
        // network → Lua
        for c in clients.iter_mut() {
            let (Some(r), Some(p)) = (c.lua_ent, c.pawn.as_ref()) else { continue };
            let ping = c.ping_ms() as i64;
            let buttons = c.buttons;
            let world_click = c.world_click;
            let info = if c.user_info_dirty {
                c.user_info_dirty = false;
                Some(c.user_info.clone())
            } else {
                None
            };
            if let Ok(mut s) = store(lua) {
                if let Some(e) = s.get_mut(r) {
                    e.pos = p.mv.origin;
                    e.vel = p.mv.velocity;
                    e.ang = [0.0, p.angles[1], 0.0];
                    e.move_type = p.move_type;
                    e.nw.insert("__srcds_ping".into(), srcds_lua::entity::NetValue::Int(ping));
                    if let Some(pd) = &mut e.player {
                        pd.old_buttons = pd.buttons;
                        pd.buttons = buttons;
                        pd.eye_angles = p.angles;
                        pd.world_click = world_click;
                        pd.crouching = p.mv.ducked;
                        pd.on_ground = p.mv.on_ground;
                        pd.noclip = p.move_type == entities::MOVETYPE_NOCLIP;
                        if let Some(i) = info {
                            pd.info = i;
                        }
                    }
                }
            }
        }
        // noclip toggles the client predicted: the gamemode may refuse them
        for c in clients.iter_mut() {
            let Some(r) = c.lua_ent else { continue };
            for on in std::mem::take(&mut c.noclip_events) {
                let out = self.glua.hook_call("PlayerNoClip", (r, on));
                let allowed = matches!(out.into_iter().next(), Some(mlua_v) if mlua_v.as_boolean() == Some(true));
                if !allowed {
                    if let Some(p) = c.pawn.as_mut() {
                        p.move_type = if on { entities::MOVETYPE_WALK } else { entities::MOVETYPE_NOCLIP };
                    }
                }
                info!(name = %c.name, noclip = on, allowed, "noclip");
            }
            // held-key changes (numpad controls of thrusters, cameras, lamps, …)
            for (code, down) in std::mem::take(&mut c.button_events) {
                self.glua.hook_call(if down { "PlayerButtonDown" } else { "PlayerButtonUp" }, (r, code as i64));
            }
            for _ in 0..std::mem::take(&mut c.flashlight_requests) {
                if let Some(p) = c.pawn.as_mut() {
                    let want = !p.flashlight;
                    let out = self.glua.hook_call("PlayerSwitchFlashlight", (r, want));
                    if !matches!(out.into_iter().next(), Some(srcds_lua::mlua::Value::Boolean(false))) {
                        p.flashlight = want;
                    }
                }
            }
        }
        // GMod packets (net messages, client Lua errors)
        for c in clients.iter_mut() {
            let Some(r) = c.lua_ent else { continue };
            for (data, bits) in std::mem::take(&mut c.gmod_packets) {
                if bits < 8 || data.is_empty() {
                    continue;
                }
                match data[0] {
                    0 => self.glua.net_incoming(r, data[1..].to_vec(), bits - 8),
                    2 => {
                        let text = String::from_utf8_lossy(&data[1..]);
                        warn!(name = %c.name, error = %text.trim_end_matches('\0').trim(), "client Lua error");
                    }
                    t => tracing::debug!(name = %c.name, kind = t, bits, "GMod packet ignored"),
                }
            }
        }
        // client commands
        let mut chat: Vec<(u8, String, bool, bool)> = Vec::new();
        for c in clients.iter_mut() {
            let Some(r) = c.lua_ent else { continue };
            for cmd in std::mem::take(&mut c.lua_commands) {
                let name = cmd.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
                match name.as_str() {
                    "say" | "say_team" => {
                        let text = cmd[name.len()..].trim().trim_matches('"').to_string();
                        let out = self.glua.player_say(r, &text, name == "say_team");
                        if !out.is_empty() {
                            info!(name = %c.name, text = %out, "chat");
                            let dead = c.pawn.as_ref().is_none_or(|p| p.health <= 0);
                            self.queue_event(
                                "player_say",
                                vec![
                                    ("userid", crate::gameevents::EventValue::Int(c.user_id as i64)),
                                    ("text", crate::gameevents::EventValue::Str(out.clone())),
                                    ("teamonly", crate::gameevents::EventValue::Bool(name == "say_team")),
                                ],
                            );
                            chat.push((r.id as u8, out, name == "say_team", dead));
                        }
                    }
                    _ => {
                        if !self.glua.client_command(r, &cmd) {
                            match name.as_str() {
                                "kill" | "explode" => {
                                    self.glua.player_suicide(r);
                                    let uid = crate::gameevents::EventValue::Int(c.user_id as i64);
                                    self.queue_event("player_death", vec![("userid", uid.clone()), ("attacker", uid), ("weapon", crate::gameevents::EventValue::Str("world".into()))]);
                                }
                                _ => c.print(&format!("Unknown command: {name}\n")),
                            }
                        }
                    }
                }
            }
        }
        for c in clients.iter_mut() {
            if c.lua_ent.is_none() {
                continue;
            }
            for (sender, text, team, dead) in &chat {
                c.say_text(*sender, text, *team, *dead);
            }
        }
        // weapons: switching and the physics gun
        for c in clients.iter_mut() {
            let (Some(r), Some(p)) = (c.lua_ent, c.pawn.as_ref()) else { continue };
            let mut ws = self.weapons.borrow_mut();
            let mut models = self.weapon_models.borrow_mut();
            if let Some(idx) = c.weapon_select {
                // keep an unknown selection for a moment: the weapon may be given by a
                // command that arrives in a later packet
                // the switch happens at the client's command time (its prediction does)
                let t = crate::weapon_sim::command_time(p.tick_base, self.dt);
                if ws.select(&self.glua, &mut models, r, idx, t) || c.weapon_select_age > 66 {
                    c.weapon_select = None;
                    c.weapon_select_age = 0;
                } else {
                    c.weapon_select_age += 1;
                }
            }
            let old = store(lua).ok().and_then(|s| s.get(r).and_then(|e| e.player.as_ref().map(|p| p.old_buttons))).unwrap_or(0);
            let view = if p.mv.ducked { entities::VIEW_HEIGHT_DUCKED } else { entities::VIEW_HEIGHT };
            let eye = [p.mv.origin[0], p.mv.origin[1], p.mv.origin[2] + view];
            // world clicking (context menu held) aims at the cursor
            let aim = c.world_click.map_or(p.angles, srcds_lua::vecang::vector_angles);
            ws.physgun(&self.glua, r, eye, aim, c.buttons, old, c.mouse_wheel, c.mouse_delta, self.dt);
            // attacks, reloads and idle animations, command by command at the
            // commands' own times (as the client predicts them)
            for mut cmd in std::mem::take(&mut c.weapon_cmds) {
                if let Some(d) = c.world_click {
                    cmd.angles = srcds_lua::vecang::vector_angles(d);
                }
                for victim in ws.run_command(&self.glua, &mut models, r, &cmd) {
                    self.glua.player_kill(victim, false);
                }
            }
            c.mouse_wheel = 0;
            c.mouse_delta = (0, 0);
        }
        // bolts and grenades in flight
        let victims = self.weapons.borrow_mut().step_projectiles(&self.glua, self.gravity, self.dt, self.cur_time());
        for v in victims {
            self.glua.player_kill(v, false);
        }
        self.glua.think(self.cur_time());
        if let Ok(mut s) = store(lua) {
            crate::physics::step(&mut s, &self.world, self.gravity, self.dt);
            crate::physics::solve_constraints(&mut s, self.dt);
            // solid props for player movement (world boxes)
            let boxes: Vec<crate::movement::SolidBox> = s
                .ents
                .values()
                .filter(|e| e.phys.is_some() && e.r.id != 0 && e.spawned && !e.removing && e.solid != 0 && e.player.is_none())
                .filter(|e| e.weapon_owner.is_null() && e.solid_flags & 4 == 0 && !matches!(e.collision_group, 1 | 2 | 11 | 20))
                .filter(|e| e.maxs != e.mins)
                .map(|e| {
                    // oriented box of the model bounds (axes as Vector rotation uses)
                    let (f, r, u) = srcds_lua::vecang::angle_vectors(e.ang);
                    crate::movement::SolidBox::Planes(crate::collision::oriented_box_planes(e.pos, [f, [-r[0], -r[1], -r[2]], u], e.mins, e.maxs))
                })
                .chain(s.ents.values().filter_map(|e| {
                    let model = e.model.strip_prefix('*')?.parse::<usize>().ok()?;
                    (brush_class(e) == Some(true) && !e.removing).then_some(crate::movement::SolidBox::Brush { model, origin: e.pos })
                }))
                .collect();
            *_game.solids.borrow_mut() = boxes;
        }
        let broadcast = lua
            .app_data_mut::<srcds_lua::lib_misc::Engine>()
            .map(|mut e| std::mem::take(&mut e.broadcast_prints))
            .unwrap_or_default();
        // Lua → network
        for c in clients.iter_mut() {
            if c.lua_ent.is_some() {
                for (kind, text) in &broadcast {
                    c.text_msg((*kind).clamp(1, 4), text);
                }
            }
            let Some(r) = c.lua_ent else { continue };
            let mut prints = Vec::new();
            let mut commands = Vec::new();
            let mut lua_code = Vec::new();
            let mut kick = None;
            let mut set_view = None;
            if let Ok(mut s) = store(lua) {
                let Some(e) = s.get_mut(r) else { continue };
                let (pos, health, move_type) = (e.pos, e.health, e.move_type);
                let color = |k: &str, d: [f32; 3]| match e.nw.get(k) {
                    Some(srcds_lua::entity::NetValue::Vec(v)) => *v,
                    _ => d,
                };
                let (player_color, weapon_color) = (color("__srcds_playercolor", [0.24, 0.34, 0.41]), color("__srcds_weaponcolor", [0.3, 1.8, 2.1]));
                // the model Lua chose (player_manager / cl_playermodel)
                let model_index = (!e.model.is_empty()).then(|| _game.precache_model(&e.model)).flatten();
                let dt = dtvars(e);
                if let Some(srcds_lua::entity::NetValue::Str(k)) = e.nw.remove("__srcds_kick") {
                    kick = Some(k);
                }
                if let (Some(pd), Some(p)) = (e.player.as_mut(), c.pawn.as_mut()) {
                    if pd.teleport {
                        pd.teleport = false;
                        p.mv.origin = pos;
                        p.mv.velocity = [0.0; 3];
                        p.mv.on_ground = false;
                    }
                    if pd.eye_angles_set {
                        pd.eye_angles_set = false;
                        p.angles = pd.eye_angles;
                    }
                    p.health = health;
                    p.player_color = player_color;
                    p.dt = dt.clone();
                    let mut ammo = vec![0; entities::AMMO_SLOTS];
                    for (name, n) in &pd.ammo {
                        let id = srcds_lua::ammo::ammo_id(lua, name);
                        if id >= 0 && (id as usize) < ammo.len() {
                            ammo[id as usize] = *n;
                        }
                    }
                    p.ammo = ammo;
                    p.weapon_color = weapon_color;
                    if let Some(m) = model_index {
                        p.model_index = m;
                    }
                    p.hands = if pd.hands.is_null() { entities::INVALID_EHANDLE } else { entities::ehandle(pd.hands.id, pd.hands.serial) };
                    p.params.walk_speed = pd.walk_speed;
                    p.params.run_speed = pd.run_speed;
                    p.params.slow_walk_speed = pd.slow_walk_speed;
                    p.params.crouched_speed = pd.crouched_walk_speed;
                    p.params.jump_power = pd.jump_power;
                    p.params.duck_time = pd.duck_speed;
                    p.params.unduck_time = pd.unduck_speed;
                    // Lua changed the move type (SetMoveType); only walk / noclip are simulated
                    if move_type != p.move_type
                        && (move_type == entities::MOVETYPE_WALK || move_type == entities::MOVETYPE_NOCLIP)
                    {
                        p.move_type = move_type;
                        p.mv.on_ground = false;
                    }
                    let h = |r: EntRef| if r.is_null() { entities::INVALID_EHANDLE } else { entities::ehandle(r.id, r.serial) };
                    let weapons: Vec<i32> = pd.weapons.iter().map(|w| h(*w)).collect();
                    // a slot that empties keeps the removed weapon's handle (resolves to
                    // no entity); never 0, which is the world
                    for (i, slot) in p.inventory.slots.iter_mut().enumerate() {
                        if let Some(w) = weapons.get(i) {
                            *slot = *w;
                        }
                    }
                    p.inventory.active = h(pd.active_weapon);
                    p.inventory.weapons = weapons;
                    p.inventory.view_model = h(pd.view_model);
                    if pd.view_entity != pd.view_entity_sent {
                        pd.view_entity_sent = pd.view_entity;
                        set_view = Some(if pd.view_entity.is_null() { r.id } else { pd.view_entity.id });
                    }
                    prints.extend(pd.outbox.drain(..));
                    // client-side Lua and commands need net messages (not implemented yet)
                    lua_code.extend(pd.send_lua.drain(..));
                    commands.extend(pd.client_commands.drain(..));
                }
            }
            for (kind, text) in prints {
                c.text_msg(kind.clamp(1, 4), &text);
            }
            for cmd in commands {
                c.send_command(&cmd);
            }
            for code in lua_code {
                if let Some((data, bits)) = srcds_lua::lib_net::lua_packet(&code) {
                    c.send_gmod(&data, bits);
                }
            }
            if let Some(index) = set_view {
                c.send_set_view(index);
            }
            if let Some(reason) = kick {
                c.drop_reason = Some(reason);
            }
        }
    }

    /// Queues a game event for every player in the game.
    pub fn queue_event(&self, name: &'static str, values: Vec<(&'static str, crate::gameevents::EventValue)>) {
        self.events.borrow_mut().push((name, values));
    }

    /// Delivers Lua net messages (after making their network strings known).
    pub fn deliver_net<'a>(&self, game: &GameInfo, clients: impl Iterator<Item = &'a mut Client>) {
        let names = self.glua.network_strings();
        if names.len() > game.dynamic.borrow().network_strings.len() {
            for n in &names {
                game.add_network_string(n);
            }
        }
        let msgs = self.glua.take_net_messages();
        let events = std::mem::take(&mut *self.events.borrow_mut());
        for c in clients {
            c.sync_tables(game);
            let Some(r) = c.lua_ent else { continue };
            for (name, values) in &events {
                c.send_game_event(&game.events, name, values);
            }
            for m in &msgs {
                let to = match &m.targets {
                    srcds_lua::lib_net::Targets::All => true,
                    srcds_lua::lib_net::Targets::Only(v) => v.contains(&r),
                    srcds_lua::lib_net::Targets::AllExcept(v) => !v.contains(&r),
                };
                if to {
                    c.send_gmod(&m.data, m.bits);
                }
            }
        }
    }

    /// Lua entities that srcds-rs networks (physics props), with their models
    /// precached into `modelprecache`.
    pub fn network_entities(&self, game: &GameInfo, tick: i32) -> Vec<entities::EntityState> {
        let Ok(mut s) = store(&self.glua.lua) else { return Vec::new() };
        // NPCs have no AI here: they at least play their idle animation instead of
        // the model's first sequence (the ragdoll pose)
        {
            let st = &mut *s;
            for e in st.ents.values_mut() {
                if e.sequence == 0 && e.class.starts_with("npc_") && !e.model.is_empty() && !e.nw.contains_key("__srcds_idle") {
                    e.nw.insert("__srcds_idle".into(), srcds_lua::entity::NetValue::Int(1));
                    if let Some(m) = st.models.as_ref() {
                        if let Some(i) = m.sequences(&e.model).iter().position(|(_, a)| a.eq_ignore_ascii_case("ACT_IDLE")) {
                            e.sequence = i as i32;
                        }
                    }
                }
            }
        }
        // simulation time advances only while an entity moves, so resting entities
        // produce no delta
        for e in s.ents.values_mut() {
            let moving = e.vel != [0.0; 3] || e.phys.as_ref().is_some_and(|p| p.held);
            let known = matches!(e.nw.get("__srcds_simtick"), Some(srcds_lua::entity::NetValue::Int(_)));
            if moving || !known {
                e.nw.insert("__srcds_simtick".into(), srcds_lua::entity::NetValue::Int(tick as i64));
            }
        }
        let mut out = Vec::new();
        // visual ropes (keyframe_rope): endpoints are entities + local offsets
        for e in s.ents.values().filter(|e| e.class == "keyframe_rope" && e.spawned && !e.removing) {
            let kv = |k: &str| e.keyvalues.iter().rev().find(|(a, _)| a.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone());
            let vec3 = |k: &str| {
                kv(k).map_or([0.0; 3], |v| {
                    let mut it = v.split_whitespace().map(|t| t.parse::<f32>().unwrap_or(0.0));
                    [it.next().unwrap_or(0.0), it.next().unwrap_or(0.0), it.next().unwrap_or(0.0)]
                })
            };
            let ent = |k: &str| match e.nw.get(k) {
                Some(srcds_lua::entity::NetValue::Ent(r)) if !r.is_null() => {
                    // the world has serial 0 on the wire
                    if r.id == 0 { entities::ehandle(0, entities::WORLD_SERIAL) } else { entities::ehandle(r.id, r.serial) }
                }
                _ => entities::ehandle(0, entities::WORLD_SERIAL),
            };
            let material = kv("RopeMaterial").unwrap_or_else(|| "cable/rope".into());
            let Some(material) = game.precache_model(&format!("{}.vmt", material.trim_end_matches(".vmt"))) else { continue };
            let num = |k: &str, d: f32| kv(k).and_then(|v| v.trim().parse::<f32>().ok()).unwrap_or(d);
            // ROPE_SIMULATE | ROPE_INITIAL_HANG, + ROPE_COLLIDE
            let flags = 8 | 64 | if num("Collide", 0.0) != 0.0 { 4 } else { 0 };
            out.push(entities::rope(
                &game.net,
                &entities::RopeState {
                    index: e.r.id,
                    serial: e.r.serial,
                    origin: e.pos,
                    start: ent("__srcds_kvent_startentity"),
                    end: ent("__srcds_kvent_endentity"),
                    start_offset: vec3("StartOffset"),
                    end_offset: vec3("EndOffset"),
                    length: num("Length", 100.0) as i32,
                    width: num("Width", 2.0),
                    material,
                    flags,
                    texture_scale: num("TextureScale", 1.0),
                },
            ));
        }
        // HL2 useable ladders
        for e in s.ents.values().filter(|e| e.class == "func_useableladder") {
            if let Some(l) = game.ladders.borrow().iter().find(|l| l.handle == entities::ehandle(e.r.id, e.r.serial)) {
                out.push(entities::ladder_entity(&game.net, e.r.id, e.r.serial, e.pos, l.bottom, l.top));
            }
        }
        // map brush entities
        for e in s.ents.values().filter(|e| e.spawned && !e.removing) {
            let Some(solid) = brush_class(e) else { continue };
            let Some(model_index) = game.model_index(&e.model) else { continue };
            let disabled = e.class == "func_brush"
                && e.keyvalues.iter().any(|(k, v)| k.eq_ignore_ascii_case("StartDisabled") && v.trim() == "1");
            out.push(entities::brush_entity(
                &game.net,
                &entities::BrushState {
                    index: e.r.id,
                    serial: e.r.serial,
                    model_index,
                    origin: e.pos,
                    angles: e.ang,
                    color: e.color,
                    render_mode: e.render_mode,
                    render_fx: e.render_fx,
                    effects: e.effects | if e.no_draw || disabled { 0x20 } else { 0 },
                    solid,
                },
            ));
        }
        for e in s.ents.values() {
            if !e.spawned || e.removing || !e.model.to_ascii_lowercase().ends_with(".mdl") {
                continue;
            }
            if e.player.is_some() || e.is_weapon() || !e.weapon_owner.is_null() || e.class == "predicted_viewmodel" {
                continue;
            }
            // physics props; scripted "anim" entities (the client runs their Lua);
            // everything else with a studio model is shown as a dynamic prop (NPCs have
            // no AI here, so they stand still)
            let class = if e.class.starts_with("prop_physics") {
                entities::CLASS_PHYSICS_PROP
            } else if e.scripted {
                entities::CLASS_SENT_ANIM
            } else {
                entities::CLASS_DYNAMIC_PROP
            };
            let Some(model_index) = game.precache_model(&e.model) else { continue };
            let owner = if e.owner.is_null() {
                entities::INVALID_EHANDLE
            } else {
                entities::ehandle(e.owner.id, e.owner.serial)
            };
            let parent = if e.parent.is_null() {
                entities::INVALID_EHANDLE
            } else {
                entities::ehandle(e.parent.id, e.parent.serial)
            };
            let mut state = entities::model_entity(
                &game.net,
                class,
                &entities::PropState {
                    parent,
                    index: e.r.id,
                    serial: e.r.serial,
                    model_index,
                    origin: e.pos,
                    angles: e.ang,
                    velocity: e.vel,
                    skin: e.skin,
                    body: 0,
                    color: e.color,
                    render_mode: e.render_mode,
                    render_fx: e.render_fx,
                    effects: e.effects | if e.no_draw { 0x20 } else { 0 },
                    collision_group: e.collision_group,
                    solid: if e.solid == 0 { 6 } else { e.solid },
                    mins: e.mins,
                    maxs: e.maxs,
                    move_type: if e.move_type == 0 { 6 } else { e.move_type },
                    owner,
                    sim_tick: match e.nw.get("__srcds_simtick") {
                        Some(srcds_lua::entity::NetValue::Int(t)) => *t as i32,
                        _ => tick,
                    },
                    awake: e.phys.as_ref().is_some_and(|p| !p.asleep && p.motion),
                    sequence: e.sequence,
                    playback_rate: e.playback_rate,
                },
                &e.class,
            );
            entities::set_dtvars(&game.net, &mut state, &dtvars(e));
            out.push(state);
        }
        out
    }

    /// Weapons and view models of all players.
    pub fn network_weapons(&self, game: &GameInfo, tick: i32) -> Vec<entities::EntityState> {
        let mut ws = self.weapons.borrow_mut();
        let mut models = self.weapon_models.borrow_mut();
        ws.network(&self.glua, game, &mut models, tick).0
    }

    pub fn disconnect(&self, c: &Client) {
        if let Some(r) = c.lua_ent {
            self.weapons.borrow_mut().release(&self.glua, r);
            self.glua.player_disconnect(r);
        }
    }

    /// The `noclip` command: asks `PlayerNoClip` like the engine does.
    pub fn allow_noclip(&self, r: EntRef, desired: bool) -> bool {
        let out = self.glua.hook_call("PlayerNoClip", (r, desired));
        matches!(out.into_iter().next(), Some(mlua_bool) if mlua_bool.as_boolean() == Some(true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use srcds_lua::vecang::Vector;

    fn has_key(lua: &LuaGame, r: srcds_lua::entity::EntRef, k: &str) -> bool {
        store(&lua.glua.lua).unwrap().get(r).is_some_and(|e| e.nw.contains_key(k))
    }

    /// Real gm_construct + sandbox (`SRCDS_RS_GMOD_DIR`): a player joins, runs
    /// `gm_spawn`, the prop is networked with a dynamic model index and lands.
    #[test]
    fn sandbox_prop_spawn_end_to_end() {
        let Ok(root) = std::env::var("SRCDS_RS_GMOD_DIR") else { return };
        let dir = Path::new(&root);
        let (map, world) = crate::bsp::load_with_collision(dir, "gm_construct").unwrap();
        let game = GameInfo {
            map,
            host_name: "t".into(),
            game_dir: "garrysmod".into(),
            gamemode: "sandbox".into(),
            max_clients: 8,
            tick_interval: 0.015,
            server_count: 1,
            query_port: 27015,
            client_lua_files: Vec::new(),
            full_signon: true,
            net: entities::NetClasses::new().unwrap(),
            movement: Default::default(),
            world: Arc::new(world),
            dynamic: Default::default(),
            solids: Default::default(),
            ladders: Default::default(),
            decals: Default::default(),
            events: Default::default(),
        };
        let lua = LuaGame::start(dir, &game).expect("lua");
        let now = std::time::Instant::now();
        let mut c = Client::new("127.0.0.1:27005".parse().unwrap(), 0, 2, "tester".into(), Some(76561197960265729), 1, now);
        c.spawn(&game, 1);
        c.signon = srcds_protocol::netmsg::SIGNONSTATE_FULL;
        c.user_info.insert("cl_playercolor".into(), "1 0 0".into());
        for t in 0..5 {
            lua.tick(&game, std::iter::once(&mut c), t);
        }
        let _ply = c.lua_ent.expect("player entity");
        // handles to the player are built from the Lua entity: same serial as networked
        assert_eq!(_ply.serial, c.serial);
        // the spawn read the client's colour
        let red: bool = lua.glua.lua.load("return player.GetAll()[1]:GetPlayerColor() == Vector(1, 0, 0)").eval().unwrap();
        assert!(red, "player colour from user info at spawn");
        assert!(game.decals.borrow().iter().any(|d| d == "decals/concrete/shot1_subrect"), "decal materials precached");
        // the loadout's weapons have their clips and the player reserve ammo
        let (clip, max, ammo_id, reserve): (i64, i64, i64, i64) = lua
            .glua
            .lua
            .load(
                "local p = player.GetAll()[1] local w = p:GetWeapon('weapon_pistol')
                 return w:Clip1(), w:GetMaxClip1(), w:GetPrimaryAmmoType(), p:GetAmmoCount('Pistol')",
            )
            .eval()
            .unwrap();
        eprintln!("pistol clip {clip}/{max} ammo type {ammo_id} reserve {reserve}");
        assert!(clip > 0 && clip == max, "pistol clip {clip}/{max}");
        assert_eq!(ammo_id, 3);
        assert!(reserve > 0);
        // the hands: a gmod_hands entity with a model, parented to the view model
        let (hands_ok, hands_model): (bool, String) = lua
            .glua
            .lua
            .load(
                "local p = player.GetAll()[1] local h = p:GetHands()
                 return IsValid(h) and h:GetParent() == p:GetViewModel(0), IsValid(h) and h:GetModel() or ''",
            )
            .eval()
            .unwrap();
        // weapon logic per command: idle, fire, reload (pistol, c_pistol.mdl)
        {
            let pistol: srcds_lua::entity::EntRef = lua.glua.lua.load("return player.GetAll()[1]:GetWeapon('weapon_pistol')").eval().unwrap();
            let mut models = lua.weapon_models.borrow_mut();
            let mut ws = lua.weapons.borrow_mut();
            assert!(ws.select(&lua.glua, &mut models, _ply, pistol.id, 10.0));
            let vm = srcds_lua::entity::with_player(&lua.glua.lua, _ply, |p, _| p.view_model).unwrap();
            let nw_i = |r: srcds_lua::entity::EntRef, k: &str| match store(&lua.glua.lua).unwrap().get(r).and_then(|e| e.nw.get(k).cloned()) {
                Some(srcds_lua::entity::NetValue::Int(n)) => n,
                _ => -1,
            };
            let nw_f = |r: srcds_lua::entity::EntRef, k: &str| match store(&lua.glua.lua).unwrap().get(r).and_then(|e| e.nw.get(k).cloned()) {
                Some(srcds_lua::entity::NetValue::Float(f)) => f,
                _ => -1.0,
            };
            assert_eq!(nw_i(vm, "__srcds_vm_seq"), 7, "draw");
            let cmd = |t: f64, b: i32, old: i32| crate::weapon_sim::WeaponCmd { time: t, buttons: b, old_buttons: old, eye: [0.0, 0.0, 64.0], angles: [0.0; 3] };
            // after the draw, idle
            let p0 = nw_i(vm, "__srcds_vm_parity");
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(11.0, 0, 0));
            assert_eq!(nw_i(vm, "__srcds_vm_parity"), p0 + 1);
            assert!((nw_f(pistol, "__srcds_idle") - (11.0 + 3.3333)).abs() < 0.01, "idle time {}", nw_f(pistol, "__srcds_idle"));
            // idling again only after the idle animation
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(11.015, 0, 0));
            assert_eq!(nw_i(vm, "__srcds_vm_parity"), p0 + 1);
            // fire: a round spent, fire sequence, refire delay
            let clip0 = srcds_lua::entity::with(&lua.glua.lua, pistol, |e| e.clip1).unwrap();
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(12.0, 1, 0));
            let clip1 = srcds_lua::entity::with(&lua.glua.lua, pistol, |e| e.clip1).unwrap();
            assert_eq!(clip1, clip0 - 1);
            assert_eq!(nw_i(vm, "__srcds_vm_seq"), 2);
            assert!(nw_f(pistol, "__srcds_npf") > 12.0);
            // reload: refills after the reload animation
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(13.0, 1 << 13, 0));
            let end = nw_f(pistol, "__srcds_npf");
            assert!(end > 13.5, "reload end {end}");
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(end + 0.01, 0, 0));
            let clip2 = srcds_lua::entity::with(&lua.glua.lua, pistol, |e| e.clip1).unwrap();
            assert_eq!(clip2, clip0, "clip refilled");
            // pistol: clicking fast refires before its 0.5 s rate, counting up the recoil
            let path = crate::weapon_sim::view_model_path(&lua.glua, &mut models, pistol);
            let seq = |m: &mut crate::weapons::WeaponModels, a: &str| m.sequence_info(&path, a).0 as i64;
            let t0 = end + 1.0;
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(t0, 1, 0));
            assert_eq!(nw_i(vm, "__srcds_vm_seq"), seq(&mut models, "ACT_VM_PRIMARYATTACK"));
            assert!((nw_f(pistol, "__srcds_npf") - (t0 + 0.5)).abs() < 1e-6);
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(t0 + 0.015, 0, 1));
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(t0 + 0.12, 0, 0));
            assert!(nw_f(pistol, "__srcds_npf") < t0 + 0.12, "fast refire allowed");
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(t0 + 0.2, 1, 0));
            assert_eq!(nw_i(pistol, "__srcds_numshots"), 1);
            assert_eq!(nw_i(vm, "__srcds_vm_seq"), seq(&mut models, "ACT_VM_RECOIL1"));
            assert!(nw_f(pistol, "__srcds_penalty") > 0.3, "penalty {}", nw_f(pistol, "__srcds_penalty"));
            assert_eq!(nw_i(pistol, "__srcds_shots"), 1);
            // empty and no reserve: the trigger dry-fires
            srcds_lua::entity::with(&lua.glua.lua, pistol, |e| e.clip1 = 0).unwrap();
            srcds_lua::entity::with_player(&lua.glua.lua, _ply, |p, _| {
                p.ammo.insert("Pistol".into(), 0);
            })
            .unwrap();
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(t0 + 2.0, 0, 1));
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(t0 + 3.0, 1, 0));
            assert_eq!(nw_i(vm, "__srcds_vm_seq"), seq(&mut models, "ACT_VM_DRYFIRE"));
            // released, empty: the empty idle
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(t0 + 6.0, 0, 1));
            assert_eq!(nw_i(vm, "__srcds_vm_seq"), seq(&mut models, "ACT_VM_IDLE_EMPTY"));
            // SMG1: held fire counts the burst (recoil animations), alt-fire 0.5 / 1 s
            let smg: srcds_lua::entity::EntRef = lua.glua.lua.load("local p = player.GetAll()[1] p:Give('weapon_smg1') p:SetAmmo(3, 'SMG1_Grenade') return p:GetWeapon('weapon_smg1')").eval().unwrap();
            assert!(ws.select(&lua.glua, &mut models, _ply, smg.id, 40.0));
            let spath = crate::weapon_sim::view_model_path(&lua.glua, &mut models, smg);
            let sclip = srcds_lua::entity::with(&lua.glua.lua, smg, |e| e.clip1).unwrap();
            for (i, t) in [41.0, 41.08, 41.16].iter().enumerate() {
                ws.run_command(&lua.glua, &mut models, _ply, &cmd(*t, 1, if i == 0 { 0 } else { 1 }));
            }
            assert_eq!(srcds_lua::entity::with(&lua.glua.lua, smg, |e| e.clip1).unwrap(), sclip - 3);
            assert_eq!(nw_i(smg, "__srcds_shots"), 3);
            assert_eq!(nw_i(vm, "__srcds_vm_seq"), models.sequence_info(&spath, "ACT_VM_RECOIL3").0 as i64);
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(42.0, 1 << 11, 0));
            assert!((nw_f(smg, "__srcds_nsf") - 43.0).abs() < 1e-6 && (nw_f(smg, "__srcds_npf") - 42.5).abs() < 1e-6, "smg grenade times");
            let left: i32 = lua.glua.lua.load("return player.GetAll()[1]:GetAmmoCount('SMG1_Grenade')").eval().unwrap();
            assert_eq!(left, 2);
            // AR2: alt-fire charges for half a second, then fires and waits a second
            let ar2: srcds_lua::entity::EntRef = lua.glua.lua.load("local p = player.GetAll()[1] p:Give('weapon_ar2') p:SetAmmo(2, 'AR2AltFire') return p:GetWeapon('weapon_ar2')").eval().unwrap();
            assert!(ws.select(&lua.glua, &mut models, _ply, ar2.id, 50.0));
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(51.0, 1 << 11, 0));
            assert!((nw_f(ar2, "__srcds_npf") - 51.5).abs() < 1e-6, "ar2 charging");
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(51.515, 0, 1 << 11));
            assert!((nw_f(ar2, "__srcds_npf") - 52.515).abs() < 1e-6, "ar2 ball fired");
            let left: i32 = lua.glua.lua.load("return player.GetAll()[1]:GetAmmoCount('AR2AltFire')").eval().unwrap();
            assert_eq!(left, 1);
            // shotgun: fire → pump → shell-by-shell reload; the trigger interrupts it
            let sg: srcds_lua::entity::EntRef = lua.glua.lua.load("local p = player.GetAll()[1] p:Give('weapon_shotgun') p:SetAmmo(20, 'Buckshot') return p:GetWeapon('weapon_shotgun')").eval().unwrap();
            assert!(ws.select(&lua.glua, &mut models, _ply, sg.id, 60.0));
            let gpath = crate::weapon_sim::view_model_path(&lua.glua, &mut models, sg);
            for a in ["ACT_SHOTGUN_PUMP", "ACT_SHOTGUN_RELOAD_START", "ACT_SHOTGUN_RELOAD_FINISH", "ACT_VM_RELOAD"] {
                assert!(models.sequence_info(&gpath, a).0 >= 0, "{a} in {gpath}");
            }
            let c0 = srcds_lua::entity::with(&lua.glua.lua, sg, |e| e.clip1).unwrap();
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(61.0, 1, 0));
            assert_eq!(srcds_lua::entity::with(&lua.glua.lua, sg, |e| e.clip1).unwrap(), c0 - 1);
            assert_eq!(nw_i(sg, "__srcds_sg_pump"), 1);
            let t1 = nw_f(sg, "__srcds_npf") + 0.001;
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(t1, 0, 1));
            assert_eq!(nw_i(sg, "__srcds_sg_pump"), 0);
            assert_eq!(nw_i(vm, "__srcds_vm_seq"), models.sequence_info(&gpath, "ACT_SHOTGUN_PUMP").0 as i64);
            let t2 = nw_f(sg, "__srcds_npf") + 0.001;
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(t2, 1 << 13, 0));
            assert_eq!(nw_i(sg, "__srcds_in_reload"), 1, "reload started");
            let t3 = nw_f(sg, "__srcds_npf") + 0.001;
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(t3, 0, 0));
            assert_eq!(srcds_lua::entity::with(&lua.glua.lua, sg, |e| e.clip1).unwrap(), c0, "one shell in");
            // trigger during the reload: fires once the shell animation is done
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(t3 + 0.015, 1, 0));
            assert!(!has_key(&lua, sg, "__srcds_in_reload"));
            let t4 = nw_f(sg, "__srcds_npf") + 0.001;
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(t4, 1, 1));
            assert_eq!(srcds_lua::entity::with(&lua.glua.lua, sg, |e| e.clip1).unwrap(), c0 - 1, "delayed shot");
            // crossbow: a shot, then the bolt reloads right away
            let xb: srcds_lua::entity::EntRef = lua.glua.lua.load("local p = player.GetAll()[1] p:Give('weapon_crossbow') p:SetAmmo(5, 'XBowBolt') return p:GetWeapon('weapon_crossbow')").eval().unwrap();
            assert!(ws.select(&lua.glua, &mut models, _ply, xb.id, 70.0));
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(71.0, 1, 0));
            assert_eq!(nw_i(xb, "__srcds_xb_reload"), 1);
            // the SMG1 grenade and the bolt fly as entities; the grenade bursts on landing
            let kinds: Vec<_> = ws.projectiles.iter().map(|p| p.kind).collect();
            assert_eq!(kinds, vec![crate::weapon_sim::ProjectileKind::Grenade, crate::weapon_sim::ProjectileKind::Bolt]);
            for i in 0..300 {
                ws.step_projectiles(&lua.glua, 600.0, 0.015, 71.0 + i as f64 * 0.015);
            }
            assert!(!ws.projectiles.iter().any(|p| p.kind == crate::weapon_sim::ProjectileKind::Grenade), "grenade exploded");
            // a bolt in open air flies 3500 u/s
            let open = crate::weapon_sim::WeaponCmd { time: 75.0, buttons: 0, old_buttons: 0, eye: [704.0, 132.0, -79.0], angles: [0.0, 180.0, 0.0] };
            ws.launch(&lua.glua, _ply, &open, crate::weapon_sim::ProjectileKind::Bolt);
            let bolt = ws.projectiles.last().unwrap().ent;
            let b0 = srcds_lua::entity::with(&lua.glua.lua, bolt, |e| e.pos).unwrap();
            for i in 0..4 {
                ws.step_projectiles(&lua.glua, 600.0, 0.015, 75.0 + i as f64 * 0.015);
            }
            let b1 = srcds_lua::entity::with(&lua.glua.lua, bolt, |e| e.pos).unwrap_or(b0);
            eprintln!("bolt {b0:?} -> {b1:?}");
            assert!(b0[0] - b1[0] > 150.0, "bolt flew");
            // a grenade lobbed at the floor bursts there (well before it would expire)
            let lob = crate::weapon_sim::WeaponCmd { time: 76.0, buttons: 0, old_buttons: 0, eye: [704.0, 132.0, -79.0], angles: [45.0, 180.0, 0.0] };
            ws.launch(&lua.glua, _ply, &lob, crate::weapon_sim::ProjectileKind::Grenade);
            for i in 0..40 {
                ws.step_projectiles(&lua.glua, 600.0, 0.015, 76.0 + i as f64 * 0.015);
            }
            assert!(!ws.projectiles.iter().any(|p| p.kind == crate::weapon_sim::ProjectileKind::Grenade), "grenade burst on the floor");
            assert!((nw_f(xb, "__srcds_npf") - 71.75).abs() < 1e-6);
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(71.2, 0, 1));
            assert_eq!(nw_i(xb, "__srcds_xb_reload"), 0, "bolt reloading");
            assert!(has_key(&lua, xb, "__srcds_in_reload"));
            // crowbar: a swing every 0.4 s while held
            let cb: srcds_lua::entity::EntRef = lua.glua.lua.load("local p = player.GetAll()[1] p:Give('weapon_crowbar') return p:GetWeapon('weapon_crowbar')").eval().unwrap();
            assert!(ws.select(&lua.glua, &mut models, _ply, cb.id, 80.0));
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(81.0, 1, 0));
            assert!((nw_f(cb, "__srcds_npf") - (81.0 + 0.4f32 as f64)).abs() < 1e-6, "crowbar refire");
        }
        eprintln!("hands valid+parented {hands_ok} model {hands_model}");
        assert!(hands_ok && hands_model.ends_with(".mdl"), "hands {hands_ok} {hands_model}");
        let hands: srcds_lua::entity::EntRef = lua.glua.lua.load("return player.GetAll()[1]:GetHands()").eval().unwrap();
        let states = lua.network_entities(&game, 250);
        let hs = states.iter().find(|e| e.index == hands.id).expect("hands networked");
        let fx = game.net.flat[hs.class as usize].iter().position(|p| p.name == "m_fEffects").unwrap();
        let par = game.net.flat[hs.class as usize].iter().position(|p| p.name == "moveparent").unwrap();
        eprintln!("hands class {} effects {:?} parent {:?}", hs.class, hs.props[fx], hs.props[par]);
        // look down a bit so the spawn trace hits the ground in front
        c.pawn.as_mut().unwrap().angles = [45.0, 0.0, 0.0];
        c.lua_commands.push("gm_spawn models/props_c17/oildrum001.mdl".into());
        for t in 5..200 {
            lua.tick(&game, std::iter::once(&mut c), t);
        }
        let s = lua.glua.state.borrow();
        for e in s.error_log.iter().take(20) {
            eprintln!("ERR {e}");
        }
        drop(s);
        // the gravity gun: effect states, dry fire, claws opening at the drum, a punt
        {
            let cannon: srcds_lua::entity::EntRef = lua.glua.lua.load("return player.GetAll()[1]:GetWeapon('weapon_physcannon')").eval().unwrap();
            let drum: srcds_lua::entity::EntRef = lua.glua.lua.load("local t = ents.FindByClass('prop_physics') return t[#t]").eval().unwrap();
            let mut models = lua.weapon_models.borrow_mut();
            let mut ws = lua.weapons.borrow_mut();
            assert!(ws.select(&lua.glua, &mut models, _ply, cannon.id, 20.0));
            let vm = srcds_lua::entity::with_player(&lua.glua.lua, _ply, |p, _| p.view_model).unwrap();
            let path = crate::weapon_sim::view_model_path(&lua.glua, &mut models, cannon);
            let nw_i = |r: srcds_lua::entity::EntRef, k: &str| match store(&lua.glua.lua).unwrap().get(r).and_then(|e| e.nw.get(k).cloned()) {
                Some(srcds_lua::entity::NetValue::Int(n)) => n,
                _ => -1,
            };
            let nw_f = |r: srcds_lua::entity::EntRef, k: &str| match store(&lua.glua.lua).unwrap().get(r).and_then(|e| e.nw.get(k).cloned()) {
                Some(srcds_lua::entity::NetValue::Float(f)) => f,
                _ => -1.0,
            };
            let cmd = |t: f64, b: i32, old: i32, pitch: f32| crate::weapon_sim::WeaponCmd { time: t, buttons: b, old_buttons: old, eye: [704.0, 132.0, -79.0], angles: [pitch, 0.0, 0.0] };
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(21.0, 0, 0, -80.0));
            assert_eq!(nw_i(cannon, "__srcds_pc_effect"), 2, "ready");
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(21.1, 1 << 11, 0, -80.0));
            assert_eq!(nw_i(cannon, "__srcds_pc_effect"), 3, "pulling");
            assert!((nw_f(cannon, "__srcds_nsf") - 21.2).abs() < 1e-6);
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(21.2, 1, 1 << 11, -80.0));
            assert_eq!(nw_i(vm, "__srcds_vm_seq"), models.sequence_info(&path, "ACT_VM_PRIMARYATTACK").0 as i64, "dry fire");
            assert!((nw_f(cannon, "__srcds_npf") - 21.7).abs() < 1e-6);
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(22.0, 0, 1, 33.0));
            assert_eq!(nw_i(cannon, "__srcds_pc_open"), 1, "claws open at the drum");
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(22.1, 1, 0, 33.0));
            assert_eq!(nw_i(cannon, "__srcds_pc_effect"), 4, "launch");
            assert_eq!(nw_i(vm, "__srcds_vm_seq"), models.sequence_info(&path, "ACT_VM_SECONDARYATTACK").0 as i64, "punt");
            let v = srcds_lua::entity::with(&lua.glua.lua, drum, |e| e.vel).unwrap();
            eprintln!("punted drum velocity {v:?}");
            // put the drum back as it was for the checks below
            srcds_lua::entity::with(&lua.glua.lua, drum, |e| {
                e.vel = [0.0; 3];
                if let Some(p) = &mut e.phys {
                    p.asleep = true;
                }
            })
            .unwrap();
            assert!(v[0] > 100.0);
            // alt-fire at the drum picks it up; it follows the view; alt-fire again drops it
            let drum_pos0 = srcds_lua::entity::with(&lua.glua.lua, drum, |e| e.pos).unwrap();
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(23.0, 1 << 11, 0, 33.0));
            assert!(ws.cannon_holds.contains_key(&_ply), "picked up");
            assert_eq!(nw_i(cannon, "__srcds_pc_effect"), 3, "holding");
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(23.015, 1 << 11, 1 << 11, 0.0));
            let held = srcds_lua::entity::with(&lua.glua.lua, drum, |e| e.pos).unwrap();
            eprintln!("drum {drum_pos0:?} -> held at {held:?}");
            assert!(held[2] > drum_pos0[2] + 20.0, "lifted to eye level");
            let (states, _) = ws.network(&lua.glua, &game, &mut models, 260);
            let ws_state = states.iter().find(|e| e.index == cannon.id).expect("cannon networked");
            let ai = game.net.flat[ws_state.class as usize].iter().position(|p| p.name == "m_hAttachedObject").unwrap();
            assert_eq!(ws_state.props[ai], srcds_protocol::sendtable::PropValue::Int(entities::ehandle(drum.id, drum.serial)));
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(23.1, 0, 1 << 11, 0.0));
            ws.run_command(&lua.glua, &mut models, _ply, &cmd(23.6, 1 << 11, 0, 0.0));
            assert!(!ws.cannon_holds.contains_key(&_ply), "dropped");
            assert_eq!(nw_i(cannon, "__srcds_pc_effect"), 2);
            // put the drum back as it was for the checks below
            srcds_lua::entity::with(&lua.glua.lua, drum, |e| {
                e.pos = drum_pos0;
                e.vel = [0.0; 3];
                if let Some(p) = &mut e.phys {
                    p.asleep = true;
                }
            })
            .unwrap();
        }
        // NPCs and sequences: a citizen gets its idle sequence (not the ragdoll pose)
        let (named, act): (i64, i64) = lua
            .glua
            .lua
            .load(
                "local n = ents.Create('npc_citizen') n:SetModel('models/humans/group01/male_07.mdl') n:SetPos(Vector(0, 0, 0)) n:Spawn()
                 NPC_TEST = n
                 return n:LookupSequence('idle_subtle'), n:SelectWeightedSequence(ACT_IDLE)",
            )
            .eval()
            .unwrap();
        assert_eq!((named, act), (1, 1));
        let states = lua.network_entities(&game, 300);
        let npc: srcds_lua::entity::EntRef = lua.glua.lua.globals().get("NPC_TEST").unwrap();
        let st = states.iter().find(|e| e.index == npc.id).expect("npc networked");
        let i = game.net.flat[st.class as usize].iter().position(|p| p.name == "m_nSequence").unwrap();
        assert_eq!(st.props[i], srcds_protocol::sendtable::PropValue::Int(1), "idle sequence");
        // map brush entities (func_brush, doors, illusionary walls) are networked
        let brushes: Vec<_> = states.iter().filter(|e| e.class == entities::CLASS_BRUSH).collect();
        let classes: std::collections::BTreeMap<String, usize> = store(&lua.glua.lua)
            .unwrap()
            .ents
            .values()
            .filter(|e| brush_class(e).is_some())
            .fold(Default::default(), |mut m, e| {
                *m.entry(e.class.clone()).or_default() += 1;
                m
            });
        eprintln!("brush entities networked: {} {classes:?}", brushes.len());
        assert!(!brushes.is_empty());
        assert!(game.solids.borrow().iter().any(|s| matches!(s, crate::movement::SolidBox::Brush { .. })), "solid brush entities collide");
        let ents = lua.network_entities(&game, 200);
        let mine: Vec<_> = {
            let st = store(&lua.glua.lua).unwrap();
            ents.iter()
                .filter(|e| e.class == entities::CLASS_PHYSICS_PROP && st.ents.get(&e.index).is_some_and(|d| !d.map_entity))
                .cloned()
                .collect()
        };
        assert_eq!(mine.len(), 1, "spawned props: {}", mine.len());
        assert!(ents.iter().any(|e| e.class == entities::CLASS_SENT_ANIM), "gmod_hands is networked");
        let m = mine[0].model_index(&game.net).unwrap();
        assert!(m > game.player_model_index(), "dynamic model index {m}");
        let st = store(&lua.glua.lua).unwrap();
        let d = st.ents.get(&mine[0].index).unwrap();
        eprintln!("prop at {:?} bounds {:?}..{:?} asleep {:?} player at {:?}", d.pos, d.mins, d.maxs, d.phys.as_ref().map(|p| p.asleep), c.pawn.as_ref().unwrap().mv.origin);
        assert!(d.phys.as_ref().unwrap().asleep, "prop came to rest");
        drop(st);
        let prop_index = mine[0].index;
        let mut all = ents.clone();
        all.extend(lua.network_weapons(&game, 200));
        all.push(c.player_entity(&game).unwrap());
        all.sort_by_key(|e| e.index);
        let weapons = all.iter().filter(|e| e.class >= entities::CLASS_WEAPON_FIRST).count();
        assert!(weapons >= 10, "weapons networked: {weapons}");
        assert!(all.iter().any(|e| e.class == entities::CLASS_VIEWMODEL));
        let count = all.len();
        let snap = entities::Snapshot { tick: 200, entities: all };
        let full = entities::encode(&game.net, &snap, None).unwrap();
        let again = entities::encode(&game.net, &snap, Some(&snap)).unwrap();
        eprintln!("snapshot: {count} entities, full {} bytes, unchanged delta {} bytes", full.data.bit_len() / 8, again.data.bit_len() / 8);

        // physics gun: select it, aim at the barrel, grab, turn, release
        let physgun = {
            let st = store(&lua.glua.lua).unwrap();
            let pd = st.get(_ply).unwrap().player.as_ref().unwrap();
            pd.weapons.iter().copied().find(|w| st.get(*w).is_some_and(|e| e.class == "weapon_physgun")).unwrap()
        };
        c.weapon_select = Some(physgun.id);
        lua.tick(&game, std::iter::once(&mut c), 201);
        let target = {
            let st = store(&lua.glua.lua).unwrap();
            let d = st.ents.get(&prop_index).unwrap();
            [d.pos[0], d.pos[1], d.pos[2] + 20.0]
        };
        let eye = {
            let o = c.pawn.as_ref().unwrap().mv.origin;
            [o[0], o[1], o[2] + entities::VIEW_HEIGHT]
        };
        let aim = srcds_lua::vecang::vector_angles([target[0] - eye[0], target[1] - eye[1], target[2] - eye[2]]);
        c.pawn.as_mut().unwrap().angles = aim;
        c.buttons = crate::weapons::IN_ATTACK;
        for t in 202..206 {
            lua.tick(&game, std::iter::once(&mut c), t);
        }
        let dbg: String = lua
            .glua
            .lua
            .load(format!(
                "local p = player.GetAll()[1] local e = Entity({prop_index})
                 local tr = p:GetEyeTrace()
                 return tostring(p:GetActiveWeapon()) .. ' pickup=' .. tostring(hook.Call('PhysgunPickup', GAMEMODE, p, e)) .. ' trace=' .. tostring(tr.Entity) .. ' ' .. tostring(tr.HitPos)"
            ))
            .eval()
            .unwrap();
        eprintln!("dbg {dbg}");
        let before = store(&lua.glua.lua).unwrap().ents.get(&prop_index).unwrap().pos;
        c.pawn.as_mut().unwrap().angles = [aim[0], aim[1] + 30.0, 0.0];
        for t in 206..210 {
            lua.tick(&game, std::iter::once(&mut c), t);
        }
        let held = store(&lua.glua.lua).unwrap().ents.get(&prop_index).unwrap().pos;
        eprintln!("physgun: before {before:?} held {held:?}");
        assert!((held[1] - before[1]).abs() > 10.0, "prop follows the aim");
        c.buttons = 0;
        for t in 210..400 {
            lua.tick(&game, std::iter::once(&mut c), t);
        }
        let after = store(&lua.glua.lua).unwrap().ents.get(&prop_index).unwrap().pos;
        eprintln!("released, rests at {after:?}");
        let s = lua.glua.state.borrow();
        for e in s.error_log.iter().take(20) {
            eprintln!("ERR {e}");
        }
        assert_eq!(s.errors, 0);
        drop(s);
        // constraints through the real constraint.lua: weld to the world, rope
        lua.glua
            .lua
            .load(
                "local function prop(z)
                     local e = ents.Create('prop_physics') e:SetModel('models/props_c17/oildrum001.mdl')
                     e:SetPos(Vector(900, 132, z)) e:Spawn() return e
                 end
                 WELDED = prop(200)
                 assert(constraint.Weld(WELDED, game.GetWorld(), 0, 0, 0, false), 'weld')
                 HANGING = prop(100)
                 local top = Vector(1000, 132, 300)
                 assert(constraint.Rope(game.GetWorld(), HANGING, 0, 0, top, Vector(0, 0, 40), 50, 0, 0, 1, 'cable/rope', false), 'rope')
                 HANGING:SetPos(Vector(1000, 132, 250))",
            )
            .exec()
            .unwrap();
        for t in 300..400 {
            lua.tick(&game, std::iter::once(&mut c), t);
        }
        let (wz, hpos): (f32, [f32; 3]) = {
            let v: (Vector, Vector) = lua.glua.lua.load("return WELDED:GetPos(), HANGING:GetPos()").eval().unwrap();
            (v.0 .0[2], v.1 .0)
        };
        eprintln!("welded z {wz}, hanging at {hpos:?}");
        assert!((wz - 200.0).abs() < 0.5, "welded prop stays in the air");
        // anchor (local +40 z) at most 50 below the rope top at z=300
        let anchor_z = hpos[2] + 40.0;
        assert!(anchor_z > 300.0 - 52.0 && anchor_z < 300.0, "rope holds: anchor at {anchor_z}");
        assert_eq!(lua.glua.state.borrow().errors, 0, "{:?}", lua.glua.state.borrow().error_log);
        let ropes: Vec<_> = lua.network_entities(&game, 400).into_iter().filter(|e| e.class == entities::CLASS_ROPE).collect();
        assert_eq!(ropes.len(), 1, "the rope is networked");
        entities::encode(&game.net, &entities::Snapshot { tick: 400, entities: ropes }, None).unwrap();

        // suicide: dead pawn, weapons gone
        c.lua_commands.push("kill".into());
        lua.tick(&game, std::iter::once(&mut c), 400);
        assert_eq!(c.pawn.as_ref().unwrap().health, 0);
        assert!(c.pawn.as_ref().unwrap().inventory.weapons.is_empty());
        assert_eq!(lua.glua.state.borrow().errors, 0, "{:?}", lua.glua.state.borrow().error_log);
        // string-table update for the new model
        let mut w = srcds_protocol::bitbuf::BitWriter::with_capacity(4096);
        let now = crate::signon::table_updates(&game, (0, 0), &mut w).unwrap();
        assert!(now.0 >= 1 && w.bit_len() > 0);
    }
}