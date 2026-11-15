//! Engine entities as seen from Lua: the entity store, the `Entity` userdata
//! (one cached userdata per entity, so entities work as table keys), its `__index`
//! chain (typed metatable → base metatables → the entity's own Lua table) and the
//! engine methods of `Entity`, `Player`, `Weapon` and `PhysObj`.
//!
//! The store is plain data owned by the Lua state; the server reads it each tick to
//! build snapshots and writes player state (positions, input) into it.

use std::collections::{BTreeMap, HashMap};

use mlua::{AnyUserData, FromLua, FromLuaMulti, IntoLua, IntoLuaMulti, Lua, MetaMethod, MultiValue, RegistryKey, Table, UserData, UserDataMethods, Value};

use crate::meta;
use crate::vecang::{angle_vectors, rotate, Angle, Vector};

pub const MAX_EDICTS: u32 = 2048;

/// Reference to an entity: store id (= edict index) and serial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntRef {
    pub id: u32,
    pub serial: u32,
}

impl EntRef {
    pub const NULL: EntRef = EntRef { id: u32::MAX, serial: 0 };
    pub fn is_null(self) -> bool {
        self.id == u32::MAX
    }
}

/// A networked variable value (NW/NW2/DT vars).
#[derive(Debug, Clone, PartialEq)]
pub enum NetValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Vec([f32; 3]),
    Ang([f32; 3]),
    Ent(EntRef),
}

impl NetValue {
    pub fn from_lua_value(v: &Value) -> Option<NetValue> {
        Some(match v {
            Value::Boolean(b) => NetValue::Bool(*b),
            Value::Integer(i) => NetValue::Int(*i),
            Value::Number(n) => NetValue::Float(*n),
            Value::String(s) => NetValue::Str(s.to_string_lossy().to_string()),
            Value::UserData(ud) => {
                if let Ok(v) = ud.borrow::<Vector>() {
                    NetValue::Vec(v.0)
                } else if let Ok(a) = ud.borrow::<Angle>() {
                    NetValue::Ang(a.0)
                } else if let Ok(e) = ud.borrow::<EntUd>() {
                    NetValue::Ent(e.0)
                } else {
                    return None;
                }
            }
            _ => return None,
        })
    }

    pub fn to_lua(&self, lua: &Lua) -> mlua::Result<Value> {
        Ok(match self {
            NetValue::Bool(b) => Value::Boolean(*b),
            NetValue::Int(i) => Value::Integer(*i),
            NetValue::Float(f) => Value::Number(*f),
            NetValue::Str(s) => Value::String(lua.create_string(s)?),
            NetValue::Vec(v) => Vector(*v).into_lua(lua)?,
            NetValue::Ang(a) => Angle(*a).into_lua(lua)?,
            NetValue::Ent(e) => e.into_lua(lua)?,
        })
    }
}

/// Per-player engine state.
#[derive(Debug, Clone)]
pub struct PlayerData {
    pub slot: u32,
    pub user_id: i32,
    pub name: String,
    pub steam_id64: u64,
    pub bot: bool,
    pub alive: bool,
    pub walk_speed: f32,
    pub run_speed: f32,
    pub slow_walk_speed: f32,
    pub crouched_walk_speed: f32,
    pub duck_speed: f32,
    pub unduck_speed: f32,
    pub jump_power: f32,
    pub max_speed: f32,
    pub armor: i32,
    pub max_armor: i32,
    pub frags: i32,
    pub deaths: i32,
    pub team: i32,
    pub weapons: Vec<EntRef>,
    pub active_weapon: EntRef,
    pub eye_angles: [f32; 3],
    pub view_offset: [f32; 3],
    pub view_offset_ducked: [f32; 3],
    pub buttons: i32,
    pub old_buttons: i32,
    /// Cursor direction while the client world clicks (context menu held).
    pub world_click: Option<[f32; 3]>,
    pub crouching: bool,
    pub on_ground: bool,
    pub noclip: bool,
    pub fov: f32,
    /// Client convars (`GetInfo`), from the userinfo the client sends.
    pub info: HashMap<String, String>,
    /// Text for the client console / chat / centre (`kind`, text): 1 notify,
    /// 2 console, 3 chat, 4 centre.
    pub outbox: Vec<(u8, String)>,
    /// Lua sent to the client (`SendLua`).
    pub send_lua: Vec<String>,
    /// Commands to run on the client (`ConCommand`).
    pub client_commands: Vec<String>,
    /// Set by Lua `SetPos`/`SetEyeAngles`; the server applies and clears it.
    pub teleport: bool,
    pub eye_angles_set: bool,
    pub ammo: HashMap<String, i32>,
    pub hands: EntRef,
    pub view_model: EntRef,
    pub view_entity: EntRef,
    /// View entity the client was last told about (`svc_SetView`).
    pub view_entity_sent: EntRef,
    pub user_group: String,
    pub last_spawn_time: f64,
}

impl PlayerData {
    pub fn new(slot: u32, user_id: i32, name: &str, steam_id64: u64, bot: bool) -> Self {
        Self {
            slot,
            user_id,
            name: name.to_string(),
            steam_id64,
            bot,
            alive: false,
            walk_speed: 200.0,
            run_speed: 400.0,
            slow_walk_speed: 100.0,
            crouched_walk_speed: 0.3,
            duck_speed: 0.1,
            unduck_speed: 0.1,
            jump_power: 200.0,
            max_speed: 400.0,
            armor: 0,
            max_armor: 100,
            frags: 0,
            deaths: 0,
            team: 1001,
            weapons: Vec::new(),
            active_weapon: EntRef::NULL,
            eye_angles: [0.0; 3],
            view_offset: [0.0, 0.0, 64.0],
            view_offset_ducked: [0.0, 0.0, 28.0],
            buttons: 0,
            old_buttons: 0,
            world_click: None,
            crouching: false,
            on_ground: true,
            noclip: false,
            fov: 0.0,
            info: HashMap::new(),
            outbox: Vec::new(),
            send_lua: Vec::new(),
            client_commands: Vec::new(),
            teleport: false,
            eye_angles_set: false,
            ammo: HashMap::new(),
            hands: EntRef::NULL,
            view_model: EntRef::NULL,
            view_entity: EntRef::NULL,
            view_entity_sent: EntRef::NULL,
            user_group: "user".into(),
            last_spawn_time: 0.0,
        }
    }
}

/// Physics object state of an entity (`GetPhysicsObject`).
#[derive(Debug, Clone)]
pub struct PhysData {
    pub motion: bool,
    pub gravity: bool,
    pub collisions: bool,
    pub asleep: bool,
    pub mass: f32,
    pub material: String,
    pub ang_vel: [f32; 3],
    pub drag: bool,
    /// Held by a physics gun: moved kinematically, not simulated.
    pub held: bool,
}

impl Default for PhysData {
    fn default() -> Self {
        Self {
            motion: true,
            gravity: true,
            collisions: true,
            asleep: false,
            mass: 50.0,
            material: "default".into(),
            ang_vel: [0.0; 3],
            drag: true,
            held: false,
        }
    }
}

#[derive(Debug)]
pub struct EntData {
    pub r: EntRef,
    pub class: String,
    pub model: String,
    pub pos: [f32; 3],
    pub ang: [f32; 3],
    pub vel: [f32; 3],
    pub mins: [f32; 3],
    pub maxs: [f32; 3],
    pub color: [u8; 4],
    pub render_mode: i32,
    pub render_fx: i32,
    pub skin: i32,
    /// Animation sequence and playback rate (networked; the client animates).
    pub sequence: i32,
    pub playback_rate: f32,
    pub material: String,
    pub sub_materials: BTreeMap<i32, String>,
    pub body_groups: BTreeMap<i32, i32>,
    pub model_scale: f32,
    pub health: i32,
    pub max_health: i32,
    pub move_type: i32,
    pub solid: i32,
    pub solid_flags: i32,
    pub collision_group: i32,
    pub flags: i64,
    pub effects: i32,
    pub spawn_flags: i64,
    pub no_draw: bool,
    pub owner: EntRef,
    pub parent: EntRef,
    pub creator: EntRef,
    pub name: String,
    pub keyvalues: Vec<(String, String)>,
    pub spawned: bool,
    pub removing: bool,
    pub nw: HashMap<String, NetValue>,
    pub dt: HashMap<(u8, i32), NetValue>,
    pub phys: Option<PhysData>,
    pub created_at: f64,
    pub next_think: Option<f64>,
    /// Map entity (from the BSP entity lump).
    pub map_entity: bool,
    pub scripted: bool,
    pub player: Option<PlayerData>,
    pub weapon_owner: EntRef,
    pub clip1: i32,
    pub clip2: i32,
    /// Weapon ammo types (names) and clip sizes (-1 = no clip).
    pub ammo1: String,
    pub ammo2: String,
    pub max_clip1: i32,
    pub max_clip2: i32,
    pub table: Option<RegistryKey>,
    pub userdata: Option<RegistryKey>,
    /// Model changed since the server last looked (precache needed).
    pub model_dirty: bool,
    /// Physics constraint carried by this (constraint) entity.
    pub constraint: Option<Constraint>,
}

/// Kinds of physics constraint srcds-rs simulates.
#[derive(Debug, Clone, PartialEq)]
pub enum ConstraintKind {
    /// Rigid: `b` keeps its pose relative to `a`.
    Weld { rel_pos: [f32; 3], rel_fwd: [f32; 3], rel_up: [f32; 3] },
    /// Anchor points at most `length` apart (`rigid`: exactly).
    Length { length: f32, rigid: bool },
    /// Anchor points coincide (ball socket, hinge, ragdoll constraint).
    Point,
    /// Spring between the anchors.
    Spring { rest: f32, constant: f32, damping: f32 },
    /// Recorded but not simulated (collision pairs, sliders, keep-upright…).
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Constraint {
    /// Constrained entities (`NULL` = the world).
    pub a: EntRef,
    pub b: EntRef,
    /// Anchor points in each body's local space.
    pub local_a: [f32; 3],
    pub local_b: [f32; 3],
    pub kind: ConstraintKind,
}

impl EntData {
    fn new(r: EntRef, class: &str, now: f64) -> Self {
        Self {
            r,
            class: class.to_string(),
            model: String::new(),
            pos: [0.0; 3],
            ang: [0.0; 3],
            vel: [0.0; 3],
            mins: [0.0; 3],
            maxs: [0.0; 3],
            color: [255; 4],
            render_mode: 0,
            render_fx: 0,
            skin: 0,
            sequence: 0,
            playback_rate: 1.0,
            material: String::new(),
            sub_materials: BTreeMap::new(),
            body_groups: BTreeMap::new(),
            model_scale: 1.0,
            health: 0,
            max_health: 0,
            move_type: 0,
            solid: 0,
            solid_flags: 0,
            collision_group: 0,
            flags: 0,
            effects: 0,
            spawn_flags: 0,
            no_draw: false,
            owner: EntRef::NULL,
            parent: EntRef::NULL,
            creator: EntRef::NULL,
            name: String::new(),
            keyvalues: Vec::new(),
            spawned: false,
            removing: false,
            nw: HashMap::new(),
            dt: HashMap::new(),
            phys: None,
            created_at: now,
            next_think: None,
            map_entity: false,
            scripted: false,
            player: None,
            weapon_owner: EntRef::NULL,
            clip1: -1,
            clip2: -1,
            ammo1: String::new(),
            ammo2: String::new(),
            max_clip1: -1,
            max_clip2: -1,
            table: None,
            userdata: None,
            model_dirty: false,
            constraint: None,
        }
    }

    pub fn is_weapon(&self) -> bool {
        self.class.starts_with("weapon_") || self.class == "gmod_tool" || self.class == "gmod_camera"
    }

    pub fn world_center(&self) -> [f32; 3] {
        let c = [
            (self.mins[0] + self.maxs[0]) * 0.5,
            (self.mins[1] + self.maxs[1]) * 0.5,
            (self.mins[2] + self.maxs[2]) * 0.5,
        ];
        add(self.pos, rotate(c, self.ang))
    }
}

/// Model information the host provides (bounds from the model file).
pub trait ModelInfo {
    /// Hull (mins, maxs) of a model, if it can be read.
    fn bounds(&self, model: &str) -> Option<([f32; 3], [f32; 3])>;
    /// Whether the model file exists.
    fn exists(&self, _model: &str) -> bool {
        true
    }
    /// Reads a game file (materials, scripts) through the game's search path.
    fn read(&self, _path: &str) -> Option<Vec<u8>> {
        None
    }
    /// The model's animation sequences as (name, activity name), in the client's order.
    fn sequences(&self, _model: &str) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// Sequences of `model` (cached per model).
pub fn model_sequences(lua: &Lua, model: &str) -> std::rc::Rc<Vec<(String, String)>> {
    let key = model.replace('\\', "/").to_ascii_lowercase();
    if let Some(c) = lua.app_data_ref::<SequenceCache>() {
        if let Some(v) = c.0.get(&key) {
            return v.clone();
        }
    }
    let v = std::rc::Rc::new(
        lua.app_data_ref::<EntityStore>()
            .and_then(|s| s.models.as_ref().map(|m| m.sequences(&key)))
            .unwrap_or_default(),
    );
    if lua.app_data_ref::<SequenceCache>().is_none() {
        lua.set_app_data(SequenceCache::default());
    }
    if let Some(mut c) = lua.app_data_mut::<SequenceCache>() {
        c.0.insert(key, v.clone());
    }
    v
}

#[derive(Default)]
struct SequenceCache(HashMap<String, std::rc::Rc<Vec<(String, String)>>>);

/// Requests `activity` (an `ACT_*` name) on a view model entity; the server's weapon
/// logic plays it (sequence, parity, idle time) at the command's time.
pub fn set_view_model_activity(lua: &Lua, vm: EntRef, activity: &str) {
    let _ = with(lua, vm, |e| {
        e.nw.insert("__srcds_vm_pending".into(), NetValue::Str(activity.to_string()));
    });
}

/// Name of the `ACT_*` global with value `act`.
fn activity_name(lua: &Lua, act: i64) -> Option<String> {
    lua.globals().pairs::<Value, Value>().flatten().find_map(|(k, v)| match (k, v) {
        (Value::String(k), Value::Integer(n)) if n == act => {
            let k = k.to_str().ok()?.to_string();
            k.starts_with("ACT_").then_some(k)
        }
        _ => None,
    })
}

/// Sequence index of an activity name (`ACT_IDLE`), first match.
pub fn sequence_for_activity(lua: &Lua, model: &str, activity: &str) -> Option<i32> {
    model_sequences(lua, model).iter().position(|(_, a)| a.eq_ignore_ascii_case(activity)).map(|i| i as i32)
}

/// `util.IsValidModel`: a `.mdl` path the host can find.
pub fn model_exists(lua: &Lua, model: &str) -> bool {
    let m = model.replace('\\', "/").to_ascii_lowercase();
    if !m.ends_with(".mdl") {
        return false;
    }
    lua.app_data_ref::<EntityStore>()
        .map_or(true, |s| s.models.as_ref().map_or(true, |mi| mi.exists(&m)))
}

/// All entities of the Lua state.
#[derive(Default)]
pub struct EntityStore {
    pub ents: BTreeMap<u32, EntData>,
    pub max_clients: u32,
    /// Edict indices reserved by the host (e.g. the player resource).
    pub reserved: Vec<u32>,
    serial: u32,
    pub cur_time: f64,
    /// Entities removed since the host last drained the list.
    pub removed: Vec<EntRef>,
    pub models: Option<Box<dyn ModelInfo>>,
}

impl EntityStore {
    pub fn get(&self, r: EntRef) -> Option<&EntData> {
        self.ents.get(&r.id).filter(|e| e.r.serial == r.serial)
    }
    pub fn get_mut(&mut self, r: EntRef) -> Option<&mut EntData> {
        self.ents.get_mut(&r.id).filter(|e| e.r.serial == r.serial)
    }

