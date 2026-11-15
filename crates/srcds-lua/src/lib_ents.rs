//! `ents`, `player`, `Entity()`, `game.GetWorld` and the `util.Trace*` family.

use mlua::{Function, IntoLua, Lua, MultiValue, ObjectLike, Table, Value};

use crate::entity::{self, add, scale, store, sub, with, EntRef};
use crate::vecang::Vector;

/// Result of a world trace (host collision).
#[derive(Debug, Clone, Copy, Default)]
pub struct TraceHit {
    pub fraction: f32,
    pub end: [f32; 3],
    pub normal: [f32; 3],
    pub start_solid: bool,
    pub all_solid: bool,
    pub hit_sky: bool,
}

/// World collision provided by the host.
pub trait WorldTrace {
    fn trace(&self, start: [f32; 3], end: [f32; 3], mins: [f32; 3], maxs: [f32; 3]) -> TraceHit;
}

pub struct WorldTracer(pub Box<dyn WorldTrace>);

/// `ents.Create`: allocates an entity; scripted classes get their merged table.
pub fn create(lua: &Lua, class: &str) -> mlua::Result<EntRef> {
    let g = lua.globals();
    // scripted entity / weapon definition
    let mut def: Option<Table> = None;
    let mut is_swep = false;
    for (lib, swep) in [("scripted_ents", false), ("weapons", true)] {
        if let Ok(t) = g.get::<Table>(lib) {
            if let Ok(get) = t.get::<Function>("Get") {
                if let Ok(Value::Table(d)) = get.call::<Value>(class) {
                    def = Some(d);
                    is_swep = swep;
                    break;
                }
            }
        }
    }
    let r = {
        let mut s = store(lua)?;
        match s.create(class, None) {
            Some(r) => r,
            None => return Ok(EntRef::NULL),
        }
    };
    if let Some(d) = def {
        let ud = r.into_lua(lua)?;
        d.set("Entity", ud.clone())?;
        if is_swep {
            d.set("Weapon", ud.clone())?;
        }
        let key = lua.create_registry_value(&d)?;
        with(lua, r, |e| {
            e.table = Some(key);
            e.scripted = true;
        })?;
        // data tables are set up at creation, before Initialize
        if let Ok(Value::Function(setup)) = d.get::<Value>("SetupDataTables") {
            let install: Value = ud.clone();
            if let Value::UserData(u) = &install {
                if let Ok(Value::Function(f)) = u.get::<Value>("InstallDataTable") {
                    f.call::<()>(ud.clone())?;
                }
            }
            setup.call::<()>(ud.clone())?;
        }
    }
    if let Ok(hook) = g.get::<Table>("hook") {
        if let Ok(run) = hook.get::<Function>("Run") {
            if let Err(e) = run.call::<()>(("OnEntityCreated", r)) {
                tracing::warn!(error = %e, "OnEntityCreated");
            }
        }
    }
    Ok(r)
}

fn wildcard(pat: &str, s: &str) -> bool {
    match pat.split_once('*') {
        None => pat.eq_ignore_ascii_case(s),
        Some((a, b)) => {
            s.len() >= a.len() + b.len()
                && s[..a.len()].eq_ignore_ascii_case(a)
                && s[s.len() - b.len()..].eq_ignore_ascii_case(b)
        }
    }
}

