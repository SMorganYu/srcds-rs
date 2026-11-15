//! Engine globals and small libraries: time, convars, timers, network strings,
//! console command registration, precache lists and game/engine info.

use std::collections::HashMap;

use mlua::{Function, Lua, MultiValue, RegistryKey, Table, UserData, UserDataMethods, Value};
use crate::runtime::{report, Shared};

/// A console variable.
#[derive(Debug, Clone)]
pub struct ConVar {
    pub name: String,
    pub value: String,
    pub default: String,
    pub help: String,
    pub flags: i64,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

/// Lua handle of a convar (by name).
#[derive(Debug, Clone)]
pub struct ConVarRef(pub String);

pub struct Timer {
    pub name: Option<String>,
    pub delay: f64,
    pub reps: i64,
    pub next: f64,
    pub func: RegistryKey,
    pub paused_left: Option<f64>,
}

/// Engine-side state the Lua libraries read and write.
#[derive(Default)]
pub struct Engine {
    pub cur_time: f64,
    pub real_start: Option<std::time::Instant>,
    pub frame_time: f64,
    pub tick_interval: f64,
    pub tick_count: i64,
    pub map: String,
    pub max_players: i64,
    pub gamemode: String,
    pub convars: HashMap<String, ConVar>,
    /// `util.AddNetworkString` names in id order (id = position + 1; 0 is unused).
    pub network_strings: Vec<String>,
    /// Console commands registered by Lua (`AddConsoleCommand`).
    pub console_commands: Vec<String>,
    /// `util.PrecacheModel` / `util.PrecacheSound` requests.
    pub precache_models: Vec<String>,
    pub precache_sounds: Vec<String>,
    pub timers: Vec<Timer>,
    /// Commands queued by `RunConsoleCommand` / `game.ConsoleCommand`.
    pub server_commands: Vec<String>,
    /// `PrintMessage(type, text)` for every player.
    pub broadcast_prints: Vec<(u8, String)>,
}

impl ConVarRef {
    fn with<R>(&self, lua: &Lua, f: impl FnOnce(&ConVar) -> R) -> Option<R> {
        let e = lua.app_data_ref::<Engine>()?;
        e.convars.get(&self.0.to_ascii_lowercase()).map(f)
    }
}

impl UserData for ConVarRef {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("GetString", |lua, this, ()| Ok(this.with(lua, |c| c.value.clone()).unwrap_or_default()));
        m.add_method("GetFloat", |lua, this, ()| {
            Ok(this.with(lua, |c| c.value.trim().parse::<f64>().unwrap_or(0.0)).unwrap_or(0.0))
        });
        m.add_method("GetInt", |lua, this, ()| {
            Ok(this
                .with(lua, |c| c.value.trim().parse::<f64>().unwrap_or(0.0) as i64)
                .unwrap_or(0))
        });
        m.add_method("GetBool", |lua, this, ()| {
            Ok(this
                .with(lua, |c| c.value.trim().parse::<f64>().unwrap_or(0.0) != 0.0)
                .unwrap_or(false))
        });
        m.add_method("GetDefault", |lua, this, ()| Ok(this.with(lua, |c| c.default.clone()).unwrap_or_default()));
        m.add_method("GetName", |_, this, ()| Ok(this.0.clone()));
        m.add_method("GetHelpText", |lua, this, ()| Ok(this.with(lua, |c| c.help.clone()).unwrap_or_default()));
        m.add_method("GetFlags", |lua, this, ()| Ok(this.with(lua, |c| c.flags).unwrap_or(0)));
        m.add_method("IsFlagSet", |lua, this, f: i64| Ok(this.with(lua, |c| c.flags & f != 0).unwrap_or(false)));
        m.add_method("GetMin", |lua, this, ()| Ok(this.with(lua, |c| c.min).flatten()));
        m.add_method("GetMax", |lua, this, ()| Ok(this.with(lua, |c| c.max).flatten()));
        m.add_method("SetString", |lua, this, v: String| {
            set_convar(lua, &this.0, v);
            Ok(())
        });
        m.add_method("SetFloat", |lua, this, v: f64| {
            set_convar(lua, &this.0, fmt_num(v));
            Ok(())
        });
        m.add_method("SetInt", |lua, this, v: i64| {
            set_convar(lua, &this.0, v.to_string());
            Ok(())
        });
        m.add_method("SetBool", |lua, this, v: bool| {
            set_convar(lua, &this.0, if v { "1" } else { "0" }.to_string());
            Ok(())
        });
        m.add_method("Revert", |lua, this, ()| {
            let d = this.with(lua, |c| c.default.clone()).unwrap_or_default();
            set_convar(lua, &this.0, d);
            Ok(())
        });
    }
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Sets a convar (clamped to its bounds) and fires the Lua change callbacks.
pub fn set_convar(lua: &Lua, name: &str, value: String) {
    let key = name.to_ascii_lowercase();
    let old = {
        let Some(mut e) = lua.app_data_mut::<Engine>() else { return };
        let Some(c) = e.convars.get_mut(&key) else { return };
        let mut v = value;
        if let Ok(n) = v.trim().parse::<f64>() {
            let mut n2 = n;
            if let Some(lo) = c.min {
                n2 = n2.max(lo);
            }
            if let Some(hi) = c.max {
                n2 = n2.min(hi);
            }
            if n2 != n {
                v = fmt_num(n2);
            }
        }
        if c.value == v {
            return;
        }
        std::mem::replace(&mut c.value, v.clone())
    };
    // cvars.OnConVarChanged( name, old, new ) is how GMod dispatches callbacks
    if let Ok(cvars) = lua.globals().get::<Table>("cvars") {
        if let Ok(f) = cvars.get::<Function>("OnConVarChanged") {
            let new = lua
                .app_data_ref::<Engine>()
                .and_then(|e| e.convars.get(&key).map(|c| c.value.clone()))
                .unwrap_or_default();
            let _ = f.call::<()>((name, old, new));
        }
    }
}

pub fn create_convar(lua: &Lua, name: &str, default: &str, flags: i64, help: &str, min: Option<f64>, max: Option<f64>) {
    let Some(mut e) = lua.app_data_mut::<Engine>() else { return };
    e.convars.entry(name.to_ascii_lowercase()).or_insert_with(|| ConVar {
        name: name.to_string(),
        value: default.to_string(),
        default: default.to_string(),
        help: help.to_string(),
        flags,
        min,
        max,
    });
}

fn lib(lua: &Lua, name: &str) -> mlua::Result<Table> {
    match lua.globals().raw_get::<Value>(name)? {
        Value::Table(t) => Ok(t),
        _ => {
            let t = lua.create_table()?;
            lua.globals().raw_set(name, &t)?;
            Ok(t)
        }
    }
}

fn to_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.to_string_lossy().to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => fmt_num(*n),
        Value::Boolean(b) => (if *b { "1" } else { "0" }).to_string(),
        _ => String::new(),
    }
}