    fn next_serial(&mut self) -> u32 {
        self.serial = (self.serial + 1) & 0x3FF;
        if self.serial == 0 {
            self.serial = 1;
        }
        self.serial
    }

    /// Allocates an entity at `id` (or the first free non-player edict).
    pub fn create(&mut self, class: &str, id: Option<u32>) -> Option<EntRef> {
        let id = match id {
            Some(i) => i,
            None => {
                let first = self.max_clients + 1;
                (first..MAX_EDICTS).find(|i| !self.ents.contains_key(i) && !self.reserved.contains(i))?
            }
        };
        let r = EntRef { id, serial: self.next_serial() };
        let now = self.cur_time;
        self.ents.insert(id, EntData::new(r, class, now));
        Some(r)
    }

    pub fn players(&self) -> impl Iterator<Item = &EntData> {
        self.ents.values().filter(|e| e.player.is_some())
    }

    pub fn player_by_slot(&self, slot: u32) -> Option<EntRef> {
        self.ents.get(&(slot + 1)).filter(|e| e.player.is_some()).map(|e| e.r)
    }
}

pub fn store(lua: &Lua) -> mlua::Result<mlua::AppDataRefMut<'_, EntityStore>> {
    lua.app_data_mut::<EntityStore>()
        .ok_or_else(|| mlua::Error::RuntimeError("entity store busy".into()))
}

fn null_err() -> mlua::Error {
    mlua::Error::RuntimeError("Tried to use a NULL entity!".into())
}

/// Runs `f` on the entity (error for NULL / removed entities).
pub fn with<R>(lua: &Lua, r: EntRef, f: impl FnOnce(&mut EntData) -> R) -> mlua::Result<R> {
    let mut s = store(lua)?;
    let e = s.get_mut(r).ok_or_else(null_err)?;
    Ok(f(e))
}

/// Runs `f` on the player data (error for non-players).
pub fn with_player<R>(lua: &Lua, r: EntRef, f: impl FnOnce(&mut PlayerData, &mut [f32; 3]) -> R) -> mlua::Result<R> {
    let mut s = store(lua)?;
    let e = s.get_mut(r).ok_or_else(null_err)?;
    let mut pos = e.pos;
    let p = e
        .player
        .as_mut()
        .ok_or_else(|| mlua::Error::RuntimeError("not a player".into()))?;
    let out = f(p, &mut pos);
    e.pos = pos;
    Ok(out)
}

pub fn valid(lua: &Lua, r: EntRef) -> bool {
    lua.app_data_ref::<EntityStore>()
        .is_some_and(|s| s.get(r).is_some_and(|e| !e.removing))
}

/// The metatable name for an entity.
fn kind(lua: &Lua, r: EntRef) -> &'static str {
    let Some(s) = lua.app_data_ref::<EntityStore>() else {
        return "Entity";
    };
    match s.get(r) {
        Some(e) if e.player.is_some() => "Player",
        Some(e) if e.is_weapon() => "Weapon",
        Some(e) if e.class.starts_with("npc_") => "NPC",
        Some(e) if e.class.starts_with("prop_vehicle") => "Vehicle",
        _ => "Entity",
    }
}

pub fn type_name(ud: &AnyUserData) -> Option<&'static str> {
    if ud.is::<PhysRef>() {
        return Some("PhysObj");
    }
    let r = ud.borrow::<EntUd>().ok()?.0;
    // the userdata does not carry the Lua state; resolve through the userdata's own
    // state is not available here, so read the kind tag cached at creation
    let _ = r;
    ud.user_value::<Option<String>>()
        .ok()
        .flatten()
        .map(|k| match k.as_str() {
            "Player" => "Player",
            "Weapon" => "Weapon",
            "NPC" => "NPC",
            "Vehicle" => "Vehicle",
            _ => "Entity",
        })
        .or(Some("Entity"))
}

/// The per-entity Lua table (created on demand).
pub fn ent_table(lua: &Lua, r: EntRef, create: bool) -> mlua::Result<Option<Table>> {
    {
        let s = store(lua)?;
        match s.get(r) {
            None => return Ok(None),
            Some(e) => {
                if let Some(k) = &e.table {
                    return Ok(Some(lua.registry_value(k)?));
                }
            }
        }
    }
    if !create {
        return Ok(None);
    }
    let t = lua.create_table()?;
    let key = lua.create_registry_value(&t)?;
    with(lua, r, |e| e.table = Some(key))?;
    Ok(Some(t))
}

impl FromLua for EntRef {
    fn from_lua(v: Value, _: &Lua) -> mlua::Result<Self> {
        match v {
            Value::UserData(ud) => Ok(ud.borrow::<EntUd>()?.0),
            Value::Nil => Ok(EntRef::NULL),
            other => Err(mlua::Error::FromLuaConversionError {
                from: other.type_name(),
                to: "Entity".into(),
                message: None,
            }),
        }
    }
}

impl IntoLua for EntRef {
    fn into_lua(self, lua: &Lua) -> mlua::Result<Value> {
        if self.is_null() {
            return lua.named_registry_value::<Value>("srcds_null");
        }
        {
            let s = store(lua)?;
            match s.get(self) {
                None => return lua.named_registry_value::<Value>("srcds_null"),
                Some(e) => {
                    if let Some(k) = &e.userdata {
                        return lua.registry_value(k);
                    }
                }
            }
        }
        let k = kind(lua, self);
        let ud = lua.create_userdata(EntUd(self))?;
        ud.set_user_value(k)?;
        let key = lua.create_registry_value(&ud)?;
        with(lua, self, |e| e.userdata = Some(key))?;
        Ok(Value::UserData(ud))
    }
}

/// Looks `key` up in metatable `name` and its `MetaBaseClass` chain.
fn meta_lookup(lua: &Lua, name: &str, key: &Value) -> mlua::Result<Value> {
    let mut t = meta::get(lua, name)?;
    for _ in 0..8 {
        let v: Value = t.raw_get(key.clone())?;
        if !v.is_nil() {
            return Ok(v);
        }
        match t.raw_get::<Value>("MetaBaseClass")? {
            Value::Table(b) => t = b,
            _ => break,
        }
    }
    Ok(Value::Nil)
}

/// The userdata wrapping an entity reference.
#[derive(Debug, Clone, Copy)]
pub struct EntUd(pub EntRef);

impl UserData for EntUd {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_function(MetaMethod::Index, |lua, (ud, key): (AnyUserData, Value)| {
            let r = ud.borrow::<EntUd>()?.0;
            let v = meta_lookup(lua, kind(lua, r), &key)?;
            if !v.is_nil() {
                return Ok(v);
            }
            match ent_table(lua, r, false)? {
                Some(t) => t.get::<Value>(key),
                None => Ok(Value::Nil),
            }
        });
        m.add_meta_function(MetaMethod::NewIndex, |lua, (ud, key, v): (AnyUserData, Value, Value)| {
            let r = ud.borrow::<EntUd>()?.0;
            match ent_table(lua, r, true)? {
                Some(t) => t.set(key, v),
                None => Err(null_err()),
            }
        });
        m.add_meta_function(MetaMethod::Eq, |_, (a, b): (Value, Value)| {
            let (Value::UserData(a), Value::UserData(b)) = (a, b) else {
                return Ok(false);
            };
            match (a.borrow::<EntUd>(), b.borrow::<EntUd>()) {
                (Ok(a), Ok(b)) => Ok(a.0 == b.0),
                _ => Ok(false),
            }
        });
        m.add_meta_function(MetaMethod::ToString, |lua, ud: AnyUserData| {
            let r = ud.borrow::<EntUd>()?.0;
            let s = store(lua)?;
            Ok(match s.get(r) {
                None => "[NULL Entity]".to_string(),
                Some(e) => match &e.player {
                    Some(p) => format!("Player [{}][{}]", r.id, p.name),
                    None if r.id == 0 => "Entity [0][worldspawn]".to_string(),
                    None => format!("{} [{}][{}]", if e.is_weapon() { "Weapon" } else { "Entity" }, r.id, e.class),
                },
            })
        });
    }
}

/// Physics object handle (`Entity:GetPhysicsObject()`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhysRef(pub EntRef);

impl UserData for PhysRef {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_function(MetaMethod::Index, |lua, (_ud, key): (AnyUserData, Value)| meta_lookup(lua, "PhysObj", &key));
        m.add_meta_function(MetaMethod::Eq, |_, (a, b): (AnyUserData, AnyUserData)| {
            Ok(match (a.borrow::<PhysRef>(), b.borrow::<PhysRef>()) {
                (Ok(a), Ok(b)) => *a == *b,
                _ => false,
            })
        });
        m.add_meta_function(MetaMethod::ToString, |_, ud: AnyUserData| {
            Ok(format!("PhysObject [{}]", ud.borrow::<PhysRef>()?.0.id))
        });
    }
}

// ---------------------------------------------------------------------------
// helpers

pub fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
pub fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
pub fn scale(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}
fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// World → local (inverse of `rotate`).
pub fn unrotate(v: [f32; 3], a: [f32; 3]) -> [f32; 3] {
    let (f, r, u) = angle_vectors(a);
    // rotate() maps local (x, y, z) to x*f - y*r + z*u
    [dot(v, f), -dot(v, r), dot(v, u)]
}

/// Global `LocalToWorld` / `WorldToLocal` (position + angle between coordinate
/// systems) and `ProtectedCall`.
fn install_coordinate_globals(lua: &Lua) -> mlua::Result<()> {
    let g = lua.globals();
    g.raw_set(
        "LocalToWorld",
        lua.create_function(|_, (lp, la, op, oa): (Vector, Angle, Vector, Angle)| {
            let (f, _, u) = angle_vectors(la.0);
            Ok((Vector(add(op.0, rotate(lp.0, oa.0))), Angle(angles_from_basis(rotate(f, oa.0), rotate(u, oa.0)))))
        })?,
    )?;
    g.raw_set(
        "WorldToLocal",
        lua.create_function(|_, (p, a, op, oa): (Vector, Angle, Vector, Angle)| {
            let (f, _, u) = angle_vectors(a.0);
            Ok((Vector(unrotate(sub(p.0, op.0), oa.0)), Angle(angles_from_basis(unrotate(f, oa.0), unrotate(u, oa.0)))))
        })?,
    )?;
    g.raw_set(
        "ProtectedCall",
        lua.create_function(|_, (f, args): (mlua::Function, MultiValue)| match f.call::<MultiValue>(args) {
            Ok(_) => Ok(true),
            Err(e) => {
                tracing::warn!(target: "lua", error = %e, "ProtectedCall");
                Ok(false)
            }
        })?,
    )?;
    Ok(())
}

/// Registers an engine method `name` on metatable `meta_name`; the first Lua argument
/// must be an entity.
pub fn method<A, R, F>(lua: &Lua, meta_name: &str, name: &str, f: F) -> mlua::Result<()>
where
    A: FromLuaMulti,
    R: IntoLuaMulti,
    F: Fn(&Lua, EntRef, A) -> mlua::Result<R> + 'static,
{
    let t = meta::get(lua, meta_name)?;
    let fname = format!("{meta_name}:{name}");
    t.raw_set(
        name,
        lua.create_function(move |lua, args: MultiValue| {
            let mut it = args.into_iter();
            let this = it.next().unwrap_or(Value::Nil);
            let r = match &this {
                Value::UserData(ud) => match ud.borrow::<EntUd>() {
                    Ok(r) => r.0,
                    Err(_) => return Err(mlua::Error::RuntimeError(format!("{fname}: bad self"))),
                },
                _ => return Err(mlua::Error::RuntimeError(format!("{fname}: called without an entity"))),
            };
            let rest: MultiValue = it.collect();
            let a = A::from_lua_multi(rest, lua)?;
            f(lua, r, a)
        })?,
    )
}

fn phys_method<A, R, F>(lua: &Lua, name: &str, f: F) -> mlua::Result<()>
where
    A: FromLuaMulti,
    R: IntoLuaMulti,
    F: Fn(&Lua, EntRef, A) -> mlua::Result<R> + 'static,
{
    let t = meta::get(lua, "PhysObj")?;
    t.raw_set(
        name,
        lua.create_function(move |lua, args: MultiValue| {
            let mut it = args.into_iter();
            let r = match it.next() {
                Some(Value::UserData(ud)) => ud.borrow::<PhysRef>()?.0,
                _ => return Err(mlua::Error::RuntimeError("PhysObj method without self".into())),
            };
            let a = A::from_lua_multi(it.collect(), lua)?;
            f(lua, r, a)
        })?,
    )
}

/// GMod `Color` table.
pub fn color_table(lua: &Lua, c: [u8; 4]) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("r", c[0])?;
    t.set("g", c[1])?;
    t.set("b", c[2])?;
    t.set("a", c[3])?;
    if let Ok(mt) = lua.named_registry_value::<Table>("srcds_meta_Color") {
        t.set_metatable(Some(mt));
    }
    Ok(t)
}

fn color_from(v: &Value) -> Option<[u8; 4]> {
    let Value::Table(t) = v else { return None };
    let c = |k: &str, d: u8| t.get::<Option<f64>>(k).ok().flatten().map_or(d, |x| x.clamp(0.0, 255.0) as u8);
    Some([c("r", 255), c("g", 255), c("b", 255), c("a", 255)])
}

/// Sets the model and its bounds.
pub fn set_model(lua: &Lua, r: EntRef, model: &str) -> mlua::Result<()> {
    let model = model.replace('\\', "/");
    let bounds = lua
        .app_data_ref::<EntityStore>()
        .and_then(|s| s.models.as_ref().and_then(|m| m.bounds(&model)));
    with(lua, r, |e| {
        if e.model != model {
            e.model_dirty = true;
        }
        e.model = model;
        if let Some((mn, mx)) = bounds {
            if e.player.is_none() {
                e.mins = mn;
                e.maxs = mx;
            }
        }
    })
}

// ---------------------------------------------------------------------------

