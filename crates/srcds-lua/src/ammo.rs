//! Ammo types: GMod's built-in list, in the order the engine registers them (their
//! position + 1 is the id the client uses: [факт] reference server.dll, the ammo
//! definition starts counting at 1), plus types Lua adds with `game.AddAmmoType`.

use mlua::{Lua, Table, Value};

pub const AMMO_TYPES: [&str; 37] = [
    "AR2",
    "AR2AltFire",
    "Pistol",
    "SMG1",
    "357",
    "XBowBolt",
    "Buckshot",
    "RPG_Round",
    "SMG1_Grenade",
    "Grenade",
    "slam",
    "AlyxGun",
    "SniperRound",
    "SniperPenetratedRound",
    "Thumper",
    "Gravity",
    "Battery",
    "GaussEnergy",
    "CombineCannon",
    "AirboatGun",
    "StriderMinigun",
    "HelicopterGun",
    "9mmRound",
    "357Round",
    "BuckshotHL1",
    "XBowBoltHL1",
    "MP5_Grenade",
    "RPG_Rocket",
    "Uranium",
    "GrenadeHL1",
    "Hornet",
    "Snark",
    "TripMine",
    "Satchel",
    "12mmRound",
    "StriderMinigunDirect",
    "CombineHeavyCannon",
];

/// Ammo types added by Lua (ids after the built-in ones).
#[derive(Default)]
pub struct CustomAmmo(pub Vec<String>);

/// Id of an ammo type by name (case-insensitive), -1 when unknown.
pub fn ammo_id(lua: &Lua, name: &str) -> i32 {
    if let Some(i) = AMMO_TYPES.iter().position(|a| a.eq_ignore_ascii_case(name)) {
        return i as i32 + 1;
    }
    lua.app_data_ref::<CustomAmmo>()
        .and_then(|c| c.0.iter().position(|a| a.eq_ignore_ascii_case(name)))
        .map_or(-1, |i| (AMMO_TYPES.len() + 1 + i) as i32)
}

/// Name of an ammo id.
pub fn ammo_name(lua: &Lua, id: i64) -> Option<String> {
    if id >= 1 && (id as usize) <= AMMO_TYPES.len() {
        return Some(AMMO_TYPES[id as usize - 1].to_string());
    }
    let i = (id as usize).checked_sub(AMMO_TYPES.len() + 1)?;
    lua.app_data_ref::<CustomAmmo>().and_then(|c| c.0.get(i).cloned())
}

/// The canonical name for an ammo argument (a name in any case, or an id).
pub fn ammo_key(lua: &Lua, v: &Value) -> String {
    match v {
        Value::Integer(i) => ammo_name(lua, *i).unwrap_or_else(|| i.to_string()),
        Value::Number(n) => ammo_name(lua, *n as i64).unwrap_or_else(|| n.to_string()),
        Value::String(s) => {
            let s = s.to_string_lossy().to_string();
            let id = ammo_id(lua, &s);
            if id > 0 {
                ammo_name(lua, id as i64).unwrap_or(s)
            } else {
                s
            }
        }
        _ => String::new(),
    }
}

pub fn install(lua: &Lua) -> mlua::Result<()> {
    lua.set_app_data(CustomAmmo::default());
    let game: Table = match lua.globals().get::<Value>("game")? {
        Value::Table(t) => t,
        _ => {
            let t = lua.create_table()?;
            lua.globals().set("game", t.clone())?;
            t
        }
    };
    game.raw_set("GetAmmoID", lua.create_function(|lua, name: String| Ok(ammo_id(lua, &name)))?)?;
    game.raw_set("GetAmmoName", lua.create_function(|lua, id: i64| Ok(ammo_name(lua, id)))?)?;
    game.raw_set(
        "AddAmmoType",
        lua.create_function(|lua, t: Table| {
            let name: String = t.get("name").unwrap_or_default();
            if !name.is_empty() && ammo_id(lua, &name) < 0 {
                if let Some(mut c) = lua.app_data_mut::<CustomAmmo>() {
                    c.0.push(name);
                }
            }
            Ok(())
        })?,
    )?;
    game.raw_set("GetAmmoMax", lua.create_function(|_, _id: Value| Ok(9999))?)?;
    game.raw_set(
        "GetAmmoTypes",
        lua.create_function(|lua, ()| {
            let t = lua.create_table()?;
            for (i, n) in AMMO_TYPES.iter().enumerate() {
                t.raw_set(i + 1, *n)?;
            }
            if let Some(c) = lua.app_data_ref::<CustomAmmo>() {
                for (i, n) in c.0.iter().enumerate() {
                    t.raw_set(AMMO_TYPES.len() + 1 + i, n.as_str())?;
                }
            }
            Ok(t)
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_start_at_one() {
        let lua = Lua::new();
        install(&lua).unwrap();
        assert_eq!(ammo_id(&lua, "AR2"), 1);
        assert_eq!(ammo_id(&lua, "pistol"), 3);
        assert_eq!(ammo_id(&lua, "Buckshot"), 7);
        assert_eq!(ammo_name(&lua, 4).as_deref(), Some("SMG1"));
        assert_eq!(ammo_key(&lua, &Value::Integer(3)), "Pistol");
        assert_eq!(ammo_id(&lua, "nope"), -1);
    }
}