pub fn install(lua: &Lua, st: &Shared) -> mlua::Result<()> {
    lua.set_app_data(Engine {
        real_start: Some(std::time::Instant::now()),
        tick_interval: 0.015,
        frame_time: 0.015,
        max_players: 32,
        map: "gm_construct".into(),
        gamemode: "sandbox".into(),
        ..Default::default()
    });
    let g = lua.globals();

    // time
    g.raw_set("CurTime", lua.create_function(|lua, ()| Ok(lua.app_data_ref::<Engine>().map_or(0.0, |e| e.cur_time)))?)?;
    g.raw_set("UnPredictedCurTime", g.raw_get::<Function>("CurTime")?)?;
    let real = |lua: &Lua| -> f64 {
        lua.app_data_ref::<Engine>()
            .and_then(|e| e.real_start.map(|s| s.elapsed().as_secs_f64()))
            .unwrap_or(0.0)
    };
    g.raw_set("RealTime", lua.create_function(move |lua, ()| Ok(real(lua)))?)?;
    g.raw_set("SysTime", lua.create_function(move |lua, ()| Ok(real(lua)))?)?;
    g.raw_set("FrameTime", lua.create_function(|lua, ()| Ok(lua.app_data_ref::<Engine>().map_or(0.015, |e| e.frame_time)))?)?;
    g.raw_set("FrameNumber", lua.create_function(|lua, ()| Ok(lua.app_data_ref::<Engine>().map_or(0, |e| e.tick_count)))?)?;
    let engine = lib(lua, "engine")?;
    engine.raw_set("TickInterval", lua.create_function(|lua, ()| Ok(lua.app_data_ref::<Engine>().map_or(0.015, |e| e.tick_interval)))?)?;
    engine.raw_set("TickCount", lua.create_function(|lua, ()| Ok(lua.app_data_ref::<Engine>().map_or(0, |e| e.tick_count)))?)?;
    engine.raw_set("ActiveGamemode", lua.create_function(|lua, ()| Ok(lua.app_data_ref::<Engine>().map(|e| e.gamemode.clone()).unwrap_or_default()))?)?;
    engine.raw_set(
        "GetGames",
        lua.create_function(|lua, ()| {
            // Half-Life 2 content is part of the base install
            let t = lua.create_table()?;
            let hl2 = lua.create_table()?;
            hl2.set("depot", 220)?;
            hl2.set("title", "Half-Life 2")?;
            hl2.set("folder", "hl2")?;
            hl2.set("mounted", true)?;
            hl2.set("owned", true)?;
            hl2.set("installed", true)?;
            t.raw_set(1, hl2)?;
            Ok(t)
        })?,
    )?;
    engine.raw_set("GetAddons", lua.create_function(|lua, ()| lua.create_table())?)?;
    engine.raw_set("GetGamemodes", lua.create_function(|lua, ()| lua.create_table())?)?;
    engine.raw_set("IsDedicatedServer", lua.create_function(|_, ()| Ok(true))?)?;

    let game = lib(lua, "game")?;
    game.raw_set("GetMap", lua.create_function(|lua, ()| Ok(lua.app_data_ref::<Engine>().map(|e| e.map.clone()).unwrap_or_default()))?)?;
    game.raw_set("MaxPlayers", lua.create_function(|lua, ()| Ok(lua.app_data_ref::<Engine>().map_or(32, |e| e.max_players)))?)?;
    game.raw_set("SinglePlayer", lua.create_function(|_, ()| Ok(false))?)?;
    game.raw_set("IsDedicated", lua.create_function(|_, ()| Ok(true))?)?;
    game.raw_set("GetSkillLevel", lua.create_function(|_, ()| Ok(1))?)?;
    game.raw_set("GetIPAddress", lua.create_function(|_, ()| Ok("0.0.0.0:27015"))?)?;
    game.raw_set(
        "ConsoleCommand",
        lua.create_function(|lua, cmd: String| {
            if let Some(mut e) = lua.app_data_mut::<Engine>() {
                e.server_commands.push(cmd);
            }
            Ok(())
        })?,
    )?;
    g.raw_set(
        "RunConsoleCommand",
        lua.create_function(|lua, args: MultiValue| {
            let parts: Vec<String> = args.iter().map(to_str).collect();
            if let Some(mut e) = lua.app_data_mut::<Engine>() {
                e.server_commands.push(parts.join(" "));
            }
            Ok(())
        })?,
    )?;

    let system = lib(lua, "system")?;
    system.raw_set("IsWindows", lua.create_function(|_, ()| Ok(cfg!(windows)))?)?;
    system.raw_set("IsLinux", lua.create_function(|_, ()| Ok(cfg!(target_os = "linux")))?)?;
    system.raw_set("IsOSX", lua.create_function(|_, ()| Ok(cfg!(target_os = "macos")))?)?;
    system.raw_set("AppTime", lua.create_function(move |lua, ()| Ok(real(lua)))?)?;

    g.raw_set(
        "GetConVar_Internal",
        lua.create_function(|lua, name: String| {
            let exists = lua
                .app_data_ref::<Engine>()
                .is_some_and(|e| e.convars.contains_key(&name.to_ascii_lowercase()));
            Ok(exists.then_some(ConVarRef(name)))
        })?,
    )?;
    g.raw_set("GetConVar", g.raw_get::<Function>("GetConVar_Internal")?)?;
    g.raw_set(
        "ConVarExists",
        lua.create_function(|lua, name: String| {
            Ok(lua
                .app_data_ref::<Engine>()
                .is_some_and(|e| e.convars.contains_key(&name.to_ascii_lowercase())))
        })?,
    )?;
    g.raw_set(
        "CreateConVar",
        lua.create_function(|lua, (name, default, flags, help, min, max): (String, Value, Value, Option<String>, Option<f64>, Option<f64>)| {
            // flags may be a number or a table of numbers
            let flags = match flags {
                Value::Integer(i) => i,
                Value::Number(n) => n as i64,
                Value::Table(t) => t.sequence_values::<i64>().filter_map(|v| v.ok()).fold(0, |a, b| a | b),
                _ => 0,
            };
            create_convar(lua, &name, &to_str(&default), flags, help.as_deref().unwrap_or(""), min, max);
            Ok(ConVarRef(name))
        })?,
    )?;
    g.raw_set(
        "GetConVarString",
        lua.create_function(|lua, name: String| {
            Ok(lua
                .app_data_ref::<Engine>()
                .and_then(|e| e.convars.get(&name.to_ascii_lowercase()).map(|c| c.value.clone()))
                .unwrap_or_default())
        })?,
    )?;
    g.raw_set(
        "GetConVarNumber",
        lua.create_function(|lua, name: String| {
            Ok(lua
                .app_data_ref::<Engine>()
                .and_then(|e| e.convars.get(&name.to_ascii_lowercase()).map(|c| c.value.trim().parse::<f64>().unwrap_or(0.0)))
                .unwrap_or(0.0))
        })?,
    )?;

    // network strings
    let util = lib(lua, "util")?;
    util.raw_set(
        "AddNetworkString",
        lua.create_function(|lua, name: String| {
            let mut e = lua.app_data_mut::<Engine>().unwrap();
            if let Some(i) = e.network_strings.iter().position(|n| n.eq_ignore_ascii_case(&name)) {
                return Ok(i as i64 + 1);
            }
            e.network_strings.push(name);
            Ok(e.network_strings.len() as i64)
        })?,
    )?;
    util.raw_set(
        "NetworkStringToID",
        lua.create_function(|lua, name: String| {
            let e = lua.app_data_ref::<Engine>().unwrap();
            Ok(e.network_strings
                .iter()
                .position(|n| n.eq_ignore_ascii_case(&name))
                .map_or(0, |i| i as i64 + 1))
        })?,
    )?;
    util.raw_set(
        "NetworkIDToString",
        lua.create_function(|lua, id: i64| {
            let e = lua.app_data_ref::<Engine>().unwrap();
            Ok((id >= 1).then(|| e.network_strings.get(id as usize - 1).cloned()).flatten())
        })?,
    )?;
    util.raw_set(
        "PrecacheModel",
        lua.create_function(|lua, m: String| {
            let mut e = lua.app_data_mut::<Engine>().unwrap();
            let m = m.replace('\\', "/").to_ascii_lowercase();
            if !e.precache_models.contains(&m) {
                e.precache_models.push(m);
            }
            Ok(())
        })?,
    )?;
    util.raw_set(
        "PrecacheSound",
        lua.create_function(|lua, s: String| {
            let mut e = lua.app_data_mut::<Engine>().unwrap();
            if !e.precache_sounds.contains(&s) {
                e.precache_sounds.push(s);
            }
            Ok(())
        })?,
    )?;
    g.raw_set(
        "AddConsoleCommand",
        lua.create_function(|lua, (name, _help, _flags): (String, Value, Value)| {
            let mut e = lua.app_data_mut::<Engine>().unwrap();
            let n = name.to_ascii_lowercase();
            if !e.console_commands.contains(&n) {
                e.console_commands.push(n);
            }
            Ok(())
        })?,
    )?;

    // materials are a client concept; the server only needs a handle
    g.raw_set(
        "Material",
        lua.create_function(|lua, (name, _flags): (String, Option<String>)| {
            let t = lua.create_table()?;
            t.raw_set("__srcds_material", name.as_str())?;
            // the .vmt: shader name, then { "key" "value" }
            let path = format!("materials/{}.vmt", name.trim_end_matches(".vmt").replace('\\', "/").to_ascii_lowercase());
            let text = lua
                .app_data_ref::<crate::entity::EntityStore>()
                .and_then(|s| s.models.as_ref().and_then(|m| m.read(&path)))
                .map(|b| String::from_utf8_lossy(&b).to_string());
            t.raw_set("__srcds_error", text.is_none())?;
            let params = lua.create_table()?;
            if let Some(text) = text {
                let toks = crate::loader::kv_tokens(&text);
                t.raw_set("__srcds_shader", toks.first().cloned().unwrap_or_default())?;
                let mut i = 1;
                let mut depth = 0;
                while i < toks.len() {
                    match toks[i].as_str() {
                        "{" => depth += 1,
                        "}" => depth -= 1,
                        k if depth == 1 && i + 1 < toks.len() && toks[i + 1] != "{" => {
                            params.raw_set(k.to_ascii_lowercase(), toks[i + 1].as_str())?;
                            i += 1;
                        }
                        _ => {}
                    }
                    i += 1;
                }
            } else {
                t.raw_set("__srcds_shader", "")?;
            }
            t.raw_set("__srcds_params", params)?;
            if let Ok(mt) = crate::meta::get(lua, "IMaterial") {
                t.set_metatable(Some(mt));
            }
            Ok((t, 0.0))
        })?,
    )?;
    let mat = crate::meta::get(lua, "IMaterial")?;
    let field = |t: &Table, k: &str| -> mlua::Result<Value> { t.raw_get(k) };
    mat.raw_set("GetName", lua.create_function(move |_, t: Table| field(&t, "__srcds_material"))?)?;
    mat.raw_set("GetShader", lua.create_function(move |_, t: Table| field(&t, "__srcds_shader"))?)?;
    mat.raw_set("IsError", lua.create_function(move |_, t: Table| field(&t, "__srcds_error"))?)?;
    mat.raw_set("GetKeyValues", lua.create_function(move |_, t: Table| field(&t, "__srcds_params"))?)?;
    let param = |t: &Table, k: &str| -> mlua::Result<Option<String>> {
        let p: Table = t.raw_get("__srcds_params")?;
        p.raw_get::<Option<String>>(k.to_ascii_lowercase())
    };
    mat.raw_set("GetString", lua.create_function(move |_, (t, k): (Table, String)| param(&t, &k))?)?;
    mat.raw_set(
        "GetFloat",
        lua.create_function(move |_, (t, k): (Table, String)| Ok(param(&t, &k)?.and_then(|v| v.trim().parse::<f64>().ok())))?,
    )?;
    mat.raw_set(
        "GetInt",
        lua.create_function(move |_, (t, k): (Table, String)| Ok(param(&t, &k)?.and_then(|v| v.trim().parse::<f64>().ok()).map(|v| v as i64)))?,
    )?;
    mat.raw_set("Width", lua.create_function(|_, _t: Table| Ok(0))?)?;
    mat.raw_set("Height", lua.create_function(|_, _t: Table| Ok(0))?)?;
    g.raw_set("gmod", lib(lua, "gmod")?)?;
    lib(lua, "gmod")?.raw_set("GetGamemode", lua.create_function(|lua, ()| lua.globals().get::<Value>("GAMEMODE"))?)?;

    g.raw_set(
        "PrintMessage",
        lua.create_function(|lua, (kind, text): (u8, String)| {
            if let Some(mut e) = lua.app_data_mut::<Engine>() {
                if e.broadcast_prints.len() < 256 {
                    e.broadcast_prints.push((kind, text));
                }
            }
            Ok(())
        })?,
    )?;
    // NPC AI queries (no NPC simulation yet: empty squads, no nodes)
    let ai = lib(lua, "ai")?;
    ai.raw_set("GetSquadMemberCount", lua.create_function(|_, _s: Value| Ok(0))?)?;
    ai.raw_set("GetSquadMembers", lua.create_function(|lua, _s: Value| lua.create_table())?)?;
    ai.raw_set("GetSquadLeader", lua.create_function(|_, _s: Value| Ok(Value::Nil))?)?;
    ai.raw_set("GetNodeCount", lua.create_function(|_, ()| Ok(0))?)?;
    ai.raw_set("GetScheduleID", lua.create_function(|_, _s: Value| Ok(-1))?)?;
    ai.raw_set("GetTaskID", lua.create_function(|_, _s: Value| Ok(-1))?)?;

    install_timers(lua, st)?;
    Ok(())
}