pub fn install(lua: &Lua) -> mlua::Result<()> {
    install_coordinate_globals(lua)?;
    lua.set_app_data(EntityStore {
        max_clients: 32,
        ..Default::default()
    });
    let null = lua.create_userdata(EntUd(EntRef::NULL))?;
    null.set_user_value("Entity")?;
    lua.set_named_registry_value("srcds_null", &null)?;
    lua.globals().raw_set("NULL", null)?;

    install_entity(lua)?;
    install_player(lua)?;
    install_weapon(lua)?;
    install_phys(lua)?;
    Ok(())
}

fn install_entity(lua: &Lua) -> mlua::Result<()> {
    const E: &str = "Entity";
    method(lua, E, "IsValid", |lua, r, ()| Ok(valid(lua, r) && r.id != 0))?;
    method(lua, E, "EntIndex", |lua, r, ()| Ok(if valid(lua, r) { r.id as i64 } else { 0 }))?;
    method(lua, E, "GetClass", |lua, r, ()| with(lua, r, |e| if e.r.id == 0 { "worldspawn".to_string() } else { e.class.clone() }))?;
    method(lua, E, "IsWorld", |_, r, ()| Ok(r.id == 0))?;
    method(lua, E, "IsPlayer", |lua, r, ()| Ok(kind(lua, r) == "Player"))?;
    method(lua, E, "IsWeapon", |lua, r, ()| Ok(kind(lua, r) == "Weapon"))?;
    method(lua, E, "IsNPC", |lua, r, ()| Ok(kind(lua, r) == "NPC"))?;
    method(lua, E, "IsNextBot", |_, _, ()| Ok(false))?;
    method(lua, E, "IsVehicle", |lua, r, ()| Ok(kind(lua, r) == "Vehicle"))?;
    method(lua, E, "IsRagdoll", |lua, r, ()| with(lua, r, |e| e.class == "prop_ragdoll"))?;
    method(lua, E, "IsScripted", |lua, r, ()| with(lua, r, |e| e.scripted))?;
    method(lua, E, "IsInWorld", |_, _, ()| Ok(true))?;
    method(lua, E, "IsDormant", |_, _, ()| Ok(false))?;
    method(lua, E, "IsConstraint", |lua, r, ()| with(lua, r, |e| e.class.starts_with("phys_") && e.class != "phys_magnet"))?;
    method(lua, E, "IsSolid", |lua, r, ()| with(lua, r, |e| e.solid != 0 && e.solid_flags & 4 == 0))?;
    method(lua, E, "IsEffectActive", |lua, r, f: i32| with(lua, r, |e| e.effects & f != 0))?;
    method(lua, E, "AddEffects", |lua, r, f: i32| with(lua, r, |e| e.effects |= f))?;
    method(lua, E, "RemoveEffects", |lua, r, f: i32| with(lua, r, |e| e.effects &= !f))?;
    method(lua, E, "GetEffects", |lua, r, ()| with(lua, r, |e| e.effects))?;
    method(lua, E, "SetEffects", |lua, r, f: i32| with(lua, r, |e| e.effects = f))?;

    method(lua, E, "GetPos", |lua, r, ()| with(lua, r, |e| Vector(e.pos)))?;
    method(lua, E, "SetPos", |lua, r, v: Vector| {
        with(lua, r, |e| {
            e.pos = v.0;
            if let Some(p) = &mut e.player {
                p.teleport = true;
            }
        })
    })?;
    method(lua, E, "GetAngles", |lua, r, ()| with(lua, r, |e| Angle(e.ang)))?;
    method(lua, E, "SetAngles", |lua, r, a: Angle| with(lua, r, |e| e.ang = a.0))?;
    method(lua, E, "GetLocalPos", |lua, r, ()| with(lua, r, |e| Vector(e.pos)))?;
    method(lua, E, "SetLocalPos", |lua, r, v: Vector| with(lua, r, |e| e.pos = v.0))?;
    method(lua, E, "GetLocalAngles", |lua, r, ()| with(lua, r, |e| Angle(e.ang)))?;
    method(lua, E, "SetLocalAngles", |lua, r, a: Angle| with(lua, r, |e| e.ang = a.0))?;
    method(lua, E, "GetNetworkOrigin", |lua, r, ()| with(lua, r, |e| Vector(e.pos)))?;
    method(lua, E, "GetVelocity", |lua, r, ()| with(lua, r, |e| Vector(e.vel)))?;
    method(lua, E, "SetVelocity", |lua, r, v: Vector| {
        // players: adds to the velocity (like the engine's base velocity push)
        with(lua, r, |e| {
            if e.player.is_some() {
                e.vel = add(e.vel, v.0);
            } else {
                e.vel = v.0;
            }
        })
    })?;
    method(lua, E, "SetLocalVelocity", |lua, r, v: Vector| with(lua, r, |e| e.vel = v.0))?;
    method(lua, E, "GetAbsVelocity", |lua, r, ()| with(lua, r, |e| Vector(e.vel)))?;
    method(lua, E, "SetAbsVelocity", |lua, r, v: Vector| with(lua, r, |e| e.vel = v.0))?;
    method(lua, E, "GetForward", |lua, r, ()| with(lua, r, |e| Vector(angle_vectors(e.ang).0)))?;
    method(lua, E, "GetRight", |lua, r, ()| with(lua, r, |e| Vector(angle_vectors(e.ang).1)))?;
    method(lua, E, "GetUp", |lua, r, ()| with(lua, r, |e| Vector(angle_vectors(e.ang).2)))?;
    method(lua, E, "LocalToWorld", |lua, r, v: Vector| with(lua, r, |e| Vector(add(e.pos, rotate(v.0, e.ang)))))?;
    method(lua, E, "WorldToLocal", |lua, r, v: Vector| with(lua, r, |e| Vector(unrotate(sub(v.0, e.pos), e.ang))))?;
    method(lua, E, "LocalToWorldAngles", |lua, r, a: Angle| {
        with(lua, r, |e| {
            // compose rotations through basis vectors
            let (f, _, u) = angle_vectors(a.0);
            let (wf, wu) = (rotate(f, e.ang), rotate(u, e.ang));
            Angle(angles_from_basis(wf, wu))
        })
    })?;
    method(lua, E, "WorldToLocalAngles", |lua, r, a: Angle| {
        with(lua, r, |e| {
            let (f, _, u) = angle_vectors(a.0);
            Angle(angles_from_basis(unrotate(f, e.ang), unrotate(u, e.ang)))
        })
    })?;
    method(lua, E, "OBBMins", |lua, r, ()| with(lua, r, |e| Vector(e.mins)))?;
    method(lua, E, "OBBMaxs", |lua, r, ()| with(lua, r, |e| Vector(e.maxs)))?;
    method(lua, E, "OBBCenter", |lua, r, ()| {
        with(lua, r, |e| Vector(scale(add(e.mins, e.maxs), 0.5)))
    })?;
    method(lua, E, "WorldSpaceCenter", |lua, r, ()| with(lua, r, |e| Vector(e.world_center())))?;
    method(lua, E, "WorldSpaceAABB", |lua, r, ()| {
        with(lua, r, |e| {
            let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
            for i in 0..8 {
                let c = [
                    if i & 1 == 0 { e.mins[0] } else { e.maxs[0] },
                    if i & 2 == 0 { e.mins[1] } else { e.maxs[1] },
                    if i & 4 == 0 { e.mins[2] } else { e.maxs[2] },
                ];
                let w = add(e.pos, rotate(c, e.ang));
                for k in 0..3 {
                    lo[k] = lo[k].min(w[k]);
                    hi[k] = hi[k].max(w[k]);
                }
            }
            (Vector(lo), Vector(hi))
        })
    })?;
    method(lua, E, "GetCollisionBounds", |lua, r, ()| with(lua, r, |e| (Vector(e.mins), Vector(e.maxs))))?;
    method(lua, E, "SetCollisionBounds", |lua, r, (a, b): (Vector, Vector)| {
        with(lua, r, |e| {
            e.mins = a.0;
            e.maxs = b.0;
        })
    })?;
    method(lua, E, "SetCollisionBoundsWS", |lua, r, (a, b): (Vector, Vector)| {
        with(lua, r, |e| {
            e.mins = sub(a.0, e.pos);
            e.maxs = sub(b.0, e.pos);
        })
    })?;
    method(lua, E, "BoundingRadius", |lua, r, ()| {
        with(lua, r, |e| {
            let d = sub(e.maxs, e.mins);
            dot(d, d).sqrt() * 0.5
        })
    })?;
    method(lua, E, "GetModelRadius", |lua, r, ()| {
        with(lua, r, |e| {
            let d = sub(e.maxs, e.mins);
            dot(d, d).sqrt() * 0.5
        })
    })?;
    method(lua, E, "NearestPoint", |lua, r, v: Vector| {
        with(lua, r, |e| {
            let l = unrotate(sub(v.0, e.pos), e.ang);
            let c = [
                l[0].clamp(e.mins[0], e.maxs[0]),
                l[1].clamp(e.mins[1], e.maxs[1]),
                l[2].clamp(e.mins[2], e.maxs[2]),
            ];
            Vector(add(e.pos, rotate(c, e.ang)))
        })
    })?;
    method(lua, E, "EyePos", |lua, r, ()| {
        with(lua, r, |e| match &e.player {
            Some(p) => Vector(add(e.pos, if p.crouching { p.view_offset_ducked } else { p.view_offset })),
            None => Vector(e.world_center()),
        })
    })?;
    method(lua, E, "EyeAngles", |lua, r, ()| {
        with(lua, r, |e| Angle(e.player.as_ref().map_or(e.ang, |p| p.eye_angles)))
    })?;

    method(lua, E, "GetModel", |lua, r, ()| with(lua, r, |e| e.model.clone()))?;
    method(lua, E, "SetModel", |lua, r, m: String| set_model(lua, r, &m))?;
    method(lua, E, "SetModelName", |lua, r, m: String| with(lua, r, |e| e.model = m))?;
    method(lua, E, "GetModelScale", |lua, r, ()| with(lua, r, |e| e.model_scale))?;
    method(lua, E, "SetModelScale", |lua, r, (s, _t): (f32, Option<f32>)| with(lua, r, |e| e.model_scale = s))?;
    method(lua, E, "GetSkin", |lua, r, ()| with(lua, r, |e| e.skin))?;
    method(lua, E, "SetSkin", |lua, r, s: i32| with(lua, r, |e| e.skin = s))?;
    method(lua, E, "SkinCount", |_, _, ()| Ok(1))?;
    method(lua, E, "GetMaterial", |lua, r, ()| with(lua, r, |e| e.material.clone()))?;
    method(lua, E, "SetMaterial", |lua, r, m: Option<String>| with(lua, r, |e| e.material = m.unwrap_or_default()))?;
    method(lua, E, "GetSubMaterial", |lua, r, i: Option<i32>| {
        with(lua, r, |e| e.sub_materials.get(&i.unwrap_or(0)).cloned().unwrap_or_default())
    })?;
    method(lua, E, "SetSubMaterial", |lua, r, (i, m): (Option<i32>, Option<String>)| {
        with(lua, r, |e| match i {
            None => e.sub_materials.clear(),
            Some(i) => {
                match m {
                    Some(m) if !m.is_empty() => e.sub_materials.insert(i, m),
                    _ => e.sub_materials.remove(&i),
                };
            }
        })
    })?;
    method(lua, E, "GetMaterials", |lua, _r, ()| lua.create_table())?;
    method(lua, E, "GetBodygroup", |lua, r, i: i32| with(lua, r, |e| e.body_groups.get(&i).copied().unwrap_or(0)))?;
    method(lua, E, "SetBodygroup", |lua, r, (i, v): (i32, i32)| with(lua, r, |e| {
        e.body_groups.insert(i, v);
    }))?;
    method(lua, E, "SetBodyGroups", |lua, r, s: String| {
        with(lua, r, |e| {
            for (i, c) in s.chars().enumerate() {
                if let Some(d) = c.to_digit(36) {
                    e.body_groups.insert(i as i32, d as i32);
                }
            }
        })
    })?;
    method(lua, E, "GetNumBodyGroups", |_, _, ()| Ok(1))?;
    method(lua, E, "GetBodygroupCount", |_, _, _i: i32| Ok(1))?;
    method(lua, E, "GetBodyGroups", |lua, _, ()| lua.create_table())?;
    method(lua, E, "FindBodygroupByName", |_, _, _n: String| Ok(-1))?;
    method(lua, E, "GetColor", |lua, r, ()| {
        let c = with(lua, r, |e| e.color)?;
        color_table(lua, c)
    })?;
    method(lua, E, "SetColor", |lua, r, v: Value| {
        let c = color_from(&v).unwrap_or([255; 4]);
        with(lua, r, |e| e.color = c)
    })?;
    method(lua, E, "GetColor4Part", |lua, r, ()| with(lua, r, |e| (e.color[0], e.color[1], e.color[2], e.color[3])))?;
    method(lua, E, "SetColor4Part", |lua, r, (a, b, c, d): (u8, u8, u8, u8)| with(lua, r, |e| e.color = [a, b, c, d]))?;
    method(lua, E, "GetRenderMode", |lua, r, ()| with(lua, r, |e| e.render_mode))?;
    method(lua, E, "SetRenderMode", |lua, r, m: i32| with(lua, r, |e| e.render_mode = m))?;
    method(lua, E, "GetRenderFX", |lua, r, ()| with(lua, r, |e| e.render_fx))?;
    method(lua, E, "SetRenderFX", |lua, r, m: i32| with(lua, r, |e| e.render_fx = m))?;
    method(lua, E, "GetNoDraw", |lua, r, ()| with(lua, r, |e| e.no_draw))?;
    method(lua, E, "SetNoDraw", |lua, r, b: bool| with(lua, r, |e| e.no_draw = b))?;
    method(lua, E, "DrawShadow", |_, _, _b: Value| Ok(()))?;

    method(lua, E, "Health", |lua, r, ()| with(lua, r, |e| e.health))?;
    method(lua, E, "SetHealth", |lua, r, h: f64| with(lua, r, |e| e.health = h as i32))?;
    method(lua, E, "GetMaxHealth", |lua, r, ()| with(lua, r, |e| e.max_health))?;
    method(lua, E, "SetMaxHealth", |lua, r, h: f64| with(lua, r, |e| e.max_health = h as i32))?;
    method(lua, E, "GetMoveType", |lua, r, ()| with(lua, r, |e| e.move_type))?;
    method(lua, E, "SetMoveType", |lua, r, t: i32| {
        with(lua, r, |e| {
            e.move_type = t;
            if let Some(p) = &mut e.player {
                p.noclip = t == 8;
            }
        })
    })?;
    method(lua, E, "GetSolid", |lua, r, ()| with(lua, r, |e| e.solid))?;
    method(lua, E, "SetSolid", |lua, r, t: i32| with(lua, r, |e| e.solid = t))?;
    method(lua, E, "GetSolidFlags", |lua, r, ()| with(lua, r, |e| e.solid_flags))?;
    method(lua, E, "SetSolidFlags", |lua, r, t: i32| with(lua, r, |e| e.solid_flags = t))?;
    method(lua, E, "AddSolidFlags", |lua, r, t: i32| with(lua, r, |e| e.solid_flags |= t))?;
    method(lua, E, "RemoveSolidFlags", |lua, r, t: i32| with(lua, r, |e| e.solid_flags &= !t))?;
    method(lua, E, "GetCollisionGroup", |lua, r, ()| with(lua, r, |e| e.collision_group))?;
    method(lua, E, "SetCollisionGroup", |lua, r, t: i32| with(lua, r, |e| e.collision_group = t))?;
    method(lua, E, "CollisionRulesChanged", |_, _, ()| Ok(()))?;
    method(lua, E, "SetNotSolid", |lua, r, b: bool| {
        with(lua, r, |e| if b { e.solid_flags |= 4 } else { e.solid_flags &= !4 })
    })?;
    method(lua, E, "GetFlags", |lua, r, ()| with(lua, r, |e| e.flags))?;
    method(lua, E, "AddFlags", |lua, r, f: i64| with(lua, r, |e| e.flags |= f))?;
    method(lua, E, "RemoveFlags", |lua, r, f: i64| with(lua, r, |e| e.flags &= !f))?;
    method(lua, E, "IsFlagSet", |lua, r, f: i64| with(lua, r, |e| e.flags & f != 0))?;
    method(lua, E, "IsOnGround", |lua, r, ()| {
        with(lua, r, |e| e.player.as_ref().map_or(e.flags & 1 != 0, |p| p.on_ground))
    })?;
    method(lua, E, "OnGround", |lua, r, ()| {
        with(lua, r, |e| e.player.as_ref().map_or(e.flags & 1 != 0, |p| p.on_ground))
    })?;
    method(lua, E, "GetGroundEntity", |lua, r, ()| with(lua, r, |e| if e.player.as_ref().is_some_and(|p| p.on_ground) { EntRef { id: 0, serial: 0 } } else { EntRef::NULL }))?;
    method(lua, E, "GetSpawnFlags", |lua, r, ()| with(lua, r, |e| e.spawn_flags))?;
    method(lua, E, "HasSpawnFlags", |lua, r, f: i64| with(lua, r, |e| e.spawn_flags & f == f))?;
    method(lua, E, "SetSpawnFlags", |lua, r, f: i64| with(lua, r, |e| e.spawn_flags = f))?;
    method(lua, E, "AddSpawnFlags", |lua, r, f: i64| with(lua, r, |e| e.spawn_flags |= f))?;
    method(lua, E, "RemoveSpawnFlags", |lua, r, f: i64| with(lua, r, |e| e.spawn_flags &= !f))?;
    method(lua, E, "GetCreationTime", |lua, r, ()| with(lua, r, |e| e.created_at))?;
    method(lua, E, "GetCreationID", |lua, r, ()| with(lua, r, |e| e.r.id as i64 * 1024 + e.r.serial as i64))?;
    method(lua, E, "MapCreationID", |lua, r, ()| with(lua, r, |e| if e.map_entity { e.r.id as i64 + 1234 } else { -1 }))?;
    method(lua, E, "CreatedByMap", |lua, r, ()| with(lua, r, |e| e.map_entity))?;

    method(lua, E, "GetOwner", |lua, r, ()| with(lua, r, |e| e.owner))?;
    method(lua, E, "SetOwner", |lua, r, o: EntRef| with(lua, r, |e| e.owner = o))?;
    method(lua, E, "GetParent", |lua, r, ()| with(lua, r, |e| e.parent))?;
    method(lua, E, "SetParent", |lua, r, (p, _att): (EntRef, Option<i32>)| with(lua, r, |e| e.parent = p))?;
    method(lua, E, "GetChildren", |lua, r, ()| {
        let kids: Vec<EntRef> = store(lua)?.ents.values().filter(|e| e.parent == r).map(|e| e.r).collect();
        lua.create_sequence_from(kids)
    })?;
    method(lua, E, "GetCreator", |lua, r, ()| with(lua, r, |e| e.creator))?;
    method(lua, E, "SetCreator", |lua, r, c: EntRef| with(lua, r, |e| e.creator = c))?;
    method(lua, E, "GetName", |lua, r, ()| {
        with(lua, r, |e| e.player.as_ref().map_or(e.name.clone(), |p| p.name.clone()))
    })?;
    method(lua, E, "SetName", |lua, r, n: String| with(lua, r, |e| e.name = n))?;
    method(lua, E, "SetKeyValue", |lua, r, (k, v): (String, Value)| {
        let v = match &v {
            Value::String(s) => s.to_string_lossy().to_string(),
            Value::Integer(i) => i.to_string(),
            Value::Number(n) => n.to_string(),
            Value::Boolean(b) => (if *b { "1" } else { "0" }).into(),
            _ => String::new(),
        };
        with(lua, r, |e| apply_keyvalue(e, &k, &v))
    })?;
    method(lua, E, "GetKeyValues", |lua, r, ()| {
        let kv = with(lua, r, |e| e.keyvalues.clone())?;
        let t = lua.create_table()?;
        for (k, v) in kv {
            t.set(k, v)?;
        }
        Ok(t)
    })?;
    method(lua, E, "GetInternalVariable", |lua, r, k: String| {
        let v = with(lua, r, |e| e.keyvalues.iter().rev().find(|(a, _)| a.eq_ignore_ascii_case(&k)).map(|(_, v)| v.clone()))?;
        Ok(v)
    })?;
    method(lua, E, "SetSaveValue", |lua, r, (k, v): (String, Value)| {
        let v = match &v {
            Value::String(s) => s.to_string_lossy().to_string(),
            Value::Integer(i) => i.to_string(),
            Value::Number(n) => n.to_string(),
            Value::Boolean(b) => (if *b { "1" } else { "0" }).into(),
            _ => String::new(),
        };
        with(lua, r, |e| {
            e.keyvalues.push((k, v));
            true
        })
    })?;
    method(lua, E, "GetSaveTable", |lua, _r, _a: MultiValue| lua.create_table())?;
    method(lua, E, "GetTable", |lua, r, ()| ent_table(lua, r, true))?;
    method(lua, E, "SetTable", |lua, r, t: Table| {
        let key = lua.create_registry_value(&t)?;
        with(lua, r, |e| e.table = Some(key))
    })?;
    method(lua, E, "GetVar", |lua, r, (k, d): (Value, Value)| {
        let t = ent_table(lua, r, true)?.ok_or_else(null_err)?;
        let v: Value = t.get(k)?;
        Ok(if v.is_nil() { d } else { v })
    })?;
    method(lua, E, "SetVar", |lua, r, (k, v): (Value, Value)| ent_table(lua, r, true)?.ok_or_else(null_err)?.set(k, v))?;

    method(lua, E, "Spawn", |lua, r, ()| spawn(lua, r))?;
    method(lua, E, "Activate", |_, _, ()| Ok(()))?;
    method(lua, E, "Remove", |lua, r, ()| {
        remove(lua, r);
        Ok(())
    })?;
    method(lua, E, "IsMarkedForDeletion", |lua, r, ()| with(lua, r, |e| e.removing))?;
    method(lua, E, "NextThink", |lua, r, t: f64| with(lua, r, |e| e.next_think = Some(t)))?;
    method(lua, E, "Fire", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "Input", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "EmitSound", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "StopSound", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "SetUseType", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "SetTrigger", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "UseTriggerBounds", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "SetShouldServerRagdoll", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "SetPreventTransmit", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "AddEFlags", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "RemoveEFlags", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "IsEFlagSet", |_, _, _a: MultiValue| Ok(false))?;
    method(lua, E, "SetUnFreezable", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "GetUnFreezable", |_, _, ()| Ok(false))?;
    method(lua, E, "SetPersistent", |lua, r, b: bool| with(lua, r, |e| {
        e.nw.insert("Persistent".into(), NetValue::Bool(b));
    }))?;
    method(lua, E, "GetPersistent", |lua, r, ()| with(lua, r, |e| e.nw.get("Persistent") == Some(&NetValue::Bool(true))))?;
    method(lua, E, "PhysicsInit", |lua, r, solid: i32| {
        with(lua, r, |e| {
            e.solid = solid;
            e.phys = Some(PhysData::default());
            e.move_type = 6; // MOVETYPE_VPHYSICS
            true
        })
    })?;
    method(lua, E, "PhysicsInitBox", |lua, r, (a, b): (Vector, Vector)| {
        with(lua, r, |e| {
            e.mins = a.0;
            e.maxs = b.0;
            e.solid = 6;
            e.phys = Some(PhysData::default());
            e.move_type = 6;
            true
        })
    })?;
    method(lua, E, "PhysicsInitSphere", |lua, r, (rad, _m): (f32, Option<String>)| {
        with(lua, r, |e| {
            e.mins = [-rad; 3];
            e.maxs = [rad; 3];
            e.solid = 6;
            e.phys = Some(PhysData::default());
            e.move_type = 6;
            true
        })
    })?;
    method(lua, E, "PhysicsInitStatic", |lua, r, solid: i32| {
        with(lua, r, |e| {
            e.solid = solid;
            let mut p = PhysData::default();
            p.motion = false;
            e.phys = Some(p);
            true
        })
    })?;
    method(lua, E, "PhysicsInitShadow", |lua, r, _a: MultiValue| {
        with(lua, r, |e| {
            let mut p = PhysData::default();
            p.motion = false;
            e.phys = Some(p);
            true
        })
    })?;
    method(lua, E, "PhysicsInitConvex", |lua, r, _a: MultiValue| {
        with(lua, r, |e| {
            e.phys = Some(PhysData::default());
            e.move_type = 6;
            true
        })
    })?;
    method(lua, E, "PhysicsInitMultiConvex", |lua, r, _a: MultiValue| {
        with(lua, r, |e| {
            e.phys = Some(PhysData::default());
            e.move_type = 6;
            true
        })
    })?;
    method(lua, E, "PhysicsDestroy", |lua, r, ()| with(lua, r, |e| e.phys = None))?;
    // motion controllers: ENT:PhysicsSimulate runs every tick (see game::think)
    method(lua, E, "StartMotionController", |lua, r, ()| with(lua, r, |e| {
        e.nw.insert("__srcds_motionctl".into(), NetValue::Bool(true));
    }))?;
    method(lua, E, "StopMotionController", |lua, r, ()| with(lua, r, |e| {
        e.nw.remove("__srcds_motionctl");
    }))?;
    method(lua, E, "AddToMotionController", |_, _, _p: Value| Ok(()))?;
    method(lua, E, "RemoveFromMotionController", |_, _, _p: Value| Ok(()))?;
    method(lua, E, "PhysWake", |lua, r, ()| {
        with(lua, r, |e| {
            if let Some(p) = &mut e.phys {
                p.asleep = false;
            }
        })
    })?;
    method(lua, E, "GetPhysicsObject", |lua, r, ()| -> mlua::Result<Value> {
        let has = store(lua)?.get(r).is_some_and(|e| e.phys.is_some());
        if has {
            lua.create_userdata(PhysRef(r)).map(Value::UserData)
        } else {
            lua.create_userdata(PhysRef(EntRef::NULL)).map(Value::UserData)
        }
    })?;
    method(lua, E, "GetPhysicsObjectNum", |lua, r, i: i32| -> mlua::Result<Value> {
        let has = i == 0 && store(lua)?.get(r).is_some_and(|e| e.phys.is_some());
        lua.create_userdata(PhysRef(if has { r } else { EntRef::NULL })).map(Value::UserData)
    })?;
    method(lua, E, "GetPhysicsObjectCount", |lua, r, ()| with(lua, r, |e| i32::from(e.phys.is_some())))?;
    method(lua, E, "TranslatePhysBoneToBone", |_, _, i: i32| Ok(i))?;
    method(lua, E, "TranslateBoneToPhysBone", |_, _, i: i32| Ok(i))?;
    method(lua, E, "GetBoneCount", |_, _, ()| Ok(1))?;
    method(lua, E, "GetBoneName", |_, _, _i: i32| Ok("static_prop"))?;
    method(lua, E, "LookupBone", |_, _, _n: String| Ok(None::<i32>))?;
    method(lua, E, "GetBonePosition", |lua, r, _i: i32| with(lua, r, |e| (Vector(e.pos), Angle(e.ang))))?;
    method(lua, E, "GetBoneMatrix", |_, _, _i: i32| Ok(()))?;
    method(lua, E, "LookupAttachment", |_, _, _n: String| Ok(0))?;
    method(lua, E, "GetAttachment", |_, _, _i: i32| Ok(()))?;
    method(lua, E, "GetAttachments", |lua, _, ()| lua.create_table())?;
    // animation sequences from the model file; the client animates (client-side
    // animation), the server only networks the sequence
    method(lua, E, "LookupSequence", |lua, r, n: String| {
        let model = with(lua, r, |e| e.model.clone())?;
        let i = model_sequences(lua, &model).iter().position(|(l, _)| l.eq_ignore_ascii_case(&n));
        Ok(match i {
            Some(i) => (i as i32, 1.0),
            None => (-1, 0.0),
        })
    })?;
    method(lua, E, "SelectWeightedSequence", |lua, r, act: i64| {
        let model = with(lua, r, |e| e.model.clone())?;
        Ok(activity_name(lua, act).and_then(|a| sequence_for_activity(lua, &model, &a)).unwrap_or(-1))
    })?;
    method(lua, E, "GetSequenceName", |lua, r, i: i32| {
        let model = with(lua, r, |e| e.model.clone())?;
        Ok(model_sequences(lua, &model).get(i.max(0) as usize).map_or_else(|| "Unknown".to_string(), |s| s.0.clone()))
    })?;
    method(lua, E, "GetSequenceActivityName", |lua, r, i: i32| {
        let model = with(lua, r, |e| e.model.clone())?;
        Ok(model_sequences(lua, &model).get(i.max(0) as usize).map_or_else(String::new, |s| s.1.clone()))
    })?;
    method(lua, E, "GetSequenceCount", |lua, r, ()| {
        let model = with(lua, r, |e| e.model.clone())?;
        Ok(model_sequences(lua, &model).len() as i32)
    })?;
    method(lua, E, "SequenceDuration", |_, _, _a: MultiValue| Ok(1.0))?;
    method(lua, E, "GetSequence", |lua, r, ()| with(lua, r, |e| e.sequence))?;
    for name in ["ResetSequence", "SetSequence"] {
        method(lua, E, name, |lua, r, v: Value| {
            let seq = match v {
                Value::Integer(i) => i as i32,
                Value::Number(n) => n as i32,
                Value::String(s) => {
                    let model = with(lua, r, |e| e.model.clone())?;
                    let n = s.to_string_lossy().to_string();
                    model_sequences(lua, &model).iter().position(|(l, _)| l.eq_ignore_ascii_case(&n)).map_or(-1, |i| i as i32)
                }
                _ => -1,
            };
            with(lua, r, |e| {
                if seq >= 0 {
                    e.sequence = seq;
                }
            })
        })?;
    }
    method(lua, E, "SetPlaybackRate", |lua, r, v: f32| with(lua, r, |e| e.playback_rate = v))?;
    method(lua, E, "GetPlaybackRate", |lua, r, ()| with(lua, r, |e| e.playback_rate))?;
    method(lua, E, "SetCycle", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "GetCycle", |_, _, ()| Ok(0.0))?;
    method(lua, E, "SetPoseParameter", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "GetPoseParameter", |_, _, _a: MultiValue| Ok(0.0))?;
    method(lua, E, "LookupPoseParameter", |_, _, _a: MultiValue| Ok(-1))?;
    method(lua, E, "GetNumPoseParameters", |_, _, ()| Ok(0))?;
    method(lua, E, "ManipulateBonePosition", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "ManipulateBoneAngles", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "ManipulateBoneScale", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "GetFlexNum", |_, _, ()| Ok(0))?;
    method(lua, E, "SetFlexWeight", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "SetFlexScale", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "GetFlexScale", |_, _, ()| Ok(1.0))?;
    method(lua, E, "SetEyeTarget", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "SetLagCompensated", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "GetBrushPlaneCount", |_, _, ()| Ok(0))?;
    method(lua, E, "WaterLevel", |_, _, ()| Ok(0))?;
    method(lua, E, "SetGravity", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "GetGravity", |_, _, ()| Ok(1.0))?;
    method(lua, E, "SetFriction", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "GetFriction", |_, _, ()| Ok(1.0))?;
    method(lua, E, "SetElasticity", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "SetMoveCollide", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "TakeDamage", |lua, r, (dmg, _att, _inf): (f64, Value, Value)| {
        with(lua, r, |e| {
            if e.health > 0 {
                e.health -= dmg as i32;
            }
        })
    })?;
    method(lua, E, "TakeDamageInfo", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "TakePhysicsDamage", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "DeleteOnRemove", |lua, r, other: EntRef| {
        // stored in the Lua table; processed when `r` is removed
        let t = ent_table(lua, r, true)?.ok_or_else(null_err)?;
        let list: Table = match t.raw_get::<Value>("__srcds_delete_on_remove")? {
            Value::Table(l) => l,
            _ => {
                let l = lua.create_table()?;
                t.raw_set("__srcds_delete_on_remove", &l)?;
                l
            }
        };
        list.raw_set(list.raw_len() + 1, other)
    })?;
    method(lua, E, "SetEntity", |lua, r, (name, ent): (String, EntRef)| {
        with(lua, r, |e| {
            e.nw.insert(format!("__srcds_kvent_{}", name.to_ascii_lowercase()), NetValue::Ent(ent));
        })
    })?;
    method(lua, E, "SetPhysConstraintObjects", |lua, r, (p1, p2): (Value, Value)| {
        let of = |v: &Value| match v {
            Value::UserData(u) => u.borrow::<PhysRef>().map(|p| p.0).unwrap_or(EntRef::NULL),
            _ => EntRef::NULL,
        };
        let (a, b) = (of(&p1), of(&p2));
        with(lua, r, |e| {
            e.nw.insert("__srcds_cons_a".into(), NetValue::Ent(a));
            e.nw.insert("__srcds_cons_b".into(), NetValue::Ent(b));
        })
    })?;
    method(lua, E, "DontDeleteOnRemove", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "GetNWVarTable", |lua, r, ()| {
        let vals: Vec<(String, NetValue)> = with(lua, r, |e| e.nw.iter().map(|(k, v)| (k.clone(), v.clone())).collect())?;
        let t = lua.create_table()?;
        for (k, v) in vals {
            t.set(k, v.to_lua(lua)?)?;
        }
        Ok(t)
    })?;

    // NW / NW2 vars share one store
    for (suffix, default) in [
        ("Bool", NetValue::Bool(false)),
        ("Int", NetValue::Int(0)),
        ("Float", NetValue::Float(0.0)),
        ("String", NetValue::Str(String::new())),
        ("Vector", NetValue::Vec([0.0; 3])),
        ("Angle", NetValue::Ang([0.0; 3])),
        ("Entity", NetValue::Ent(EntRef::NULL)),
        ("Var", NetValue::Int(0)),
    ] {
        for prefix in ["NW", "NW2", "Networked", "Networked2"] {
            let def = default.clone();
            let is_var = suffix == "Var";
            method(lua, E, &format!("Get{prefix}{suffix}"), move |lua, r, (k, d): (Value, Value)| -> mlua::Result<Value> {
                let key = key_str(&k);
                let v = match store(lua)?.get(r) {
                    Some(e) => e.nw.get(&key).cloned(),
                    None => return Ok(d),
                };
                match v {
                    Some(v) => v.to_lua(lua),
                    None if !d.is_nil() || is_var => Ok(d),
                    None => def.to_lua(lua),
                }
            })?;
            method(lua, E, &format!("Set{prefix}{suffix}"), move |lua, r, (k, v): (Value, Value)| {
                let key = key_str(&k);
                let nv = NetValue::from_lua_value(&v);
                with(lua, r, |e| match nv {
                    Some(v) => {
                        e.nw.insert(key, v);
                    }
                    None => {
                        e.nw.remove(&key);
                    }
                })
            })?;
        }
    }
    method(lua, E, "SetNWVarProxy", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, E, "SetNW2VarProxy", |_, _, _a: MultiValue| Ok(()))?;

    // DT vars (NetworkVar)
    for (ty, suffix) in [(0u8, "Bool"), (1, "Int"), (2, "Float"), (3, "Vector"), (4, "Angle"), (5, "Entity"), (6, "String")] {
        method(lua, E, &format!("GetDT{suffix}"), move |lua, r, slot: i32| -> mlua::Result<Value> {
            let v = store(lua)?.get(r).and_then(|e| e.dt.get(&(ty, slot)).cloned());
            match v {
                Some(v) => v.to_lua(lua),
                None => match ty {
                    0 => Ok(Value::Boolean(false)),
                    1 => Ok(Value::Integer(0)),
                    2 => Ok(Value::Number(0.0)),
                    3 => Vector([0.0; 3]).into_lua(lua),
                    4 => Angle([0.0; 3]).into_lua(lua),
                    5 => EntRef::NULL.into_lua(lua),
                    _ => Ok(Value::String(lua.create_string("")?)),
                },
            }
        })?;
        method(lua, E, &format!("SetDT{suffix}"), move |lua, r, (slot, v): (i32, Value)| {
            let nv = match (ty, NetValue::from_lua_value(&v)) {
                (1, Some(NetValue::Float(f))) => Some(NetValue::Int(f as i64)),
                (2, Some(NetValue::Int(i))) => Some(NetValue::Float(i as f64)),
                (_, x) => x,
            };
            with(lua, r, |e| {
                if let Some(v) = nv {
                    e.dt.insert((ty, slot), v);
                }
            })
        })?;
    }
    Ok(())
}

