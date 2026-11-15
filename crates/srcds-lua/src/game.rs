//! Host-facing game flow: map entities, player connect/spawn/disconnect, client
//! commands and chat, and the per-tick think (timers, `Think`/`Tick` hooks, entity
//! and weapon thinks). Hook order follows the GMod server.

use mlua::{Function, IntoLuaMulti, MultiValue, Table, Value};
use tracing::{info, warn};

use crate::entity::{self, apply_keyvalue, store, with, with_player, EntRef, PlayerData};
use crate::runtime::{report, GLua};

pub const IN_ATTACK: i32 = 1 << 0;
pub const IN_ATTACK2: i32 = 1 << 11;
pub const IN_RELOAD: i32 = 1 << 13;

/// Splits a console line into arguments like the engine tokenizer (quoted strings
/// are one argument; quotes are removed).
pub fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = line.chars().peekable();
    loop {
        while it.peek().is_some_and(|c| c.is_whitespace()) {
            it.next();
        }
        let Some(&c) = it.peek() else { break };
        let mut tok = String::new();
        if c == '"' {
            it.next();
            for c in it.by_ref() {
                if c == '"' {
                    break;
                }
                tok.push(c);
            }
        } else {
            while let Some(&c) = it.peek() {
                if c.is_whitespace() || c == '"' {
                    break;
                }
                tok.push(c);
                it.next();
            }
        }
        out.push(tok);
    }
    out
}

impl GLua {
    /// `hook.Call(name, GAMEMODE, ...)`; errors are reported and yield no values.
    pub fn hook_call(&self, name: &str, args: impl IntoLuaMulti) -> MultiValue {
        let run = || -> mlua::Result<MultiValue> {
            let g = self.lua.globals();
            let hook: Table = g.get("hook")?;
            let call: Function = hook.get("Call")?;
            let gm: Value = g.get("GAMEMODE")?;
            let mut a = args.into_lua_multi(&self.lua)?;
            a.push_front(gm);
            a.push_front(Value::String(self.lua.create_string(name)?));
            call.call::<MultiValue>(a)
        };
        match run() {
            Ok(v) => v,
            Err(e) => {
                report(&self.state, &e);
                MultiValue::new()
            }
        }
    }