fn install_timers(lua: &Lua, st: &Shared) -> mlua::Result<()> {
    let timer = lib(lua, "timer")?;
    let create = |lua: &Lua, name: Option<String>, delay: f64, reps: i64, func: Function| -> mlua::Result<()> {
        let key = lua.create_registry_value(func)?;
        let mut e = lua.app_data_mut::<Engine>().unwrap();
        let now = e.cur_time;
        if let Some(n) = &name {
            e.timers.retain(|t| t.name.as_ref() != Some(n));
        }
        e.timers.push(Timer {
            name,
            delay: delay.max(0.0),
            reps,
            next: now + delay.max(0.0),
            func: key,
            paused_left: None,
        });
        Ok(())
    };
    timer.raw_set(
        "Simple",
        lua.create_function(move |lua, (delay, func): (f64, Function)| create(lua, None, delay, 1, func))?,
    )?;
    timer.raw_set(
        "Create",
        lua.create_function(move |lua, (name, delay, reps, func): (Value, f64, i64, Function)| {
            create(lua, Some(to_str(&name)), delay, reps, func)
        })?,
    )?;
    let by_name = |lua: &Lua, name: &str, f: &mut dyn FnMut(&mut Timer, f64)| -> bool {
        let mut e = lua.app_data_mut::<Engine>().unwrap();
        let now = e.cur_time;
        let mut found = false;
        for t in e.timers.iter_mut().filter(|t| t.name.as_deref() == Some(name)) {
            f(t, now);
            found = true;
        }
        found
    };
    timer.raw_set(
        "Remove",
        lua.create_function(|lua, name: Value| {
            let n = to_str(&name);
            lua.app_data_mut::<Engine>().unwrap().timers.retain(|t| t.name.as_deref() != Some(n.as_str()));
            Ok(())
        })?,
    )?;
    timer.raw_set("Destroy", timer.raw_get::<Function>("Remove")?)?;
    timer.raw_set(
        "Exists",
        lua.create_function(|lua, name: Value| {
            let n = to_str(&name);
            Ok(lua.app_data_ref::<Engine>().unwrap().timers.iter().any(|t| t.name.as_deref() == Some(n.as_str())))
        })?,
    )?;
    timer.raw_set(
        "Adjust",
        lua.create_function(move |lua, (name, delay, reps, func): (Value, f64, Option<i64>, Option<Function>)| {
            let key = func.map(|f| lua.create_registry_value(f)).transpose()?;
            let mut key = key;
            Ok(by_name(lua, &to_str(&name), &mut |t, now| {
                t.delay = delay;
                t.next = now + delay;
                if let Some(r) = reps {
                    t.reps = r;
                }
                if let Some(k) = key.take() {
                    t.func = k;
                }
            }))
        })?,
    )?;
    timer.raw_set(
        "Pause",
        lua.create_function(move |lua, name: Value| {
            Ok(by_name(lua, &to_str(&name), &mut |t, now| {
                if t.paused_left.is_none() {
                    t.paused_left = Some((t.next - now).max(0.0));
                }
            }))
        })?,
    )?;
    timer.raw_set(
        "UnPause",
        lua.create_function(move |lua, name: Value| {
            Ok(by_name(lua, &to_str(&name), &mut |t, now| {
                if let Some(l) = t.paused_left.take() {
                    t.next = now + l;
                }
            }))
        })?,
    )?;
    timer.raw_set(
        "Start",
        lua.create_function(move |lua, name: Value| {
            Ok(by_name(lua, &to_str(&name), &mut |t, now| {
                t.next = now + t.delay;
                t.paused_left = None;
            }))
        })?,
    )?;
    timer.raw_set(
        "Stop",
        lua.create_function(move |lua, name: Value| {
            Ok(by_name(lua, &to_str(&name), &mut |t, _| t.paused_left = Some(t.delay)))
        })?,
    )?;
    timer.raw_set(
        "TimeLeft",
        lua.create_function(|lua, name: Value| {
            let n = to_str(&name);
            let e = lua.app_data_ref::<Engine>().unwrap();
            Ok(e.timers
                .iter()
                .find(|t| t.name.as_deref() == Some(n.as_str()))
                .map(|t| t.paused_left.unwrap_or(t.next - e.cur_time)))
        })?,
    )?;
    timer.raw_set(
        "RepsLeft",
        lua.create_function(|lua, name: Value| {
            let n = to_str(&name);
            let e = lua.app_data_ref::<Engine>().unwrap();
            Ok(e.timers.iter().find(|t| t.name.as_deref() == Some(n.as_str())).map(|t| t.reps))
        })?,
    )?;
    let _ = st;
    Ok(())
}