fn key_str(k: &Value) -> String {
    match k {
        Value::String(s) => s.to_string_lossy().to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// Builds angles from forward and up vectors.
pub fn angles_from_basis(f: [f32; 3], u: [f32; 3]) -> [f32; 3] {
    let xy = (f[0] * f[0] + f[1] * f[1]).sqrt();
    let pitch = (-f[2]).atan2(xy).to_degrees();
    let yaw = f[1].atan2(f[0]).to_degrees();
    // roll: angle of `u` around forward relative to the zero-roll up vector
    let (_, r0, u0) = angle_vectors([pitch, yaw, 0.0]);
    let roll = (-dot(u, r0)).atan2(dot(u, u0)).to_degrees();
    let _ = r0;
    [pitch, yaw, roll]
}

/// Map/engine keyvalues that change entity fields.
pub fn apply_keyvalue(e: &mut EntData, k: &str, v: &str) {
    let num = |s: &str| s.trim().parse::<f64>().unwrap_or(0.0);
    let vec3 = |s: &str| {
        let mut it = s.split_whitespace().map(|t| t.parse::<f32>().unwrap_or(0.0));
        [it.next().unwrap_or(0.0), it.next().unwrap_or(0.0), it.next().unwrap_or(0.0)]
    };
    match k.to_ascii_lowercase().as_str() {
        "origin" => e.pos = vec3(v),
        "angles" => e.ang = vec3(v),
        "model" => {
            e.model = v.replace('\\', "/");
            e.model_dirty = true;
        }
        "targetname" => e.name = v.to_string(),
        "spawnflags" => e.spawn_flags = num(v) as i64,
        "skin" => e.skin = num(v) as i32,
        "health" => e.health = num(v) as i32,
        "rendercolor" => {
            let c = vec3(v);
            e.color = [c[0] as u8, c[1] as u8, c[2] as u8, e.color[3]];
        }
        "renderamt" => e.color[3] = num(v) as u8,
        "rendermode" => e.render_mode = num(v) as i32,
        "modelscale" => e.model_scale = num(v) as f32,
        _ => {}
    }
    e.keyvalues.push((k.to_string(), v.to_string()));
}

/// `Entity:Spawn()`: marks the entity spawned and runs `ENT:Initialize` for
/// scripted entities.
pub fn spawn(lua: &Lua, r: EntRef) -> mlua::Result<()> {
    let first = with(lua, r, |e| {
        let f = !e.spawned;
        e.spawned = true;
        // engine physics props create their physics object on spawn
        if f && e.class.starts_with("prop_physics") && e.phys.is_none() {
            e.solid = 6;
            e.move_type = 6;
            e.phys = Some(PhysData::default());
        }
        f
    })?;
    if !first {
        return Ok(());
    }
    setup_constraint(lua, r);
    let ud = r.into_lua(lua)?;
    if let Some(t) = ent_table(lua, r, false)? {
        if let Ok(mlua::Value::Function(init)) = t.get::<Value>("Initialize") {
            init.call::<()>(ud.clone())?;
        }
    }
    Ok(())
}

fn kv_vec(e: &EntData, key: &str) -> Option<[f32; 3]> {
    let v = e.keyvalues.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case(key))?.1.clone();
    let mut it = v.split_whitespace().map(|t| t.parse::<f32>().unwrap_or(0.0));
    Some([it.next()?, it.next()?, it.next()?])
}

fn kv_num(e: &EntData, key: &str) -> Option<f32> {
    e.keyvalues.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case(key))?.1.trim().parse().ok()
}