fn list(lua: &Lua, f: impl Fn(&entity::EntData) -> bool) -> mlua::Result<Table> {
    let v: Vec<EntRef> = store(lua)?
        .ents
        .values()
        .filter(|e| !e.removing && e.r.id != 0 && f(e))
        .map(|e| e.r)
        .collect();
    lua.create_sequence_from(v)
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

pub fn install(lua: &Lua) -> mlua::Result<()> {
    install_effect_data(lua)?;
    let g = lua.globals();
    let ents = lib(lua, "ents")?;
    ents.raw_set("Create", lua.create_function(|lua, class: String| create(lua, &class))?)?;
    ents.raw_set("CreateClientProp", lua.create_function(|_, _: MultiValue| Ok(EntRef::NULL))?)?;
    ents.raw_set("GetAll", lua.create_function(|lua, ()| list(lua, |_| true))?)?;
    ents.raw_set("Iterator", lua.create_function(|lua, ()| {
        let t = list(lua, |_| true)?;
        let ipairs: Function = lua.globals().get("ipairs")?;
        ipairs.call::<MultiValue>(t)
    })?)?;
    ents.raw_set("GetCount", lua.create_function(|lua, _: Option<bool>| Ok(store(lua)?.ents.len()))?)?;
    ents.raw_set("GetEdictCount", lua.create_function(|lua, ()| Ok(store(lua)?.ents.len()))?)?;
    ents.raw_set(
        "FindByClass",
        lua.create_function(|lua, c: String| {
            let players = c.eq_ignore_ascii_case("player");
            list(lua, |e| if players { e.player.is_some() } else { wildcard(&c, &e.class) })
        })?,
    )?;
    ents.raw_set(
        "FindByClassAndParent",
        lua.create_function(|lua, (c, p): (String, EntRef)| list(lua, |e| wildcard(&c, &e.class) && e.parent == p))?,
    )?;
    ents.raw_set("FindByModel", lua.create_function(|lua, m: String| list(lua, |e| wildcard(&m, &e.model)))?)?;
    ents.raw_set("FindByName", lua.create_function(|lua, n: String| list(lua, |e| wildcard(&n, &e.name)))?)?;
    ents.raw_set(
        "FindInSphere",
        lua.create_function(|lua, (c, r): (Vector, f32)| {
            list(lua, |e| {
                let d = sub(e.world_center(), c.0);
                d[0] * d[0] + d[1] * d[1] + d[2] * d[2] <= r * r
            })
        })?,
    )?;
    ents.raw_set(
        "FindInBox",
        lua.create_function(|lua, (a, b): (Vector, Vector)| {
            let lo = [a.0[0].min(b.0[0]), a.0[1].min(b.0[1]), a.0[2].min(b.0[2])];
            let hi = [a.0[0].max(b.0[0]), a.0[1].max(b.0[1]), a.0[2].max(b.0[2])];
            list(lua, |e| (0..3).all(|k| e.pos[k] >= lo[k] && e.pos[k] <= hi[k]))
        })?,
    )?;
    ents.raw_set(
        "FindInCone",
        lua.create_function(|lua, (o, dir, dist, cos): (Vector, Vector, f32, f32)| {
            list(lua, |e| {
                let d = sub(e.world_center(), o.0);
                let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
                l <= dist && (l == 0.0 || (d[0] * dir.0[0] + d[1] * dir.0[1] + d[2] * dir.0[2]) / l >= cos)
            })
        })?,
    )?;
    ents.raw_set("FindInPVS", lua.create_function(|lua, _: Value| list(lua, |_| true))?)?;
    ents.raw_set("FindAlongRay", lua.create_function(|lua, _: MultiValue| list(lua, |_| false))?)?;
    ents.raw_set(
        "GetByIndex",
        lua.create_function(|lua, i: i64| Ok(by_index(lua, i)))?,
    )?;
    ents.raw_set(
        "GetMapCreatedEntity",
        lua.create_function(|lua, id: i64| {
            let s = store(lua)?;
            Ok(s.ents.values().find(|e| e.map_entity && e.r.id as i64 + 1234 == id).map_or(EntRef::NULL, |e| e.r))
        })?,
    )?;
    g.raw_set("Entity", lua.create_function(|lua, i: i64| Ok(by_index(lua, i)))?)?;
    lib(lua, "game")?.raw_set("GetWorld", lua.create_function(|lua, ()| Ok(by_index(lua, 0)))?)?;

    let player = lib(lua, "player")?;
    let players = |lua: &Lua, f: &dyn Fn(&entity::PlayerData) -> bool| -> mlua::Result<Table> {
        let v: Vec<EntRef> = store(lua)?
            .players()
            .filter(|e| !e.removing && f(e.player.as_ref().unwrap()))
            .map(|e| e.r)
            .collect();
        lua.create_sequence_from(v)
    };
    player.raw_set("GetAll", lua.create_function(move |lua, ()| players(lua, &|_| true))?)?;
    player.raw_set("Iterator", lua.create_function(move |lua, ()| {
        let t = players(lua, &|_| true)?;
        let ipairs: Function = lua.globals().get("ipairs")?;
        ipairs.call::<MultiValue>(t)
    })?)?;
    player.raw_set("GetHumans", lua.create_function(move |lua, ()| players(lua, &|p| !p.bot))?)?;
    player.raw_set("GetBots", lua.create_function(move |lua, ()| players(lua, &|p| p.bot))?)?;
    player.raw_set("GetCount", lua.create_function(|lua, ()| Ok(store(lua)?.players().count()))?)?;
    player.raw_set(
        "GetByID",
        lua.create_function(|lua, i: i64| {
            let s = store(lua)?;
            Ok(s.ents.get(&(i as u32)).filter(|e| e.player.is_some()).map_or(EntRef::NULL, |e| e.r))
        })?,
    )?;
    let find_player = |lua: &Lua, f: &dyn Fn(&entity::PlayerData) -> bool| -> mlua::Result<Value> {
        let r = store(lua)?.players().find(|e| f(e.player.as_ref().unwrap())).map(|e| e.r);
        match r {
            Some(r) => r.into_lua(lua),
            None => Ok(Value::Boolean(false)),
        }
    };
    player.raw_set(
        "GetBySteamID64",
        lua.create_function(move |lua, id: Value| {
            let id = match id {
                Value::String(s) => s.to_string_lossy().parse::<u64>().unwrap_or(0),
                Value::Integer(i) => i as u64,
                Value::Number(n) => n as u64,
                _ => 0,
            };
            find_player(lua, &|p| p.steam_id64 == id && id != 0)
        })?,
    )?;
    player.raw_set(
        "GetBySteamID",
        lua.create_function(move |lua, id: String| {
            find_player(lua, &|p| {
                let account = (p.steam_id64 & 0xFFFF_FFFF) as u32;
                format!("STEAM_0:{}:{}", account & 1, account >> 1).eq_ignore_ascii_case(&id)
            })
        })?,
    )?;
    player.raw_set(
        "GetByUniqueID",
        lua.create_function(move |lua, id: Value| {
            let want = match id {
                Value::String(s) => s.to_string_lossy().parse::<i64>().unwrap_or(-1),
                Value::Integer(i) => i,
                Value::Number(n) => n as i64,
                _ => -1,
            };
            find_player(lua, &|p| entity::crc32(format!("gm_{}_gm", p.steam_id64).as_bytes()) as i64 == want)
        })?,
    )?;
    player.raw_set(
        "GetByAccountID",
        lua.create_function(move |lua, id: i64| find_player(lua, &|p| (p.steam_id64 & 0xFFFF_FFFF) as i64 == id))?,
    )?;
    player.raw_set("CreateNextBot", lua.create_function(|_, _n: String| Ok(EntRef::NULL))?)?;

    install_traces(lua)?;
    Ok(())
}

pub fn by_index(lua: &Lua, i: i64) -> EntRef {
    let Ok(s) = store(lua) else { return EntRef::NULL };
    if i < 0 {
        return EntRef::NULL;
    }
    s.ents.get(&(i as u32)).map_or(EntRef::NULL, |e| e.r)
}

// ---------------------------------------------------------------------------
// traces

struct TraceArgs {
    start: [f32; 3],
    end: [f32; 3],
    mins: [f32; 3],
    maxs: [f32; 3],
    filter: Value,
    ignore_world: bool,
    hit_clientonly: bool,
}

fn vec_field(t: &Table, k: &str) -> mlua::Result<Option<[f32; 3]>> {
    Ok(match t.get::<Value>(k)? {
        Value::UserData(ud) => ud.borrow::<Vector>().ok().map(|v| v.0),
        _ => None,
    })
}

/// Whether `filter` excludes entity `r`.
fn filtered(lua: &Lua, filter: &Value, r: EntRef) -> mlua::Result<bool> {
    Ok(match filter {
        Value::UserData(ud) => ud.borrow::<entity::EntUd>().map(|f| f.0 == r).unwrap_or(false),
        Value::Table(t) => {
            // list of entities, or (GMod) a list of class names
            let class = store(lua)?.get(r).map(|e| e.class.clone()).unwrap_or_default();
            let mut hit = false;
            for v in t.clone().sequence_values::<Value>().flatten() {
                match v {
                    Value::UserData(ud) => {
                        if ud.borrow::<entity::EntUd>().map(|f| f.0 == r).unwrap_or(false) {
                            hit = true;
                        }
                    }
                    Value::String(s) => {
                        if s.to_string_lossy() == class {
                            hit = true;
                        }
                    }
                    _ => {}
                }
            }
            hit
        }
        Value::Function(f) => {
            // returns true to hit
            !f.call::<bool>(r).unwrap_or(true)
        }
        _ => false,
    })
}

/// Swept box against an axis-aligned box; returns (fraction, normal).
fn sweep_aabb(start: [f32; 3], end: [f32; 3], lo: [f32; 3], hi: [f32; 3]) -> Option<(f32, [f32; 3])> {
    let d = sub(end, start);
    let (mut t0, mut t1) = (0.0f32, 1.0f32);
    let mut normal = [0.0; 3];
    for k in 0..3 {
        if d[k].abs() < 1e-9 {
            if start[k] < lo[k] || start[k] > hi[k] {
                return None;
            }
            continue;
        }
        let (mut a, mut b) = ((lo[k] - start[k]) / d[k], (hi[k] - start[k]) / d[k]);
        let mut n = [0.0; 3];
        n[k] = -d[k].signum();
        if a > b {
            std::mem::swap(&mut a, &mut b);
        }
        if a > t0 {
            t0 = a;
            normal = n;
        }
        t1 = t1.min(b);
        if t0 > t1 {
            return None;
        }
    }
    let inside = (0..3).all(|k| start[k] >= lo[k] && start[k] <= hi[k]);
    if inside {
        return Some((0.0, [0.0, 0.0, 1.0]));
    }
    Some((t0, normal))
}

fn run_trace(lua: &Lua, a: &TraceArgs) -> mlua::Result<Table> {
    let mut best = TraceHit {
        fraction: 1.0,
        end: a.end,
        ..Default::default()
    };
    let mut hit_ent: Option<EntRef> = None;
    let mut hit_world = false;
    if !a.ignore_world {
        if let Some(w) = lua.app_data_ref::<WorldTracer>() {
            let h = w.0.trace(a.start, a.end, a.mins, a.maxs);
            if h.fraction < 1.0 || h.start_solid {
                best = h;
                hit_world = true;
            }
        }
    }
    // entities: world-space AABB of each solid entity, grown by the trace box
    let candidates: Vec<(EntRef, [f32; 3], [f32; 3])> = {
        let s = store(lua)?;
        s.ents
            .values()
            .filter(|e| {
                e.r.id != 0
                    && !e.removing
                    && e.spawned
                    && e.weapon_owner.is_null()
                    && (e.solid != 0 || e.player.is_some())
                    && e.solid_flags & 4 == 0
                    && e.player.as_ref().is_none_or(|p| p.alive)
            })
            .map(|e| {
                let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
                for i in 0..8 {
                    let c = [
                        if i & 1 == 0 { e.mins[0] } else { e.maxs[0] },
                        if i & 2 == 0 { e.mins[1] } else { e.maxs[1] },
                        if i & 4 == 0 { e.mins[2] } else { e.maxs[2] },
                    ];
                    let w = add(e.pos, crate::vecang::rotate(c, e.ang));
                    for k in 0..3 {
                        lo[k] = lo[k].min(w[k]);
                        hi[k] = hi[k].max(w[k]);
                    }
                }
                (e.r, sub(lo, a.maxs), sub(hi, a.mins))
            })
            .collect()
    };
    let _ = a.hit_clientonly;
    for (r, lo, hi) in candidates {
        if let Some((f, n)) = sweep_aabb(a.start, a.end, lo, hi) {
            if f < best.fraction || (f == 0.0 && best.fraction > 0.0) {
                if filtered(lua, &a.filter, r)? {
                    continue;
                }
                best.fraction = f;
                best.normal = n;
                best.start_solid = f == 0.0;
                best.end = add(a.start, scale(sub(a.end, a.start), f));
                hit_ent = Some(r);
                hit_world = false;
            }
        }
    }
    let t = lua.create_table()?;
    let hit = best.fraction < 1.0 || best.start_solid;
    t.set("Hit", hit)?;
    t.set("HitPos", Vector(best.end))?;
    t.set("HitNormal", Vector(best.normal))?;
    t.set("Fraction", best.fraction)?;
    t.set("FractionLeftSolid", 0.0)?;
    t.set("StartPos", Vector(a.start))?;
    let d = sub(a.end, a.start);
    let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(1e-6);
    t.set("Normal", Vector(scale(d, 1.0 / l)))?;
    t.set("StartSolid", best.start_solid)?;
    t.set("AllSolid", best.all_solid)?;
    t.set("HitSky", best.hit_sky)?;
    t.set("HitNoDraw", false)?;
    t.set("HitWorld", hit && hit_world)?;
    t.set("HitNonWorld", hit_ent.is_some())?;
    t.set("HitTexture", if hit_world { "**studio**" } else { "" })?;
    t.set("HitGroup", 0)?;
    t.set("HitBox", 0)?;
    t.set("PhysicsBone", 0)?;
    t.set("SurfaceProps", 0)?;
    t.set("MatType", 67)?; // MAT_CONCRETE
    t.set("DispFlags", 0)?;
    t.set("Contents", if hit { 1 } else { 0 })?;
    let ent = match hit_ent {
        Some(r) => r,
        None if hit && hit_world => by_index(lua, 0),
        None => EntRef::NULL,
    };
    t.set("Entity", ent)?;
    Ok(t)
}

fn trace_args(t: &Table, mins: Option<[f32; 3]>, maxs: Option<[f32; 3]>) -> mlua::Result<TraceArgs> {
    let start = vec_field(t, "start")?.unwrap_or([0.0; 3]);
    Ok(TraceArgs {
        start,
        end: vec_field(t, "endpos")?.unwrap_or(start),
        mins: mins.or(vec_field(t, "mins")?).unwrap_or([0.0; 3]),
        maxs: maxs.or(vec_field(t, "maxs")?).unwrap_or([0.0; 3]),
        filter: t.get("filter")?,
        ignore_world: t.get::<Option<bool>>("ignoreworld")?.unwrap_or(false),
        hit_clientonly: t.get::<Option<bool>>("hitclientonly")?.unwrap_or(false),
    })
}

fn finish(t: &Table, res: Table) -> mlua::Result<Table> {
    // `output` table is filled in place and also returned
    if let Value::Table(out) = t.get::<Value>("output")? {
        for pair in res.pairs::<Value, Value>() {
            let (k, v) = pair?;
            out.set(k, v)?;
        }
        return Ok(out);
    }
    Ok(res)
}

/// `EffectData()`: a plain value object (`CEffectData`) with Set*/Get* pairs.
/// `util.Effect` itself sends nothing yet (temp entities are not implemented).
fn install_effect_data(lua: &Lua) -> mlua::Result<()> {
    let meta = crate::meta::get(lua, "CEffectData")?;
    lua.load(
        r#"
        local meta = ...
        meta.__index = meta
        meta.MetaName = "CEffectData"
        local defaults = {
            Origin = function() return Vector(0, 0, 0) end,
            Start = function() return Vector(0, 0, 0) end,
            Normal = function() return Vector(0, 0, 0) end,
            Angles = function() return Angle(0, 0, 0) end,
            Entity = function() return NULL end,
        }
        for _, k in ipairs({ "Origin", "Start", "Normal", "Angles", "Entity", "Magnitude", "Scale",
                "Radius", "Flags", "Color", "Attachment", "DamageType", "HitBox", "MaterialIndex",
                "SurfaceProp", "EntIndex" }) do
            meta["Set" .. k] = function(self, v) self.fields[k] = v end
            meta["Get" .. k] = function(self)
                local v = self.fields[k]
                if v ~= nil then return v end
                local d = defaults[k]
                return d and d() or 0
            end
        end
        function meta:SetEntity(e)
            self.fields.Entity = e
            self.fields.EntIndex = IsValid(e) and e:EntIndex() or 0
        end
        function meta:IsValid() return true end
        meta.__tostring = function() return "CEffectData" end
        function EffectData() return setmetatable({ fields = {} }, meta) end
        "#,
    )
    .set_name("=effectdata")
    .call::<()>(meta)
}

fn install_traces(lua: &Lua) -> mlua::Result<()> {
    let util = lib(lua, "util")?;
    util.raw_set(
        "TraceLine",
        lua.create_function(|lua, t: Table| {
            let a = trace_args(&t, Some([0.0; 3]), Some([0.0; 3]))?;
            finish(&t, run_trace(lua, &a)?)
        })?,
    )?;
    util.raw_set(
        "TraceHull",
        lua.create_function(|lua, t: Table| {
            let a = trace_args(&t, None, None)?;
            finish(&t, run_trace(lua, &a)?)
        })?,
    )?;
    util.raw_set(
        "TraceEntity",
        lua.create_function(|lua, (t, e): (Table, EntRef)| {
            let (mn, mx) = with(lua, e, |d| (d.mins, d.maxs))?;
            let mut a = trace_args(&t, Some(mn), Some(mx))?;
            if a.filter.is_nil() {
                a.filter = e.into_lua(lua)?;
            }
            finish(&t, run_trace(lua, &a)?)
        })?,
    )?;
    util.raw_set(
        "TraceEntityHull",
        lua.create_function(|lua, (t, e): (Table, EntRef)| {
            let (mn, mx) = with(lua, e, |d| (d.mins, d.maxs))?;
            let a = trace_args(&t, Some(mn), Some(mx))?;
            finish(&t, run_trace(lua, &a)?)
        })?,
    )?;
    util.raw_set(
        "PointContents",
        lua.create_function(|lua, p: Vector| {
            let solid = lua
                .app_data_ref::<WorldTracer>()
                .is_some_and(|w| w.0.trace(p.0, p.0, [0.0; 3], [0.0; 3]).start_solid);
            Ok(if solid { 1 } else { 0 })
        })?,
    )?;
    util.raw_set(
        "IsInWorld",
        lua.create_function(|lua, p: Vector| {
            Ok(!lua
                .app_data_ref::<WorldTracer>()
                .is_some_and(|w| w.0.trace(p.0, p.0, [0.0; 3], [0.0; 3]).start_solid))
        })?,
    )?;
    util.raw_set("IsValidModel", lua.create_function(|lua, m: String| Ok(entity::model_exists(lua, &m)))?)?;
    util.raw_set("IsValidProp", lua.create_function(|lua, m: String| Ok(entity::model_exists(lua, &m)))?)?;
    util.raw_set("IsValidRagdoll", lua.create_function(|_, _m: String| Ok(false))?)?;
    util.raw_set("IsModelLoaded", lua.create_function(|_, _m: String| Ok(true))?)?;
    util.raw_set("CRC", lua.create_function(|_, s: mlua::String| Ok(entity::crc32(&s.as_bytes()).to_string()))?)?;
    util.raw_set("GetModelInfo", lua.create_function(|_, _m: String| Ok(()))?)?;
    util.raw_set("GetModelMeshes", lua.create_function(|_, _m: MultiValue| Ok(()))?)?;
    util.raw_set("Effect", lua.create_function(|_, _a: MultiValue| Ok(()))?)?;
    util.raw_set("Decal", lua.create_function(|_, _a: MultiValue| Ok(()))?)?;
    util.raw_set("ScreenShake", lua.create_function(|_, _a: MultiValue| Ok(()))?)?;
    util.raw_set("BlastDamage", lua.create_function(|_, _a: MultiValue| Ok(()))?)?;
    util.raw_set("BlastDamageInfo", lua.create_function(|_, _a: MultiValue| Ok(()))?)?;
    util.raw_set("SpriteTrail", lua.create_function(|_, _a: MultiValue| Ok(EntRef::NULL))?)?;
    util.raw_set("GetSurfaceIndex", lua.create_function(|_, _a: String| Ok(0))?)?;
    util.raw_set("GetSurfacePropName", lua.create_function(|_, _a: i32| Ok("default"))?)?;
    util.raw_set("GetSurfaceData", lua.create_function(|_, _a: i32| Ok(()))?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::GLua;

    struct Floor;
    impl super::WorldTrace for Floor {
        fn trace(&self, s: [f32; 3], e: [f32; 3], mins: [f32; 3], _maxs: [f32; 3]) -> super::TraceHit {
            // plane z = 0
            let (zs, ze) = (s[2] + mins[2], e[2] + mins[2]);
            if zs >= 0.0 && ze < 0.0 {
                let f = zs / (zs - ze);
                super::TraceHit {
                    fraction: f,
                    end: [s[0] + (e[0] - s[0]) * f, s[1] + (e[1] - s[1]) * f, s[2] + (e[2] - s[2]) * f],
                    normal: [0.0, 0.0, 1.0],
                    ..Default::default()
                }
            } else {
                super::TraceHit { fraction: 1.0, end: e, ..Default::default() }
            }
        }
    }

    #[test]
    fn entities_and_traces() {
        let g = GLua::new(std::path::Path::new(".")).unwrap();
        g.lua.set_app_data(super::WorldTracer(Box::new(Floor)));
        crate::entity::store(&g.lua).unwrap().create("worldspawn", Some(0));
        let ok: bool = g
            .lua
            .load(
                "local e = ents.Create('prop_physics')
                 e:SetPos(Vector(0, 0, 50)) e:SetCollisionBounds(Vector(-10,-10,-10), Vector(10,10,10))
                 e:SetSolid(6) e:Spawn()
                 e.Custom = 5
                 local keyed = { [e] = true }
                 assert(keyed[ents.GetByIndex(e:EntIndex())], 'cached userdata')
                 assert(e.Custom == 5 and e:GetTable().Custom == 5)
                 assert(#ents.FindByClass('prop_*') == 1)
                 local tr = util.TraceLine({ start = Vector(0,0,100), endpos = Vector(0,0,-100) })
                 assert(tr.Entity == e, 'hit prop ' .. tostring(tr.Entity))
                 assert(math.abs(tr.HitPos.z - 60) < 0.01, tostring(tr.HitPos))
                 local tr2 = util.TraceLine({ start = Vector(0,0,100), endpos = Vector(0,0,-100), filter = e })
                 assert(tr2.HitWorld and tr2.Entity == game.GetWorld() and math.abs(tr2.HitPos.z) < 0.01)
                 e:SetNWInt('n', 3) assert(e:GetNWInt('n') == 3 and e:GetNWInt('m', 7) == 7)
                 assert(tostring(e):find('prop_physics'))
                 e:Remove()
                 assert(not e:IsValid() and e == e)
                 return true",
            )
            .eval()
            .unwrap();
        assert!(ok);
    }
}