/// Advances the clock and runs due timers.
pub fn run_timers(lua: &Lua, st: &Shared, now: f64) {
    if let Some(mut e) = lua.app_data_mut::<Engine>() {
        e.cur_time = now;
    }
    loop {
        // take one due timer at a time so callbacks may create/remove timers
        let due = {
            let mut e = lua.app_data_mut::<Engine>().unwrap();
            let i = e
                .timers
                .iter()
                .position(|t| t.paused_left.is_none() && t.next <= now);
            match i {
                None => None,
                Some(i) => {
                    let t = &mut e.timers[i];
                    let f: Option<Function> = lua.registry_value(&t.func).ok();
                    let done = t.reps == 1;
                    if t.reps > 1 {
                        t.reps -= 1;
                    }
                    t.next = now + t.delay.max(0.001);
                    if done {
                        e.timers.remove(i);
                    }
                    f
                }
            }
        };
        let Some(f) = due else { break };
        if let Err(err) = f.call::<()>(()) {
            report(st, &err);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::GLua;

    #[test]
    fn convars_and_timers() {
        let g = GLua::new(std::path::Path::new(".")).unwrap();
        g.lua
            .load(
                "local cv = CreateConVar('sbox_maxprops', '200', 0, 'help', 0, 1000)
                 N = cv:GetInt() + GetConVarNumber('sbox_maxprops')
                 cv:SetInt(5000)
                 CLAMPED = GetConVar('sbox_maxprops'):GetInt()
                 COUNT = 0
                 timer.Create('t', 1, 3, function() COUNT = COUNT + 1 end)
                 timer.Simple(0.5, function() SIMPLE = true end)",
            )
            .exec()
            .unwrap();
        let gl = g.lua.globals();
        assert_eq!(gl.get::<i64>("N").unwrap(), 400);
        assert_eq!(gl.get::<i64>("CLAMPED").unwrap(), 1000);
        for t in 1..=10 {
            super::run_timers(&g.lua, &g.state, t as f64 * 0.5);
        }
        assert_eq!(gl.get::<i64>("COUNT").unwrap(), 3);
        assert!(gl.get::<bool>("SIMPLE").unwrap());
    }

    #[test]
    fn network_strings() {
        let g = GLua::new(std::path::Path::new(".")).unwrap();
        let id: i64 = g
            .lua
            .load("util.AddNetworkString('a') util.AddNetworkString('b') return util.NetworkStringToID('B')")
            .eval()
            .unwrap();
        assert_eq!(id, 2);
    }
}