    /// Engine start after the Lua boot: gamemode hooks, map entities, `InitPostEntity`.
    pub fn start_game(&self, max_clients: u32, reserved: Vec<u32>, map_entities: &[Vec<(String, String)>]) {
        if let Ok(mut s) = store(&self.lua) {
            s.max_clients = max_clients;
            s.reserved = reserved;
        }
        self.hook_call("OnGamemodeLoaded", ());
        self.hook_call("PostGamemodeLoaded", ());
        self.hook_call("Initialize", ());
        // world first, then map entities in lump order
        let mut created = 0usize;
        for (i, kv) in map_entities.iter().enumerate() {
            let class = kv
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("classname"))
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            let r = {
                let Ok(mut s) = store(&self.lua) else { return };
                if i == 0 {
                    s.create("worldspawn", Some(0))
                } else {
                    s.create(&class, None)
                }
            };
            let Some(r) = r else { break };
            let _ = with(&self.lua, r, |e| {
                for (k, v) in kv {
                    if !k.eq_ignore_ascii_case("classname") {
                        apply_keyvalue(e, k, v);
                    }
                }
                e.map_entity = true;
                e.spawned = true;
                e.model_dirty = false;
                // the world has a static physics object (constraints attach to it)
                if e.r.id == 0 {
                    let mut p = entity::PhysData::default();
                    p.motion = false;
                    e.phys = Some(p);
                }
            });
            // bounds of studio models
            if let Ok(m) = with(&self.lua, r, |e| e.model.clone()) {
                if m.ends_with(".mdl") {
                    let _ = entity::set_model(&self.lua, r, &m);
                    if class.starts_with("prop_physics") {
                        let _ = with(&self.lua, r, |e| {
                            e.solid = 6;
                            e.move_type = 6;
                            e.phys = Some(entity::PhysData::default());
                        });
                    }
                }
            }
            created += 1;
        }
        info!(map_entities = created, "map entities created");
        self.hook_call("InitPostEntity", ());
    }

    /// A client finished signon: creates its player entity (index slot + 1) and runs
    /// the connect / initial-spawn hooks, then spawns it.
    pub fn player_connect(&self, slot: u32, user_id: i32, name: &str, steam_id64: u64, bot: bool, ip: &str) -> EntRef {
        self.player_connect_with_serial(slot, user_id, name, steam_id64, bot, ip, None, &Default::default())
    }

    /// [`Self::player_connect`] with the serial the network uses for the player's
    /// entity: every handle to the player (weapon / view model / hands owner) is built
    /// from the Lua entity, so the two must agree ([факт] live client: with different
    /// serials the client did not see its weapons as its own — no predicted shots,
    /// the world model drawn under the player).
    #[allow(clippy::too_many_arguments)]
    /// `info`: the client's user-info convars so far (sandbox reads `cl_playercolor`,
    /// `cl_playermodel`, … in the spawn hooks).
    #[allow(clippy::too_many_arguments)]
    pub fn player_connect_with_serial(
        &self,
        slot: u32,
        user_id: i32,
        name: &str,
        steam_id64: u64,
        bot: bool,
        ip: &str,
        serial: Option<u32>,
        info: &std::collections::HashMap<String, String>,
    ) -> EntRef {
        let r = {
            let Ok(mut s) = store(&self.lua) else { return EntRef::NULL };
            if s.ents.contains_key(&(slot + 1)) {
                return s.player_by_slot(slot).unwrap_or(EntRef::NULL);
            }
            match s.create("player", Some(slot + 1)) {
                Some(mut r) => {
                    if let Some(serial) = serial {
                        r.serial = serial;
                    }
                    let e = s.ents.get_mut(&r.id).unwrap();
                    e.r = r;
                    let mut pd = PlayerData::new(slot, user_id, name, steam_id64, bot);
                    pd.info = info.clone();
                    e.player = Some(pd);
                    e.mins = [-16.0, -16.0, 0.0];
                    e.maxs = [16.0, 16.0, 72.0];
                    e.solid = 2;
                    e.move_type = 2;
                    e.nw.insert("__srcds_ip".into(), entity::NetValue::Str(ip.to_string()));
                    r
                }
                None => return EntRef::NULL,
            }
        };
        // the player's view model (index after the player slots)
        if let Ok(mut s) = store(&self.lua) {
            if let Some(vm) = s.create("predicted_viewmodel", None) {
                let e = s.get_mut(vm).unwrap();
                e.owner = r;
                e.spawned = true;
                if let Some(p) = s.get_mut(r).and_then(|e| e.player.as_mut()) {
                    p.view_model = vm;
                }
            }
        }
        self.hook_call("PlayerConnect", (name.to_string(), ip.to_string()));
        let sid = if bot {
            "BOT".to_string()
        } else {
            let account = (steam_id64 & 0xFFFF_FFFF) as u32;
            format!("STEAM_0:{}:{}", account & 1, account >> 1)
        };
        let uid = entity::crc32(format!("gm_{steam_id64}_gm").as_bytes()).to_string();
        self.hook_call("PlayerAuthed", (r, sid.clone(), uid));
        self.hook_call("NetworkIDValidated", (name.to_string(), sid));
        self.hook_call("PlayerInitialSpawn", (r, false));
        self.player_spawn(r);
        r
    }

    /// (Re)spawns a player: `PlayerSelectSpawn` for the position, then `PlayerSpawn`.
    pub fn player_spawn(&self, r: EntRef) {
        let now = store(&self.lua).map(|s| s.cur_time).unwrap_or(0.0);
        let ok = with(&self.lua, r, |e| {
            e.health = 100;
            e.max_health = 100;
            e.move_type = 2;
            e.vel = [0.0; 3];
            e.nw.remove("__srcds_respawn");
            if let Some(p) = &mut e.player {
                p.alive = true;
                p.armor = 0;
                p.last_spawn_time = now;
            }
        });
        if ok.is_err() {
            return;
        }
        let spot = self.hook_call("PlayerSelectSpawn", (r, false));
        if let Some(Value::UserData(ud)) = spot.into_iter().next() {
            if let Ok(s) = ud.borrow::<entity::EntUd>() {
                let s = s.0;
                if let Ok((pos, ang)) = with(&self.lua, s, |e| (e.pos, e.ang)) {
                    let _ = with(&self.lua, r, |e| {
                        e.pos = pos;
                        if let Some(p) = &mut e.player {
                            p.eye_angles = [0.0, ang[1], 0.0];
                            p.eye_angles_set = true;
                            p.teleport = true;
                        }
                    });
                }
            }
        }
        self.hook_call("PlayerSpawn", (r, false));
    }

    /// Kills a player: `PlayerDeath` (or `PlayerSilentDeath`), weapons removed,
    /// `PostPlayerDeath`; `PlayerDeathThink` then decides when to respawn.
    pub fn player_kill(&self, r: EntRef, silent: bool) {
        let alive = with_player(&self.lua, r, |p, _| p.alive).unwrap_or(false);
        if !alive {
            return;
        }
        let _ = with(&self.lua, r, |e| {
            e.health = 0;
            e.nw.remove("__srcds_die");
            if let Some(p) = &mut e.player {
                p.alive = false;
                p.deaths += 1;
            }
        });
        if silent {
            self.hook_call("PlayerSilentDeath", r);
        } else {
            self.hook_call("PlayerDeath", (r, r, r));
        }
        let weapons = with_player(&self.lua, r, |p, _| {
            p.active_weapon = EntRef::NULL;
            std::mem::take(&mut p.weapons)
        })
        .unwrap_or_default();
        for w in weapons {
            entity::remove(&self.lua, w);
        }
        self.hook_call("PostPlayerDeath", r);
    }

    /// The engine `kill` / `explode` commands (asks `CanPlayerSuicide`).
    pub fn player_suicide(&self, r: EntRef) {
        let out = self.hook_call("CanPlayerSuicide", r);
        if matches!(out.into_iter().next(), Some(Value::Boolean(false))) {
            return;
        }
        self.player_kill(r, false);
    }

    pub fn player_disconnect(&self, r: EntRef) {
        if !entity::valid(&self.lua, r) {
            return;
        }
        self.hook_call("PlayerDisconnected", r);
        let mut weapons = with_player(&self.lua, r, |p, _| std::mem::take(&mut p.weapons)).unwrap_or_default();
        if let Ok(vm) = with_player(&self.lua, r, |p, _| p.view_model) {
            weapons.push(vm);
        }
        for w in weapons {
            entity::remove(&self.lua, w);
        }
        entity::remove(&self.lua, r);
    }

    /// A console command from a client (`StringCmd`) for Lua `concommand`s.
    /// Returns false when no Lua command of that name exists.
    pub fn client_command(&self, r: EntRef, line: &str) -> bool {
        let toks = tokenize(line);
        let Some(cmd) = toks.first() else { return false };
        let arg_str = line.trim_start()[line.trim_start().find(char::is_whitespace).unwrap_or(line.trim_start().len())..]
            .trim()
            .to_string();
        let run = || -> mlua::Result<bool> {
            let cc: Table = self.lua.globals().get("concommand")?;
            let run: Function = cc.get("Run")?;
            let args = self.lua.create_sequence_from(toks[1..].iter().cloned())?;
            let found: Option<bool> = run.call((r, cmd.to_ascii_lowercase(), args, arg_str.clone()))?;
            Ok(found.unwrap_or(false))
        };
        match run() {
            Ok(f) => f,
            Err(e) => {
                report(&self.state, &e);
                true
            }
        }
    }

    /// `say` / `say_team`: runs `PlayerSay`; returns the text to broadcast (empty =
    /// suppressed).
    pub fn player_say(&self, r: EntRef, text: &str, team: bool) -> String {
        let out = self.hook_call("PlayerSay", (r, text.to_string(), team));
        match out.into_iter().next() {
            Some(Value::String(s)) => s.to_string_lossy().to_string(),
            _ => text.to_string(),
        }
    }

    /// One server tick at time `now` (seconds since start).
    pub fn think(&self, now: f64) {
        if let Ok(mut s) = store(&self.lua) {
            s.cur_time = now;
        }
        crate::lib_misc::run_timers(&self.lua, &self.state, now);
        self.hook_call("Tick", ());
        self.hook_call("Think", ());

        // deaths requested by Lua (Player:Kill / KillSilent)
        let dying: Vec<(EntRef, bool)> = store(&self.lua)
            .map(|s| {
                s.players()
                    .filter_map(|e| match e.nw.get("__srcds_die") {
                        Some(entity::NetValue::Int(k)) => Some((e.r, *k == 2)),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (r, silent) in dying {
            self.player_kill(r, silent);
        }
        // dead players: the gamemode decides when to respawn
        let dead: Vec<EntRef> = store(&self.lua)
            .map(|s| s.players().filter(|e| e.player.as_ref().is_some_and(|p| !p.alive)).map(|e| e.r).collect())
            .unwrap_or_default();
        for r in dead {
            self.hook_call("PlayerDeathThink", r);
        }
        // respawn requests (Player:Spawn)
        let respawn: Vec<EntRef> = store(&self.lua)
            .map(|s| {
                s.players()
                    .filter(|e| e.nw.contains_key("__srcds_respawn"))
                    .map(|e| e.r)
                    .collect()
            })
            .unwrap_or_default();
        for r in respawn {
            self.player_spawn(r);
        }

        // motion controllers: (angular, linear, mode) from ENT:PhysicsSimulate(phys, dt)
        let controlled: Vec<EntRef> = store(&self.lua)
            .map(|s| {
                s.ents
                    .values()
                    .filter(|e| e.nw.contains_key("__srcds_motionctl") && e.phys.as_ref().is_some_and(|p| p.motion) && !e.removing)
                    .map(|e| e.r)
                    .collect()
            })
            .unwrap_or_default();
        let dt = self
            .lua
            .app_data_ref::<crate::lib_misc::Engine>()
            .map_or(0.015, |e| e.tick_interval) as f32;
        for r in controlled {
            let Ok(Some(t)) = entity::ent_table(&self.lua, r, false) else { continue };
            let Ok(Value::Function(f)) = t.get::<Value>("PhysicsSimulate") else { continue };
            let phys = match self.lua.create_userdata(entity::PhysRef(r)) {
                Ok(p) => p,
                Err(_) => continue,
            };
            match f.call::<(Value, Value, Option<i64>)>((r, phys, dt as f64)) {
                Ok((ang, lin, mode)) => {
                    let v = |x: &Value| match x {
                        Value::UserData(u) => u.borrow::<crate::vecang::Vector>().map(|v| v.0).unwrap_or([0.0; 3]),
                        _ => [0.0; 3],
                    };
                    let (a, l) = (v(&ang), v(&lin));
                    let mode = mode.unwrap_or(0);
                    let _ = with(&self.lua, r, |e| {
                        let mass = e.phys.as_ref().map_or(1.0, |p| p.mass.max(0.001));
                        // SIM_LOCAL_ACCELERATION 1, SIM_LOCAL_FORCE 2, SIM_GLOBAL_ACCELERATION 3, SIM_GLOBAL_FORCE 4
                        let (lin_w, ang_l) = match mode {
                            1 => (crate::vecang::rotate(l, e.ang), a),
                            2 => (crate::vecang::rotate(entity::scale(l, 1.0 / mass), e.ang), entity::scale(a, 1.0 / mass)),
                            3 => (l, entity::unrotate(a, e.ang)),
                            4 => (entity::scale(l, 1.0 / mass), entity::unrotate(entity::scale(a, 1.0 / mass), e.ang)),
                            _ => ([0.0; 3], [0.0; 3]),
                        };
                        if mode != 0 {
                            e.vel = entity::add(e.vel, entity::scale(lin_w, dt));
                            if let Some(p) = &mut e.phys {
                                p.ang_vel = entity::add(p.ang_vel, entity::scale(ang_l, dt));
                                p.asleep = false;
                            }
                        }
                    });
                }
                Err(e) => report(&self.state, &e),
            }
        }
        // scripted entity thinks
        let thinkers: Vec<EntRef> = store(&self.lua)
            .map(|s| {
                s.ents
                    .values()
                    .filter(|e| e.scripted && e.spawned && !e.removing && e.weapon_owner.is_null())
                    .filter(|e| e.next_think.is_none_or(|t| t <= now))
                    .map(|e| e.r)
                    .collect()
            })
            .unwrap_or_default();
        for r in thinkers {
            let Ok(Some(t)) = entity::ent_table(&self.lua, r, false) else { continue };
            if let Ok(Value::Function(f)) = t.get::<Value>("Think") {
                let _ = with(&self.lua, r, |e| e.next_think = None);
                match f.call::<Value>(r) {
                    Ok(Value::Boolean(true)) => {}
                    Ok(_) => {
                        // without `return true` the next think is the next tick unless set
                    }
                    Err(e) => report(&self.state, &e),
                }
            }
        }

        // weapons of alive players
        let wielders: Vec<(EntRef, EntRef, i32, i32)> = store(&self.lua)
            .map(|s| {
                s.players()
                    .filter_map(|e| {
                        let p = e.player.as_ref()?;
                        (p.alive && !p.active_weapon.is_null()).then_some((e.r, p.active_weapon, p.buttons, p.old_buttons))
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (_ply, w, _buttons, _old) in wielders {
            let Ok(Some(t)) = entity::ent_table(&self.lua, w, false) else { continue };
            // attacks / reload run per user command in the server's weapon logic
            if let Ok(Value::Function(f)) = t.get::<Value>("Think") {
                if let Err(e) = f.call::<()>(w) {
                    report(&self.state, &e);
                }
            }
        }
    }

    /// Drains the ids removed since the last call.
    pub fn take_removed(&self) -> Vec<EntRef> {
        store(&self.lua).map(|mut s| std::mem::take(&mut s.removed)).unwrap_or_default()
    }

    /// Commands queued by `RunConsoleCommand` / `game.ConsoleCommand`.
    pub fn take_server_commands(&self) -> Vec<String> {
        self.lua
            .app_data_mut::<crate::lib_misc::Engine>()
            .map(|mut e| std::mem::take(&mut e.server_commands))
            .unwrap_or_default()
    }
}

#[allow(dead_code)]
fn _w() {
    warn!("");
}

#[cfg(test)]
mod tests {
    use super::tokenize;

    #[test]
    fn tokenizer() {
        assert_eq!(tokenize("gm_spawn \"models/a b.mdl\" 0"), ["gm_spawn", "models/a b.mdl", "0"]);
        assert_eq!(tokenize("  say hi  there "), ["say", "hi", "there"]);
    }

    /// Boots sandbox, connects a player and checks the spawn hooks ran (needs
    /// `SRCDS_RS_GMOD_DIR`).
    #[test]
    fn sandbox_player_spawn() {
        let Ok(root) = std::env::var("SRCDS_RS_GMOD_DIR") else { return };
        let g = crate::GLua::new(std::path::Path::new(&root)).unwrap();
        g.boot("sandbox").unwrap();
        let spawn = vec![
            vec![("classname".to_string(), "worldspawn".to_string())],
            vec![
                ("classname".to_string(), "info_player_start".to_string()),
                ("origin".to_string(), "100 200 300".to_string()),
                ("angles".to_string(), "0 90 0".to_string()),
            ],
        ];
        g.start_game(8, vec![9], &spawn);
        let p = g.player_connect(0, 2, "tester", 76561197960265729, false, "127.0.0.1:27005");
        for t in 1..=10 {
            g.think(t as f64 * 0.015);
        }
        let s = g.state.borrow();
        eprintln!("errors: {}", s.errors);
        for e in s.error_log.iter().take(30) {
            eprintln!("  ERR {e}");
        }
        drop(s);
        for (k, n) in g.missing_report(40) {
            eprintln!("  {n:6} {k}");
        }
        let dbg: String = g
            .lua
            .load(
                "local p = player.GetAll()[1]
                 local ok, err = pcall(function() hook.Call('PlayerSetModel', GAMEMODE, p) end)
                 return tostring(player_manager.GetPlayerClass(p)) .. ' ' .. tostring(ok) .. ' ' .. tostring(err) .. ' ' .. p:GetModel()",
            )
            .eval()
            .unwrap();
        eprintln!("dbg {dbg}");
        assert!(g.client_command(p, "gm_spawn models/props_c17/oildrum001.mdl"));
        g.think(0.5);
        let s = g.state.borrow();
        for e in s.error_log.iter().take(30) {
            eprintln!("  ERR after gm_spawn {e}");
        }
        drop(s);
        let st = crate::entity::store(&g.lua).unwrap();
        let props: Vec<_> = st.ents.values().filter(|e| e.class == "prop_physics").map(|e| (e.r.id, e.model.clone(), e.pos)).collect();
        eprintln!("props {props:?}");
        eprintln!("sendlua {:?}", st.get(p).unwrap().player.as_ref().unwrap().send_lua);
        assert_eq!(props.len(), 1);
        let e = st.get(p).unwrap();
        let pd = e.player.as_ref().unwrap();
        let weapons: Vec<String> = pd.weapons.iter().filter_map(|w| st.get(*w).map(|e| e.class.clone())).collect();
        eprintln!("pos {:?} model {} weapons {:?} walk {} run {}", e.pos, e.model, weapons, pd.walk_speed, pd.run_speed);
        assert_eq!(e.pos, [100.0, 200.0, 300.0]);
        assert!(pd.alive);
        drop(st);
        // scripted entities using DEFINE_BASECLASS / BaseClass.Think: thruster, camera
        let errs = g.state.borrow().errors;
        g.lua
            .load(
                "local t = ents.Create('gmod_thruster') t:SetModel('models/thrusters/jetpack.mdl') t:Spawn()
                 assert(baseclass.Get('base_gmodentity').Think, 'base class registered')
                 local p = player.GetAll()[1] p:SelectWeapon('gmod_camera')",
            )
            .exec()
            .unwrap();
        for t in 20..30 {
            g.think(t as f64 * 0.015);
        }
        let s = g.state.borrow();
        assert_eq!(s.errors, errs, "{:?}", &s.error_log[s.error_log.len().saturating_sub(5)..]);
        drop(s);
        // numpad controls (thrusters, cameras, lamps): the engine reports held-key
        // changes through PlayerButtonDown / Up, sandbox forwards them to numpad
        let key: i64 = g
            .lua
            .load(
                "NUMPAD_HIT = 0
                 numpad.Register('srcds_test', function(pl, x) NUMPAD_HIT = NUMPAD_HIT + x end)
                 numpad.OnDown(player.GetAll()[1], KEY_PAD_1, 'srcds_test', 1)
                 numpad.OnUp(player.GetAll()[1], KEY_PAD_1, 'srcds_test', 10)
                 return KEY_PAD_1",
            )
            .eval()
            .unwrap();
        assert_eq!(key, 38);
        g.hook_call("PlayerButtonDown", (p, key));
        g.hook_call("PlayerButtonUp", (p, key));
        let hit: i64 = g.lua.load("return NUMPAD_HIT").eval().unwrap();
        assert_eq!(hit, 11);
        // camera view: SetViewEntity(self) is the normal view
        let same: bool = g
            .lua
            .load("local p = player.GetAll()[1] p:SetViewEntity(p) return p:GetViewEntity() == p")
            .eval()
            .unwrap();
        assert!(same);
        // EffectData values round-trip (sandbox's freeze / spawn effects use them)
        let ok: bool = g
            .lua
            .load(
                "local e = EffectData() e:SetOrigin(Vector(1, 2, 3)) e:SetScale(2)
                 return e:GetOrigin() == Vector(1, 2, 3) and e:GetScale() == 2 and e:GetMagnitude() == 0
                    and e:GetNormal() == Vector(0, 0, 0) and not IsValid(e:GetEntity())",
            )
            .eval()
            .unwrap();
        assert!(ok);
    }

    /// Exploration: every sandbox tool's server-side LeftClick / RightClick / Reload
    /// on a prop and on the world (`SRCDS_RS_GMOD_DIR`; prints failures).
    #[test]
    fn sandbox_tools_server_side() {
        let Ok(root) = std::env::var("SRCDS_RS_GMOD_DIR") else { return };
        let g = crate::GLua::new(std::path::Path::new(&root)).unwrap();
        g.boot("sandbox").unwrap();
        let spawn = vec![vec![("classname".to_string(), "worldspawn".to_string())], vec![("classname".to_string(), "info_player_start".to_string())]];
        g.start_game(8, vec![9], &spawn);
        let _p = g.player_connect(0, 2, "tester", 76561197960265729, false, "127.0.0.1:27005");
        for t in 1..=5 {
            g.think(t as f64 * 0.015);
        }
        // the client sends every tool's convars as user info: use their defaults
        let defaults: Vec<(String, String)> = g
            .lua
            .load(
                "local p = player.GetAll()[1] p:Give('gmod_tool') local w = p:GetWeapon('gmod_tool') local t = {}
                 for mode, tool in pairs(w.Tool or {}) do for k, v in pairs(tool.ClientConVar or {}) do t[#t + 1] = { mode .. '_' .. k, tostring(v) } end end
                 return t",
            )
            .eval::<Vec<Vec<String>>>()
            .unwrap()
            .into_iter()
            .map(|kv| (kv[0].to_ascii_lowercase(), kv[1].clone()))
            .collect();
        crate::entity::with_player(&g.lua, _p, |pd, _| pd.info.extend(defaults)).unwrap();
        let out: String = g
            .lua
            .load(
                r#"
                local p = player.GetAll()[1]
                p:Give('gmod_tool')
                local w = p:GetWeapon('gmod_tool')
                if not IsValid(w) then return 'no tool weapon' end
                if not w.Tool then return 'no w.Tool' end
                local function prop()
                    local e = ents.Create('prop_physics') e:SetModel('models/props_c17/oildrum001.mdl') e:SetPos(Vector(0, 0, 100)) e:Spawn()
                    return e
                end
                local out = {}
                local modes = {}
                for mode in pairs(w.Tool) do modes[#modes + 1] = mode end
                table.sort(modes)
                for _, mode in ipairs(modes) do
                    local tool = w.Tool[mode]
                    for _, target in ipairs({ 'prop', 'world' }) do
                        for _, fn in ipairs({ 'LeftClick', 'RightClick', 'Reload' }) do
                            if tool[fn] then
                                local e = target == 'prop' and prop() or game.GetWorld()
                                local tr = { Hit = true, HitPos = Vector(0, 0, 100), HitNormal = Vector(0, 0, 1), Entity = e,
                                    PhysicsBone = 0, HitWorld = target == 'world', HitNonWorld = target ~= 'world', StartPos = p:GetShootPos(),
                                    Normal = Vector(0, 0, -1), Fraction = 0.5, MatType = 67, HitTexture = '', HitSky = false }
                                tool:ClearObjects()
                                local before = #ents.GetAll()
                                local ok, err = pcall(tool[fn], tool, tr)
                                local made = #ents.GetAll() - before
                                if made > 0 then out[#out + 1] = '  ' .. mode .. '.' .. fn .. ' (' .. target .. ') created ' .. made end
                                if not ok then out[#out + 1] = mode .. '.' .. fn .. ' (' .. target .. '): ' .. tostring(err) end
                                -- two-click tools: click a second time
                                if ok and tool:NumObjects() > 0 then
                                    local ok2, err2 = pcall(tool[fn], tool, tr)
                                    if not ok2 then out[#out + 1] = mode .. '.' .. fn .. ' #2 (' .. target .. '): ' .. tostring(err2) end
                                end
                            end
                        end
                    end
                end
                return #modes .. ' tools\n' .. table.concat(out, '\n')
            "#,
            )
            .eval()
            .unwrap_or_else(|e| format!("script error: {e}"));
        eprintln!("{out}");
        assert_eq!(out.lines().filter(|l| l.contains("): ")).count(), 0, "tool failures:
{out}");
        // the tools you place things with really create them
        let counts: Vec<i64> = g
            .lua
            .load(
                "local n = 0 for _ in pairs(list.Get('DynamiteModels')) do n = n + 1 end
                 return { #ents.FindByClass('gmod_dynamite'), #ents.FindByClass('gmod_cameraprop'), #ents.FindByClass('gmod_thruster'),
                          #ents.FindByClass('gmod_lamp'), #ents.FindByClass('gmod_light'), #ents.FindByClass('gmod_balloon') }",
            )
            .eval()
            .unwrap();
        assert!(counts.iter().all(|c| *c > 0), "dynamite, camera, thruster, lamp, light, balloon: {counts:?}");
        let ok: bool = g
            .lua
            .load(
                "local a, b = LocalToWorld(Vector(10, 0, 0), Angle(0, 90, 0), Vector(1, 2, 3), Angle(0, 90, 0))
                 local c, d = WorldToLocal(a, b, Vector(1, 2, 3), Angle(0, 90, 0))
                 return a:Distance(Vector(1, 12, 3)) < 0.01 and math.abs(math.NormalizeAngle(b.y - 180)) < 0.01
                    and c:Distance(Vector(10, 0, 0)) < 0.01 and math.abs(d.y - 90) < 0.01
                    and ProtectedCall(function() error('x') end) == false",
            )
            .eval()
            .unwrap();
        assert!(ok, "LocalToWorld / WorldToLocal / ProtectedCall");
        let s = g.state.borrow();
        for e in s.error_log.iter().take(40) {
            eprintln!("  ERR {e}");
        }
        drop(s);
        for (k, n) in g.missing_report(60) {
            eprintln!("  {n:6} {k}");
        }
    }

    /// `kill`, then a click after the respawn delay brings the player back
    /// (sandbox `PlayerDeathThink`; `SRCDS_RS_GMOD_DIR`).
    #[test]
    fn sandbox_respawn_after_kill() {
        let Ok(root) = std::env::var("SRCDS_RS_GMOD_DIR") else { return };
        let g = crate::GLua::new(std::path::Path::new(&root)).unwrap();
        g.boot("sandbox").unwrap();
        let spawn = vec![vec![("classname".to_string(), "worldspawn".to_string())], vec![("classname".to_string(), "info_player_start".to_string())]];
        g.start_game(8, vec![9], &spawn);
        let p = g.player_connect(0, 2, "tester", 76561197960265729, false, "127.0.0.1:27005");
        let mut t = 0.0;
        for _ in 0..5 {
            t += 0.015;
            g.think(t);
        }
        g.player_suicide(p);
        let alive = |g: &crate::GLua| crate::entity::with_player(&g.lua, p, |pd, _| pd.alive).unwrap();
        assert!(!alive(&g));
        // three seconds without input: still dead
        for _ in 0..200 {
            t += 0.015;
            g.think(t);
        }
        assert!(!alive(&g), "respawned without input");
        // click (IN_ATTACK pressed this tick)
        crate::entity::with_player(&g.lua, p, |pd, _| {
            pd.old_buttons = 0;
            pd.buttons = 1;
        })
        .unwrap();
        t += 0.015;
        g.think(t);
        t += 0.015;
        g.think(t);
        let s = g.state.borrow();
        assert!(alive(&g), "no respawn after click; errors: {:?}", &s.error_log[s.error_log.len().saturating_sub(3)..]);
    }
}