/// Computes the constraint geometry of a constraint entity when it spawns (its
/// position and keyvalues are final by then).
fn setup_constraint(lua: &Lua, r: EntRef) {
    let Ok(mut s) = store(lua) else { return };
    let Some(c) = s.get(r) else { return };
    let (Some(a), Some(b)) = (
        match c.nw.get("__srcds_cons_a") {
            Some(NetValue::Ent(x)) => Some(*x),
            _ => None,
        },
        match c.nw.get("__srcds_cons_b") {
            Some(NetValue::Ent(x)) => Some(*x),
            _ => None,
        },
    ) else {
        return;
    };
    let pose = |x: EntRef| -> ([f32; 3], [f32; 3]) {
        s.get(x).filter(|_| x.id != 0).map_or(([0.0; 3], [0.0; 3]), |e| (e.pos, e.ang))
    };
    let (pa, aa) = pose(a);
    let (pb, ab) = pose(b);
    let class = c.class.clone();
    let here = c.pos;
    let other = kv_vec(c, "attachpoint").or(kv_vec(c, "springaxis")).unwrap_or(here);
    let local = |p: [f32; 3], at: [f32; 3], ang: [f32; 3]| unrotate(sub(at, p), ang);
    let flags = kv_num(c, "spawnflags").unwrap_or(0.0) as i64;
    let (kind, la, lb) = match class.as_str() {
        "phys_constraint" => {
            let (f, _, u) = angle_vectors(ab);
            (
                ConstraintKind::Weld {
                    rel_pos: unrotate(sub(pb, pa), aa),
                    rel_fwd: unrotate(f, aa),
                    rel_up: unrotate(u, aa),
                },
                [0.0; 3],
                [0.0; 3],
            )
        }
        "phys_lengthconstraint" => (
            ConstraintKind::Length {
                length: kv_num(c, "length").unwrap_or(0.0).max(0.0),
                rigid: flags & 2 != 0,
            },
            local(pa, here, aa),
            local(pb, other, ab),
        ),
        "phys_spring" => {
            let d = sub(other, here);
            (
                ConstraintKind::Spring {
                    rest: (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt(),
                    constant: kv_num(c, "constant").unwrap_or(100.0),
                    damping: kv_num(c, "damping").unwrap_or(0.0),
                },
                local(pa, here, aa),
                local(pb, other, ab),
            )
        }
        "phys_ballsocket" | "phys_hinge" | "phys_ragdollconstraint" => {
            (ConstraintKind::Point, local(pa, here, aa), local(pb, here, ab))
        }
        _ => (ConstraintKind::Other, [0.0; 3], [0.0; 3]),
    };
    if let Some(c) = s.get_mut(r) {
        c.constraint = Some(Constraint { a, b, local_a: la, local_b: lb, kind });
    }
}

/// `Entity:Remove()`: runs `ENT:OnRemove`, `EntityRemoved` and dependants; the slot
/// is freed immediately (the host sees the id in `removed`).
pub fn remove(lua: &Lua, r: EntRef) {
    if r.id == 0 || !valid(lua, r) {
        return;
    }
    let _ = with(lua, r, |e| e.removing = true);
    let Ok(ud) = r.into_lua(lua) else { return };
    if let Ok(hook) = lua.globals().get::<Table>("hook") {
        if let Ok(call) = hook.get::<mlua::Function>("Run") {
            if let Err(e) = call.call::<()>(("EntityRemoved", ud.clone())) {
                tracing::warn!(error = %e, "EntityRemoved");
            }
        }
    }
    let mut dependants = Vec::new();
    if let Ok(Some(t)) = ent_table(lua, r, false) {
        if let Ok(Value::Function(f)) = t.get::<Value>("OnRemove") {
            if let Err(e) = f.call::<()>(ud.clone()) {
                tracing::warn!(error = %e, "ENT:OnRemove");
            }
        }
        if let Ok(Value::Table(l)) = t.raw_get::<Value>("__srcds_delete_on_remove") {
            for v in l.sequence_values::<EntRef>().flatten() {
                dependants.push(v);
            }
        }
    }
    // weapons owned by a removed player and the player's references
    let removed = {
        let Ok(mut s) = store(lua) else { return };
        let Some(e) = s.ents.remove(&r.id) else { return };
        for other in s.ents.values_mut() {
            if let Some(p) = &mut other.player {
                p.weapons.retain(|w| *w != r);
                if p.active_weapon == r {
                    p.active_weapon = EntRef::NULL;
                }
            }
        }
        s.removed.push(r);
        e
    };
    if let Some(k) = removed.table {
        let _ = lua.remove_registry_value(k);
    }
    if let Some(k) = removed.userdata {
        let _ = lua.remove_registry_value(k);
    }
    for d in dependants {
        remove(lua, d);
    }
}

fn install_player(lua: &Lua) -> mlua::Result<()> {
    const P: &str = "Player";
    method(lua, P, "Nick", |lua, r, ()| with_player(lua, r, |p, _| p.name.clone()))?;
    method(lua, P, "Name", |lua, r, ()| with_player(lua, r, |p, _| p.name.clone()))?;
    method(lua, P, "GetName", |lua, r, ()| with_player(lua, r, |p, _| p.name.clone()))?;
    method(lua, P, "UserID", |lua, r, ()| with_player(lua, r, |p, _| p.user_id))?;
    method(lua, P, "SteamID64", |lua, r, ()| with_player(lua, r, |p, _| if p.bot { "0".to_string() } else { p.steam_id64.to_string() }))?;
    method(lua, P, "SteamID", |lua, r, ()| {
        with_player(lua, r, |p, _| {
            if p.bot {
                return "BOT".to_string();
            }
            if p.steam_id64 == 0 {
                return "STEAM_ID_LAN".to_string();
            }
            let account = (p.steam_id64 & 0xFFFF_FFFF) as u32;
            format!("STEAM_0:{}:{}", account & 1, account >> 1)
        })
    })?;
    method(lua, P, "AccountID", |lua, r, ()| with_player(lua, r, |p, _| (p.steam_id64 & 0xFFFF_FFFF) as i64))?;
    method(lua, P, "UniqueID", |lua, r, ()| {
        with_player(lua, r, |p, _| {
            // CRC32 of "gm_<steamid>_gm" in GMod; a stable per-player number here
            let s = format!("gm_{}_gm", p.steam_id64);
            crc32(s.as_bytes()) as i64
        })
    })?;
    method(lua, P, "IsBot", |lua, r, ()| with_player(lua, r, |p, _| p.bot))?;
    method(lua, P, "IsListenServerHost", |_, _, ()| Ok(false))?;
    method(lua, P, "IsFullyAuthenticated", |_, _, ()| Ok(true))?;
    method(lua, P, "IsConnected", |lua, r, ()| Ok(valid(lua, r)))?;
    method(lua, P, "IsTimingOut", |_, _, ()| Ok(false))?;
    method(lua, P, "Ping", |lua, r, ()| {
        with(lua, r, |e| match e.nw.get("__srcds_ping") {
            Some(NetValue::Int(i)) => *i,
            _ => 0,
        })
    })?;
    method(lua, P, "PacketLoss", |_, _, ()| Ok(0))?;
    method(lua, P, "TimeConnected", |lua, r, ()| {
        let now = store(lua)?.cur_time;
        with(lua, r, |e| now - e.created_at)
    })?;
    method(lua, P, "IPAddress", |lua, r, ()| {
        with(lua, r, |e| match e.nw.get("__srcds_ip") {
            Some(NetValue::Str(s)) => s.clone(),
            _ => "loopback".to_string(),
        })
    })?;
    method(lua, P, "GetUserGroup", |lua, r, ()| with_player(lua, r, |p, _| p.user_group.clone()))?;
    method(lua, P, "Alive", |lua, r, ()| with_player(lua, r, |p, _| p.alive))?;
    method(lua, P, "Armor", |lua, r, ()| with_player(lua, r, |p, _| p.armor))?;
    method(lua, P, "SetArmor", |lua, r, a: f64| with_player(lua, r, |p, _| p.armor = a as i32))?;
    method(lua, P, "GetMaxArmor", |lua, r, ()| with_player(lua, r, |p, _| p.max_armor))?;
    method(lua, P, "SetMaxArmor", |lua, r, a: f64| with_player(lua, r, |p, _| p.max_armor = a as i32))?;
    method(lua, P, "Frags", |lua, r, ()| with_player(lua, r, |p, _| p.frags))?;
    method(lua, P, "Deaths", |lua, r, ()| with_player(lua, r, |p, _| p.deaths))?;
    method(lua, P, "AddFrags", |lua, r, n: i32| with_player(lua, r, |p, _| p.frags += n))?;
    method(lua, P, "AddDeaths", |lua, r, n: i32| with_player(lua, r, |p, _| p.deaths += n))?;
    method(lua, P, "SetFrags", |lua, r, n: i32| with_player(lua, r, |p, _| p.frags = n))?;
    method(lua, P, "SetDeaths", |lua, r, n: i32| with_player(lua, r, |p, _| p.deaths = n))?;
    method(lua, P, "Team", |lua, r, ()| with_player(lua, r, |p, _| p.team))?;
    method(lua, P, "SetTeam", |lua, r, t: i32| with_player(lua, r, |p, _| p.team = t))?;
    for (name, field) in [
        ("WalkSpeed", 0usize),
        ("RunSpeed", 1),
        ("SlowWalkSpeed", 2),
        ("CrouchedWalkSpeed", 3),
        ("DuckSpeed", 4),
        ("UnDuckSpeed", 5),
        ("JumpPower", 6),
        ("MaxSpeed", 7),
    ] {
        fn slot(p: &mut PlayerData, i: usize) -> &mut f32 {
            match i {
                0 => &mut p.walk_speed,
                1 => &mut p.run_speed,
                2 => &mut p.slow_walk_speed,
                3 => &mut p.crouched_walk_speed,
                4 => &mut p.duck_speed,
                5 => &mut p.unduck_speed,
                6 => &mut p.jump_power,
                _ => &mut p.max_speed,
            }
        }
        method(lua, P, &format!("Get{name}"), move |lua, r, ()| with_player(lua, r, |p, _| *slot(p, field)))?;
        method(lua, P, &format!("Set{name}"), move |lua, r, v: f32| with_player(lua, r, |p, _| *slot(p, field) = v))?;
    }
    method(lua, P, "SetPlayerColor", |lua, r, v: Vector| with(lua, r, |e| {
        e.nw.insert("__srcds_playercolor".into(), NetValue::Vec(v.0));
    }))?;
    method(lua, P, "GetPlayerColor", |lua, r, ()| {
        with(lua, r, |e| match e.nw.get("__srcds_playercolor") {
            Some(NetValue::Vec(v)) => Vector(*v),
            _ => Vector([0.24, 0.34, 0.41]),
        })
    })?;
    method(lua, P, "SetWeaponColor", |lua, r, v: Vector| with(lua, r, |e| {
        e.nw.insert("__srcds_weaponcolor".into(), NetValue::Vec(v.0));
    }))?;
    method(lua, P, "GetWeaponColor", |lua, r, ()| {
        with(lua, r, |e| match e.nw.get("__srcds_weaponcolor") {
            Some(NetValue::Vec(v)) => Vector(*v),
            _ => Vector([0.3, 1.8, 2.1]),
        })
    })?;
    method(lua, P, "SetViewOffset", |lua, r, v: Vector| with_player(lua, r, |p, _| p.view_offset = v.0))?;
    method(lua, P, "GetViewOffset", |lua, r, ()| with_player(lua, r, |p, _| Vector(p.view_offset)))?;
    method(lua, P, "SetViewOffsetDucked", |lua, r, v: Vector| with_player(lua, r, |p, _| p.view_offset_ducked = v.0))?;
    method(lua, P, "GetViewOffsetDucked", |lua, r, ()| with_player(lua, r, |p, _| Vector(p.view_offset_ducked)))?;
    method(lua, P, "SetHull", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "SetHullDuck", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "GetHull", |_, _, ()| Ok((Vector([-16.0, -16.0, 0.0]), Vector([16.0, 16.0, 72.0]))))?;
    method(lua, P, "GetHullDuck", |_, _, ()| Ok((Vector([-16.0, -16.0, 0.0]), Vector([16.0, 16.0, 36.0]))))?;
    method(lua, P, "SetStepSize", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "GetStepSize", |_, _, ()| Ok(18.0))?;
    method(lua, P, "SetEyeAngles", |lua, r, a: Angle| {
        with_player(lua, r, |p, _| {
            p.eye_angles = a.0;
            p.eye_angles_set = true;
        })
    })?;
    method(lua, P, "GetAimVector", |lua, r, ()| with_player(lua, r, |p, _| Vector(p.world_click.unwrap_or_else(|| angle_vectors(p.eye_angles).0))))?;
    method(lua, P, "GetShootPos", |lua, r, ()| {
        with_player(lua, r, |p, pos| Vector(add(*pos, if p.crouching { p.view_offset_ducked } else { p.view_offset })))
    })?;
    method(lua, P, "Crouching", |lua, r, ()| with_player(lua, r, |p, _| p.crouching))?;
    method(lua, P, "KeyDown", |lua, r, k: i32| with_player(lua, r, |p, _| p.buttons & k != 0))?;
    method(lua, P, "KeyDownLast", |lua, r, k: i32| with_player(lua, r, |p, _| p.old_buttons & k != 0))?;
    method(lua, P, "KeyPressed", |lua, r, k: i32| with_player(lua, r, |p, _| p.buttons & k != 0 && p.old_buttons & k == 0))?;
    method(lua, P, "KeyReleased", |lua, r, k: i32| with_player(lua, r, |p, _| p.buttons & k == 0 && p.old_buttons & k != 0))?;
    method(lua, P, "GetFOV", |lua, r, ()| with_player(lua, r, |p, _| if p.fov > 0.0 { p.fov } else { 75.0 }))?;
    method(lua, P, "SetFOV", |lua, r, (f, _t): (f32, Option<f32>)| with_player(lua, r, |p, _| p.fov = f))?;
    // an unknown client convar reads as "" (tools pass the value on as a string)
    method(lua, P, "GetInfo", |lua, r, k: String| {
        with_player(lua, r, |p, _| p.info.get(&k.to_ascii_lowercase()).cloned().unwrap_or_default())
    })?;
    method(lua, P, "GetInfoNum", |lua, r, (k, d): (String, f64)| {
        with_player(lua, r, |p, _| {
            p.info
                .get(&k.to_ascii_lowercase())
                .and_then(|v| v.trim().parse::<f64>().ok())
                .unwrap_or(d)
        })
    })?;
    method(lua, P, "ChatPrint", |lua, r, s: String| with_player(lua, r, |p, _| p.outbox.push((3, s))))?;
    method(lua, P, "PrintMessage", |lua, r, (kind, s): (u8, String)| with_player(lua, r, |p, _| p.outbox.push((kind, s))))?;
    method(lua, P, "SendLua", |lua, r, s: String| with_player(lua, r, |p, _| p.send_lua.push(s)))?;
    method(lua, P, "ConCommand", |lua, r, s: String| with_player(lua, r, |p, _| p.client_commands.push(s)))?;
    // like the global RunConsoleCommand but on the player's client; arguments are
    // quoted so they stay one argument each
    method(lua, P, "RunConsoleCommand", |lua, r, args: MultiValue| {
        let mut it = args.into_iter();
        let Some(cmd) = it.next().map(|v| key_str(&v)) else { return Ok(()) };
        let mut line = cmd;
        for a in it {
            let s = key_str(&a).replace('"', "");
            line.push_str(&format!(" \"{s}\""));
        }
        with_player(lua, r, |p, _| p.client_commands.push(line))
    })?;
    method(lua, P, "SendHint", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "SuppressHint", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "Kick", |lua, r, reason: Option<String>| {
        with(lua, r, |e| {
            e.nw.insert("__srcds_kick".into(), NetValue::Str(reason.unwrap_or_else(|| "Kicked".into())));
        })
    })?;
    // deaths are run by the host on the next tick (hooks, weapon removal)
    method(lua, P, "Kill", |lua, r, ()| with(lua, r, |e| {
        e.nw.insert("__srcds_die".into(), NetValue::Int(1));
    }))?;
    method(lua, P, "KillSilent", |lua, r, ()| with(lua, r, |e| {
        e.nw.insert("__srcds_die".into(), NetValue::Int(2));
    }))?;
    method(lua, P, "Spawn", |lua, r, ()| {
        // handled by the host on the next tick
        with(lua, r, |e| {
            e.nw.insert("__srcds_respawn".into(), NetValue::Bool(true));
        })
    })?;
    method(lua, P, "GetObserverMode", |_, _, ()| Ok(0))?;
    method(lua, P, "GetObserverTarget", |_, _, ()| Ok(EntRef::NULL))?;
    method(lua, P, "Spectate", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "SpectateEntity", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "UnSpectate", |_, _, ()| Ok(()))?;
    method(lua, P, "Freeze", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "Lock", |_, _, ()| Ok(()))?;
    method(lua, P, "UnLock", |_, _, ()| Ok(()))?;
    method(lua, P, "IsFrozen", |_, _, ()| Ok(false))?;
    method(lua, P, "InVehicle", |_, _, ()| Ok(false))?;
    method(lua, P, "GetVehicle", |_, _, ()| Ok(EntRef::NULL))?;
    method(lua, P, "AllowFlashlight", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "CanUseFlashlight", |_, _, ()| Ok(true))?;
    method(lua, P, "FlashlightIsOn", |_, _, ()| Ok(false))?;
    method(lua, P, "Flashlight", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "CrosshairEnable", |_, _, ()| Ok(()))?;
    method(lua, P, "CrosshairDisable", |_, _, ()| Ok(()))?;
    method(lua, P, "SetNoCollideWithTeammates", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "SetAvoidPlayers", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "ShouldDropWeapon", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "SetAllowWeaponsInVehicle", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "SetCanWalk", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "SetCanZoom", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "SetSuppressPickupNotices", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "SetLaggedMovementValue", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "GetLaggedMovementValue", |_, _, ()| Ok(1.0))?;
    method(lua, P, "SetDSP", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "ScreenFade", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "ViewPunch", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "SetClassID", |lua, r, id: i64| with(lua, r, |e| {
        e.nw.insert("__srcds_class".into(), NetValue::Int(id));
    }))?;
    method(lua, P, "GetClassID", |lua, r, ()| {
        with(lua, r, |e| match e.nw.get("__srcds_class") {
            Some(NetValue::Int(i)) => *i,
            _ => 0,
        })
    })?;
    method(lua, P, "GetViewEntity", |lua, r, ()| {
        with_player(lua, r, |p, _| if p.view_entity.is_null() { r } else { p.view_entity })
    })?;
    method(lua, P, "SetViewEntity", |lua, r, v: Option<EntRef>| {
        // the player itself (or nil / NULL) = the normal view
        let v = v.filter(|v| *v != r).unwrap_or(EntRef::NULL);
        with_player(lua, r, |p, _| p.view_entity = v)
    })?;
    method(lua, P, "GetHands", |lua, r, ()| with_player(lua, r, |p, _| p.hands))?;
    method(lua, P, "SetHands", |lua, r, h: EntRef| with_player(lua, r, |p, _| p.hands = h))?;
    method(lua, P, "GetViewModel", |lua, r, i: Option<i32>| {
        with_player(lua, r, |p, _| if i.unwrap_or(0) == 0 { p.view_model } else { EntRef::NULL })
    })?;
    method(lua, P, "LastHitGroup", |_, _, ()| Ok(0))?;
    method(lua, P, "GetPreferredCarryAngles", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "IsPlayingTaunt", |_, _, ()| Ok(false))?;
    method(lua, P, "IsSprinting", |lua, r, ()| with_player(lua, r, |p, _| p.buttons & (1 << 17) != 0))?;
    method(lua, P, "IsWalking", |lua, r, ()| with_player(lua, r, |p, _| p.buttons & (1 << 18) != 0))?;
    method(lua, P, "GetCurrentCommand", |_, _, ()| Ok(()))?;
    method(lua, P, "LagCompensation", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "DrawViewModel", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "DrawWorldModel", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "SetupHands", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "SetPData", |lua, r, (k, v): (Value, Value)| {
        let sid = with_player(lua, r, |p, _| p.steam_id64)?;
        let v = NetValue::from_lua_value(&v);
        let mut pd = lua.app_data_mut::<PData>().unwrap();
        let key = (sid, key_str_owned(&k));
        match v {
            Some(v) => pd.0.insert(key, v),
            None => pd.0.remove(&key),
        };
        Ok(true)
    })?;
    method(lua, P, "GetPData", |lua, r, (k, d): (Value, Value)| -> mlua::Result<Value> {
        let sid = with_player(lua, r, |p, _| p.steam_id64)?;
        let v = lua.app_data_ref::<PData>().unwrap().0.get(&(sid, key_str_owned(&k))).cloned();
        match v {
            Some(v) => v.to_lua(lua),
            None => Ok(d),
        }
    })?;
    method(lua, P, "RemovePData", |lua, r, k: Value| {
        let sid = with_player(lua, r, |p, _| p.steam_id64)?;
        lua.app_data_mut::<PData>().unwrap().0.remove(&(sid, key_str_owned(&k)));
        Ok(true)
    })?;
    lua.set_app_data(PData::default());

    // weapons and ammo
    method(lua, P, "GetWeapons", |lua, r, ()| {
        let w = with_player(lua, r, |p, _| p.weapons.clone())?;
        lua.create_sequence_from(w)
    })?;
    method(lua, P, "GetWeapon", |lua, r, class: String| {
        let w = with_player(lua, r, |p, _| p.weapons.clone())?;
        let s = store(lua)?;
        Ok(w.into_iter().find(|w| s.get(*w).is_some_and(|e| e.class == class)).unwrap_or(EntRef::NULL))
    })?;
    method(lua, P, "HasWeapon", |lua, r, class: Option<String>| {
        let Some(class) = class else { return Ok(false) };
        let w = with_player(lua, r, |p, _| p.weapons.clone())?;
        let s = store(lua)?;
        Ok(w.into_iter().any(|w| s.get(w).is_some_and(|e| e.class == class)))
    })?;
    method(lua, P, "GetActiveWeapon", |lua, r, ()| with_player(lua, r, |p, _| p.active_weapon))?;
    method(lua, P, "SelectWeapon", |lua, r, class: String| {
        let w = with_player(lua, r, |p, _| p.weapons.clone())?;
        let found = {
            let s = store(lua)?;
            w.into_iter().find(|w| s.get(*w).is_some_and(|e| e.class == class))
        };
        if let Some(w) = found {
            with_player(lua, r, |p, _| p.active_weapon = w)?;
        }
        Ok(())
    })?;
    method(lua, P, "SetActiveWeapon", |lua, r, w: EntRef| with_player(lua, r, |p, _| p.active_weapon = w))?;
    method(lua, P, "StripWeapons", |lua, r, ()| {
        let w = with_player(lua, r, |p, _| std::mem::take(&mut p.weapons))?;
        with_player(lua, r, |p, _| p.active_weapon = EntRef::NULL)?;
        for w in w {
            remove(lua, w);
        }
        Ok(())
    })?;
    method(lua, P, "StripWeapon", |lua, r, class: String| {
        let w = with_player(lua, r, |p, _| p.weapons.clone())?;
        let found = {
            let s = store(lua)?;
            w.into_iter().find(|w| s.get(*w).is_some_and(|e| e.class == class))
        };
        if let Some(w) = found {
            remove(lua, w);
        }
        Ok(())
    })?;
    method(lua, P, "DropWeapon", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, P, "Give", |lua, r, (class, no_ammo): (String, Option<bool>)| give(lua, r, &class, no_ammo.unwrap_or(false)))?;
    method(lua, P, "GiveAmmo", |lua, r, (n, ty, _hide): (i32, Value, Option<bool>)| {
        let k = crate::ammo::ammo_key(lua, &ty);
        with_player(lua, r, |p, _| {
            *p.ammo.entry(k).or_default() += n;
            n
        })
    })?;
    method(lua, P, "GetAmmoCount", |lua, r, ty: Value| {
        let k = crate::ammo::ammo_key(lua, &ty);
        with_player(lua, r, |p, _| p.ammo.get(&k).copied().unwrap_or(0))
    })?;
    method(lua, P, "SetAmmo", |lua, r, (n, ty): (i32, Value)| {
        let k = crate::ammo::ammo_key(lua, &ty);
        with_player(lua, r, |p, _| {
            p.ammo.insert(k, n);
        })
    })?;
    method(lua, P, "RemoveAmmo", |lua, r, (n, ty): (i32, Value)| {
        let k = crate::ammo::ammo_key(lua, &ty);
        with_player(lua, r, |p, _| {
            let a = p.ammo.entry(k).or_default();
            *a = (*a - n).max(0);
        })
    })?;
    method(lua, P, "RemoveAllAmmo", |lua, r, ()| with_player(lua, r, |p, _| p.ammo.clear()))?;
    method(lua, P, "StripAmmo", |lua, r, ()| with_player(lua, r, |p, _| p.ammo.clear()))?;
    method(lua, P, "GetAmmo", |lua, r, ()| {
        let a = with_player(lua, r, |p, _| p.ammo.clone())?;
        let t = lua.create_table()?;
        for (k, v) in a {
            t.set(k, v)?;
        }
        Ok(t)
    })?;
    Ok(())
}

fn key_str_owned(k: &Value) -> String {
    key_str(k)
}

/// `Player:SetPData` storage (per SteamID64; in memory until sql is backed by a file).
#[derive(Default)]
pub struct PData(pub HashMap<(u64, String), NetValue>);

/// Starts reloading weapon `w` (plays `activity` on the owner's view model) when its
/// clip is not full and the owner has reserve ammo; the clip fills when the reload
/// animation has run ([`finish_reload`]). Returns whether a reload started.
pub fn default_reload(lua: &Lua, w: EntRef, activity: &str) -> bool {
    let Ok((owner, clip, max, ammo, busy)) = with(lua, w, |e| (e.weapon_owner, e.clip1, e.max_clip1, e.ammo1.clone(), e.nw.contains_key("__srcds_in_reload"))) else {
        return false;
    };
    if busy || max <= 0 || clip >= max || ammo.is_empty() {
        return false;
    }
    let reserve = with_player(lua, owner, |p, _| p.ammo.get(&ammo).copied().unwrap_or(0)).unwrap_or(0);
    if reserve <= 0 {
        return false;
    }
    // the reload animation's length sets when the next attack (and the clip fill)
    // comes; the weapon logic resolves it
    let _ = with(lua, w, |e| {
        e.nw.insert("__srcds_in_reload".into(), NetValue::Int(1));
        e.nw.insert("__srcds_reload_start".into(), NetValue::Int(1));
    });
    let vm = with_player(lua, owner, |p, _| p.view_model).unwrap_or(EntRef::NULL);
    set_view_model_activity(lua, vm, activity);
    true
}

/// Ends a reload: the clip takes what the owner's reserve has.
pub fn finish_reload(lua: &Lua, w: EntRef) {
    let Ok((owner, clip, max, ammo)) = with(lua, w, |e| {
        e.nw.remove("__srcds_in_reload");
        (e.weapon_owner, e.clip1.max(0), e.max_clip1, e.ammo1.clone())
    }) else {
        return;
    };
    let take = with_player(lua, owner, |p, _| {
        let r = p.ammo.entry(ammo.clone()).or_default();
        let take = (max - clip).min(*r).max(0);
        *r -= take;
        take
    })
    .unwrap_or(0);
    let _ = with(lua, w, |e| e.clip1 = clip + take);
}

/// Ammo setup of a new weapon: clip sizes and ammo types from the SWEP table
/// (`Primary` / `Secondary`) or the engine weapon script; the default clip fills the
/// clip and the rest goes to the owner's reserve.
fn setup_weapon_ammo(lua: &Lua, w: EntRef, ply: EntRef, class: &str, no_ammo: bool) -> mlua::Result<()> {
    let mut slots: [(i32, i32, String); 2] = [(-1, 0, String::new()), (-1, 0, String::new())];
    if let Some(t) = ent_table(lua, w, false)? {
        for (i, key) in ["Primary", "Secondary"].iter().enumerate() {
            if let Ok(Value::Table(p)) = t.get::<Value>(*key) {
                let num = |k: &str| match p.get::<Value>(k) {
                    Ok(Value::Integer(n)) => n as i32,
                    Ok(Value::Number(n)) => n as i32,
                    _ => -1,
                };
                let ammo = match p.get::<Value>("Ammo") {
                    Ok(Value::String(s)) => s.to_string_lossy().to_string(),
                    _ => String::new(),
                };
                slots[i] = (num("ClipSize"), num("DefaultClip").max(0), ammo);
            }
        }
    } else {
        let text = lua
            .app_data_ref::<EntityStore>()
            .and_then(|s| s.models.as_ref().and_then(|m| m.read(&format!("scripts/weapons/{class}.txt"))))
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .unwrap_or_default();
        let key = |k: &str| -> Option<String> {
            let mut it = text.split('"').skip(1).step_by(2);
            while let Some(t) = it.next() {
                if t.eq_ignore_ascii_case(k) {
                    return it.next().map(str::to_string);
                }
            }
            None
        };
        let num = |k: &str, d: i32| key(k).and_then(|v| v.trim().parse::<i32>().ok()).unwrap_or(d);
        // [факт] HL2 scripts (weapon_pistol) give clip_size only: the default clip is
        // then the clip size
        let (c1, c2) = (num("clip_size", -1), num("clip2_size", -1));
        slots[0] = (c1, num("default_clip", c1).max(0), key("primary_ammo").unwrap_or_default());
        slots[1] = (c2, num("default_clip2", c2).max(0), key("secondary_ammo").unwrap_or_default());
    }
    let canon = |a: &str| {
        let id = crate::ammo::ammo_id(lua, a);
        if id > 0 { crate::ammo::ammo_name(lua, id as i64).unwrap_or_default() } else { String::new() }
    };
    let (a1, a2) = (canon(&slots[0].2), canon(&slots[1].2));
    let mut reserve: Vec<(String, i32)> = Vec::new();
    let mut clip = |size: i32, default: i32, ammo: &str| -> i32 {
        if no_ammo {
            return if size > 0 { 0 } else { -1 };
        }
        if size > 0 {
            let c = default.min(size);
            if default > c && !ammo.is_empty() {
                reserve.push((ammo.to_string(), default - c));
            }
            c
        } else {
            if default > 0 && !ammo.is_empty() {
                reserve.push((ammo.to_string(), default));
            }
            -1
        }
    };
    let c1 = clip(slots[0].0, slots[0].1, &a1);
    let c2 = clip(slots[1].0, slots[1].1, &a2);
    with(lua, w, |e| {
        e.max_clip1 = slots[0].0;
        e.max_clip2 = slots[1].0;
        e.clip1 = c1;
        e.clip2 = c2;
        e.ammo1 = a1.clone();
        e.ammo2 = a2.clone();
    })?;
    with_player(lua, ply, |p, _| {
        for (a, n) in reserve {
            *p.ammo.entry(a).or_default() += n;
        }
    })?;
    Ok(())
}

/// Creates a weapon entity for a player (`Player:Give`).
pub fn give(lua: &Lua, ply: EntRef, class: &str, no_ammo: bool) -> mlua::Result<EntRef> {
    // already owned?
    let owned = with_player(lua, ply, |p, _| p.weapons.clone())?;
    {
        let s = store(lua)?;
        if owned.iter().any(|w| s.get(*w).is_some_and(|e| e.class == class)) {
            return Ok(EntRef::NULL);
        }
    }
    let w = crate::lib_ents::create(lua, class)?;
    if w.is_null() {
        return Ok(w);
    }
    let pos = with(lua, ply, |e| e.pos)?;
    with(lua, w, |e| {
        e.pos = pos;
        e.owner = ply;
        e.weapon_owner = ply;
        e.solid = 0;
        e.effects |= 0x20; // EF_NODRAW until deployed
    })?;
    spawn(lua, w)?;
    setup_weapon_ammo(lua, w, ply, class, no_ammo)?;
    let first = with_player(lua, ply, |p, _| {
        p.weapons.push(w);
        if p.active_weapon.is_null() {
            p.active_weapon = w;
            true
        } else {
            false
        }
    })?;
    // scripted weapons: owner-equip callbacks
    if let Some(t) = ent_table(lua, w, false)? {
        let wl = w.into_lua(lua)?;
        let pl = ply.into_lua(lua)?;
        if let Ok(Value::Function(f)) = t.get::<Value>("Equip") {
            f.call::<()>((wl.clone(), pl.clone()))?;
        }
        if first {
            if let Ok(Value::Function(f)) = t.get::<Value>("Deploy") {
                f.call::<()>(wl.clone())?;
            }
        }
    }
    if let Ok(hook) = lua.globals().get::<Table>("hook") {
        if let Ok(run) = hook.get::<mlua::Function>("Run") {
            run.call::<()>(("WeaponEquip", w, ply))?;
        }
    }
    Ok(w)
}

fn install_weapon(lua: &Lua) -> mlua::Result<()> {
    const W: &str = "Weapon";
    method(lua, W, "Clip1", |lua, r, ()| with(lua, r, |e| e.clip1))?;
    method(lua, W, "Clip2", |lua, r, ()| with(lua, r, |e| e.clip2))?;
    method(lua, W, "SetClip1", |lua, r, n: i32| with(lua, r, |e| e.clip1 = n))?;
    method(lua, W, "SetClip2", |lua, r, n: i32| with(lua, r, |e| e.clip2 = n))?;
    method(lua, W, "GetPrimaryAmmoType", |lua, r, ()| {
        let a = with(lua, r, |e| e.ammo1.clone())?;
        Ok(if a.is_empty() { -1 } else { crate::ammo::ammo_id(lua, &a) })
    })?;
    method(lua, W, "GetSecondaryAmmoType", |lua, r, ()| {
        let a = with(lua, r, |e| e.ammo2.clone())?;
        Ok(if a.is_empty() { -1 } else { crate::ammo::ammo_id(lua, &a) })
    })?;
    method(lua, W, "GetMaxClip1", |lua, r, ()| with(lua, r, |e| e.max_clip1))?;
    method(lua, W, "GetMaxClip2", |lua, r, ()| with(lua, r, |e| e.max_clip2))?;
    method(lua, W, "IsWeaponVisible", |_, _, ()| Ok(true))?;
    method(lua, W, "GetNextPrimaryFire", |lua, r, ()| {
        with(lua, r, |e| match e.nw.get("__srcds_npf") {
            Some(NetValue::Float(f)) => *f,
            _ => 0.0,
        })
    })?;
    method(lua, W, "SetNextPrimaryFire", |lua, r, t: f64| with(lua, r, |e| {
        e.nw.insert("__srcds_npf".into(), NetValue::Float(t));
    }))?;
    method(lua, W, "GetNextSecondaryFire", |lua, r, ()| {
        with(lua, r, |e| match e.nw.get("__srcds_nsf") {
            Some(NetValue::Float(f)) => *f,
            _ => 0.0,
        })
    })?;
    method(lua, W, "SetNextSecondaryFire", |lua, r, t: f64| with(lua, r, |e| {
        e.nw.insert("__srcds_nsf".into(), NetValue::Float(t));
    }))?;
    // the owner's view model plays the activity (the networked sequence and a
    // restart counter; the sequence is looked up in the view model's model)
    method(lua, W, "SendWeaponAnim", |lua, r, act: i64| {
        let Some(name) = activity_name(lua, act) else { return Ok(()) };
        let owner = with(lua, r, |e| e.weapon_owner)?;
        let vm = with_player(lua, owner, |p, _| p.view_model).unwrap_or(EntRef::NULL);
        set_view_model_activity(lua, vm, &name);
        Ok(())
    })?;
    method(lua, W, "SetHoldType", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, W, "GetHoldType", |_, _, ()| Ok("normal"))?;
    method(lua, W, "SetWeaponHoldType", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, W, "DefaultReload", |lua, r, act: i64| {
        let name = activity_name(lua, act).unwrap_or_else(|| "ACT_VM_RELOAD".into());
        Ok(default_reload(lua, r, &name))
    })?;
    method(lua, W, "CallOnClient", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, W, "SetDeploySpeed", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, W, "AllowsAutoSwitchFrom", |_, _, ()| Ok(true))?;
    method(lua, W, "AllowsAutoSwitchTo", |_, _, ()| Ok(true))?;
    method(lua, W, "GetPrintName", |lua, r, ()| {
        let t = ent_table(lua, r, false)?;
        let n = t.and_then(|t| t.get::<Option<String>>("PrintName").ok().flatten());
        Ok(n.unwrap_or_else(|| with(lua, r, |e| e.class.clone()).unwrap_or_default()))
    })?;
    method(lua, W, "GetSlot", |_, _, ()| Ok(0))?;
    method(lua, W, "GetSlotPos", |_, _, ()| Ok(0))?;
    method(lua, W, "GetWeaponViewModel", |_, _, ()| Ok(""))?;
    method(lua, W, "GetWeaponWorldModel", |lua, r, ()| with(lua, r, |e| e.model.clone()))?;
    method(lua, W, "HasAmmo", |_, _, ()| Ok(true))?;
    method(lua, W, "IsScripted", |lua, r, ()| with(lua, r, |e| e.scripted))?;
    method(lua, W, "LastShootTime", |_, _, ()| Ok(0.0))?;
    method(lua, W, "SetLastShootTime", |_, _, _a: MultiValue| Ok(()))?;
    method(lua, W, "GetActivity", |_, _, ()| Ok(0))?;
    method(lua, W, "SetActivity", |_, _, _a: MultiValue| Ok(()))?;
    Ok(())
}

fn install_phys(lua: &Lua) -> mlua::Result<()> {
    let with_phys = |lua: &Lua, r: EntRef, f: &mut dyn FnMut(&mut EntData)| -> mlua::Result<()> {
        let mut s = store(lua)?;
        if let Some(e) = s.get_mut(r) {
            if e.phys.is_some() {
                f(e);
            }
        }
        Ok(())
    };
    let _ = with_phys;
    fn ph<R>(lua: &Lua, r: EntRef, d: R, f: impl FnOnce(&mut EntData) -> R) -> mlua::Result<R> {
        let mut s = store(lua)?;
        Ok(match s.get_mut(r) {
            Some(e) if e.phys.is_some() => f(e),
            _ => d,
        })
    }
    phys_method(lua, "IsValid", |lua, r, ()| Ok(store(lua)?.get(r).is_some_and(|e| e.phys.is_some())))?;
    phys_method(lua, "GetEntity", |_, r, ()| Ok(r))?;
    phys_method(lua, "GetPos", |lua, r, ()| ph(lua, r, Vector([0.0; 3]), |e| Vector(e.pos)))?;
    phys_method(lua, "SetPos", |lua, r, (v, _t): (Vector, Option<bool>)| ph(lua, r, (), |e| { e.pos = v.0; e.phys.as_mut().unwrap().asleep = false; }))?;
    phys_method(lua, "GetAngles", |lua, r, ()| ph(lua, r, Angle([0.0; 3]), |e| Angle(e.ang)))?;
    // the object's angles after the rotation that takes `from` onto `to`
    phys_method(lua, "AlignAngles", |lua, r, (from, to): (Angle, Angle)| {
        ph(lua, r, Angle([0.0; 3]), |e| {
            let (f, _, u) = angle_vectors(e.ang);
            let turn = |v: [f32; 3]| rotate(unrotate(v, from.0), to.0);
            Angle(angles_from_basis(turn(f), turn(u)))
        })
    })?;
    phys_method(lua, "SetAngles", |lua, r, a: Angle| ph(lua, r, (), |e| e.ang = a.0))?;
    phys_method(lua, "GetVelocity", |lua, r, ()| ph(lua, r, Vector([0.0; 3]), |e| Vector(e.vel)))?;
    phys_method(lua, "SetVelocity", |lua, r, v: Vector| ph(lua, r, (), |e| { e.vel = v.0; e.phys.as_mut().unwrap().asleep = false; }))?;
    phys_method(lua, "SetVelocityInstantaneous", |lua, r, v: Vector| ph(lua, r, (), |e| e.vel = v.0))?;
    phys_method(lua, "AddVelocity", |lua, r, v: Vector| ph(lua, r, (), |e| { e.vel = add(e.vel, v.0); e.phys.as_mut().unwrap().asleep = false; }))?;
    phys_method(lua, "GetAngleVelocity", |lua, r, ()| ph(lua, r, Vector([0.0; 3]), |e| Vector(e.phys.as_ref().unwrap().ang_vel)))?;
    phys_method(lua, "AddAngleVelocity", |lua, r, v: Vector| {
        ph(lua, r, (), |e| {
            let p = e.phys.as_mut().unwrap();
            p.ang_vel = add(p.ang_vel, v.0);
        })
    })?;
    phys_method(lua, "ApplyForceCenter", |lua, r, f: Vector| {
        ph(lua, r, (), |e| {
            let m = e.phys.as_ref().unwrap().mass.max(0.001);
            e.vel = add(e.vel, scale(f.0, 1.0 / m));
            e.phys.as_mut().unwrap().asleep = false;
        })
    })?;
    phys_method(lua, "ApplyForceOffset", |lua, r, (f, _p): (Vector, Option<Vector>)| {
        ph(lua, r, (), |e| {
            let m = e.phys.as_ref().unwrap().mass.max(0.001);
            e.vel = add(e.vel, scale(f.0, 1.0 / m));
        })
    })?;
    phys_method(lua, "ApplyTorqueCenter", |lua, r, t: Vector| {
        ph(lua, r, (), |e| {
            let p = e.phys.as_mut().unwrap();
            let k = 1.0 / (p.mass.max(0.001) * 100.0);
            p.ang_vel = add(p.ang_vel, scale(t.0, k));
            p.asleep = false;
        })
    })?;
    // (linear, angular) velocity change of an impulse at a world position
    phys_method(lua, "CalculateVelocityOffset", |lua, r, (f, pos): (Vector, Vector)| {
        ph(lua, r, (Vector([0.0; 3]), Vector([0.0; 3])), |e| {
            let m = e.phys.as_ref().unwrap().mass.max(0.001);
            let arm = sub(pos.0, e.world_center());
            let torque = [arm[1] * f.0[2] - arm[2] * f.0[1], arm[2] * f.0[0] - arm[0] * f.0[2], arm[0] * f.0[1] - arm[1] * f.0[0]];
            (Vector(scale(f.0, 1.0 / m)), Vector(scale(unrotate(torque, e.ang), 1.0 / (m * 100.0))))
        })
    })?;
    phys_method(lua, "CalculateForceOffset", |lua, r, (f, pos): (Vector, Vector)| {
        ph(lua, r, (Vector([0.0; 3]), Vector([0.0; 3])), |e| {
            let arm = sub(pos.0, e.world_center());
            let torque = [arm[1] * f.0[2] - arm[2] * f.0[1], arm[2] * f.0[0] - arm[0] * f.0[2], arm[0] * f.0[1] - arm[1] * f.0[0]];
            (f, Vector(torque))
        })
    })?;
    phys_method(lua, "GetMass", |lua, r, ()| ph(lua, r, 0.0, |e| e.phys.as_ref().unwrap().mass))?;
    phys_method(lua, "SetMass", |lua, r, m: f32| ph(lua, r, (), |e| e.phys.as_mut().unwrap().mass = m.max(0.001)))?;
    phys_method(lua, "GetInvMass", |lua, r, ()| ph(lua, r, 0.0, |e| 1.0 / e.phys.as_ref().unwrap().mass))?;
    phys_method(lua, "EnableMotion", |lua, r, b: bool| {
        ph(lua, r, (), |e| {
            e.phys.as_mut().unwrap().motion = b;
            e.phys.as_mut().unwrap().asleep = false;
            if !b {
                e.vel = [0.0; 3];
            }
        })
    })?;
    phys_method(lua, "IsMotionEnabled", |lua, r, ()| ph(lua, r, false, |e| e.phys.as_ref().unwrap().motion))?;
    phys_method(lua, "IsMoveable", |lua, r, ()| ph(lua, r, false, |e| e.phys.as_ref().unwrap().motion))?;
    phys_method(lua, "EnableGravity", |lua, r, b: bool| ph(lua, r, (), |e| e.phys.as_mut().unwrap().gravity = b))?;
    phys_method(lua, "IsGravityEnabled", |lua, r, ()| ph(lua, r, false, |e| e.phys.as_ref().unwrap().gravity))?;
    phys_method(lua, "EnableCollisions", |lua, r, b: bool| ph(lua, r, (), |e| e.phys.as_mut().unwrap().collisions = b))?;
    phys_method(lua, "IsCollisionEnabled", |lua, r, ()| ph(lua, r, false, |e| e.phys.as_ref().unwrap().collisions))?;
    phys_method(lua, "EnableDrag", |lua, r, b: bool| ph(lua, r, (), |e| e.phys.as_mut().unwrap().drag = b))?;
    phys_method(lua, "IsDragEnabled", |lua, r, ()| ph(lua, r, false, |e| e.phys.as_ref().unwrap().drag))?;
    phys_method(lua, "Wake", |lua, r, ()| ph(lua, r, (), |e| e.phys.as_mut().unwrap().asleep = false))?;
    phys_method(lua, "Sleep", |lua, r, ()| ph(lua, r, (), |e| e.phys.as_mut().unwrap().asleep = true))?;
    phys_method(lua, "IsAsleep", |lua, r, ()| ph(lua, r, false, |e| e.phys.as_ref().unwrap().asleep))?;
    phys_method(lua, "GetMaterial", |lua, r, ()| ph(lua, r, String::new(), |e| e.phys.as_ref().unwrap().material.clone()))?;
    phys_method(lua, "SetMaterial", |lua, r, m: String| ph(lua, r, (), |e| e.phys.as_mut().unwrap().material = m))?;
    phys_method(lua, "IsPenetrating", |_, _, ()| Ok(false))?;
    phys_method(lua, "GetSurfaceArea", |lua, r, ()| {
        ph(lua, r, 0.0, |e| {
            let d = sub(e.maxs, e.mins);
            2.0 * (d[0] * d[1] + d[1] * d[2] + d[0] * d[2])
        })
    })?;
    phys_method(lua, "GetVolume", |lua, r, ()| {
        ph(lua, r, 0.0, |e| {
            let d = sub(e.maxs, e.mins);
            d[0] * d[1] * d[2]
        })
    })?;
    phys_method(lua, "GetAABB", |lua, r, ()| ph(lua, r, (Vector([0.0; 3]), Vector([0.0; 3])), |e| (Vector(e.mins), Vector(e.maxs))))?;
    phys_method(lua, "LocalToWorld", |lua, r, v: Vector| ph(lua, r, v, |e| Vector(add(e.pos, rotate(v.0, e.ang)))))?;
    phys_method(lua, "WorldToLocal", |lua, r, v: Vector| ph(lua, r, v, |e| Vector(unrotate(sub(v.0, e.pos), e.ang))))?;
    phys_method(lua, "LocalToWorldVector", |lua, r, v: Vector| ph(lua, r, v, |e| Vector(rotate(v.0, e.ang))))?;
    phys_method(lua, "WorldToLocalVector", |lua, r, v: Vector| ph(lua, r, v, |e| Vector(unrotate(v.0, e.ang))))?;
    phys_method(lua, "GetMassCenter", |lua, r, ()| ph(lua, r, Vector([0.0; 3]), |e| Vector(scale(add(e.mins, e.maxs), 0.5))))?;
    phys_method(lua, "SetDamping", |_, _, _a: MultiValue| Ok(()))?;
    phys_method(lua, "GetDamping", |_, _, ()| Ok((0.0, 0.0)))?;
    phys_method(lua, "SetBuoyancyRatio", |_, _, _a: MultiValue| Ok(()))?;
    phys_method(lua, "SetInertia", |_, _, _a: MultiValue| Ok(()))?;
    phys_method(lua, "GetInertia", |_, _, ()| Ok(Vector([1.0; 3])))?;
    phys_method(lua, "SetContents", |_, _, _a: MultiValue| Ok(()))?;
    phys_method(lua, "GetContents", |_, _, ()| Ok(0))?;
    phys_method(lua, "AddGameFlag", |_, _, _a: MultiValue| Ok(()))?;
    phys_method(lua, "ClearGameFlag", |_, _, _a: MultiValue| Ok(()))?;
    phys_method(lua, "HasGameFlag", |_, _, _a: MultiValue| Ok(false))?;
    phys_method(lua, "GetStress", |_, _, ()| Ok(0.0))?;
    phys_method(lua, "GetSpeedDamping", |_, _, ()| Ok(0.0))?;
    phys_method(lua, "RecheckCollisionFilter", |_, _, ()| Ok(()))?;
    phys_method(lua, "OutputDebugInfo", |_, _, ()| Ok(()))?;
    phys_method(lua, "ComputeShadowControl", |_, _, _a: MultiValue| Ok(()))?;
    phys_method(lua, "UpdateShadow", |lua, r, (p, a, _dt): (Vector, Angle, Option<f32>)| {
        ph(lua, r, (), |e| {
            e.pos = p.0;
            e.ang = a.0;
        })
    })?;
    phys_method(lua, "GetMesh", |_, _, ()| Ok(()))?;
    phys_method(lua, "GetMeshConvexes", |_, _, ()| Ok(()))?;
    phys_method(lua, "GetFrictionSnapshot", |lua, _, ()| lua.create_table())?;
    Ok(())
}

/// CRC-32 (IEEE), as GMod's `util.CRC`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    #[test]
    fn crc() {
        assert_eq!(super::crc32(b"123456789"), 0xCBF4_3926);
    }
}
