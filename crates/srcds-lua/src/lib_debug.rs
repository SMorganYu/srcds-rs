//! The part of the `debug` library GMod scripts use, implemented over mlua's safe
//! introspection API (the raw C `debug` library is not loaded).

use mlua::{Function, Lua, Table, Value};

fn info_table(lua: &Lua, src: &str, short: &str, what: &str, line_defined: i64, last: i64) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("source", src)?;
    t.set("short_src", short)?;
    t.set("what", what)?;
    t.set("linedefined", line_defined)?;
    t.set("lastlinedefined", last)?;
    Ok(t)
}

fn getinfo(lua: &Lua, target: Value) -> mlua::Result<Value> {
    match target {
        Value::Function(f) => {
            let i = f.info();
            let t = info_table(
                lua,
                i.source.as_deref().unwrap_or("=[C]"),
                i.short_src.as_deref().unwrap_or("[C]"),
                i.what,
                i.line_defined.map_or(-1, |l| l as i64),
                i.last_line_defined.map_or(-1, |l| l as i64),
            )?;
            t.set("currentline", -1)?;
            t.set("func", f)?;
            Ok(Value::Table(t))
        }
        Value::Integer(_) | Value::Number(_) => {
            let level = match target {
                Value::Integer(i) => i,
                Value::Number(n) => n as i64,
                _ => 0,
            };
            if level < 0 {
                return Ok(Value::Nil);
            }
            // level 0 is this function itself, like the C library
            let Some(d) = lua.inspect_stack(level as usize) else {
                return Ok(Value::Nil);
            };
            let s = d.source();
            let t = info_table(
                lua,
                s.source.as_deref().unwrap_or("=[C]"),
                s.short_src.as_deref().unwrap_or("[C]"),
                s.what,
                s.line_defined.map_or(-1, |l| l as i64),
                s.last_line_defined.map_or(-1, |l| l as i64),
            )?;
            t.set("currentline", d.curr_line())?;
            let n = d.names();
            if let Some(name) = n.name {
                t.set("name", name.as_ref())?;
            }
            t.set("namewhat", n.name_what.as_deref().unwrap_or(""))?;
            Ok(Value::Table(t))
        }
        _ => Ok(Value::Nil),
    }
}

pub fn install(lua: &Lua) -> mlua::Result<()> {
    let g = lua.globals();
    let debug: Table = match g.raw_get::<Value>("debug")? {
        Value::Table(t) => t,
        _ => {
            let t = lua.create_table()?;
            g.raw_set("debug", &t)?;
            t
        }
    };
    debug.raw_set(
        "getinfo",
        lua.create_function(|lua, (target, _what): (Value, Option<String>)| getinfo(lua, target))?,
    )?;
    let getmetatable: Function = g.get("getmetatable")?;
    // engine userdata report their GMod metatable (MetaName/MetaID/MetaBaseClass),
    // which isentity()/TypeID() read
    let orig = getmetatable.clone();
    g.raw_set(
        "getmetatable",
        lua.create_function(move |lua, v: Value| {
            if let Value::UserData(ud) = &v {
                let name = if ud.is::<crate::vecang::Vector>() {
                    Some("Vector")
                } else if ud.is::<crate::vecang::Angle>() {
                    Some("Angle")
                } else {
                    crate::entity::type_name(ud)
                };
                if let Some(n) = name {
                    return Ok(Value::Table(crate::meta::get(lua, n)?));
                }
            }
            orig.call::<Value>(v)
        })?,
    )?;
    debug.raw_set(
        "getmetatable",
        lua.create_function(move |lua, v: Value| match &v {
            // raw: ignores __metatable
            Value::Table(t) => Ok(t.metatable().map_or(Value::Nil, Value::Table)),
            Value::UserData(ud) => {
                let name = if ud.is::<crate::vecang::Vector>() {
                    Some("Vector")
                } else if ud.is::<crate::vecang::Angle>() {
                    Some("Angle")
                } else {
                    crate::entity::type_name(ud)
                };
                match name {
                    Some(n) => Ok(Value::Table(crate::meta::get(lua, n)?)),
                    None => getmetatable.call(v),
                }
            }
            _ => getmetatable.call(v),
        })?,
    )?;
    debug.raw_set(
        "setmetatable",
        lua.create_function(|_, (v, mt): (Value, Option<Table>)| {
            if let Value::Table(t) = &v {
                t.set_metatable(mt);
            }
            Ok(v)
        })?,
    )?;
    debug.raw_set(
        "traceback",
        lua.create_function(|lua, args: mlua::MultiValue| {
            let msg = match args.front() {
                Some(Value::String(s)) => format!("{}\n", s.to_string_lossy()),
                Some(Value::Nil) | None => String::new(),
                Some(other) => return Ok(other.clone()),
            };
            let mut out = format!("{msg}stack traceback:");
            let mut level = 1;
            while let Some(d) = lua.inspect_stack(level) {
                let s = d.source();
                let name = d.names().name.map(|n| n.to_string());
                out.push_str(&format!(
                    "\n\t{}:{}: in {}",
                    s.short_src.as_deref().unwrap_or("[C]"),
                    d.curr_line(),
                    name.map_or("main chunk".to_string(), |n| format!("function '{n}'"))
                ));
                level += 1;
                if level > 64 {
                    break;
                }
            }
            Ok(Value::String(lua.create_string(out)?))
        })?,
    )?;
    if let Ok(f) = g.get::<Function>("getfenv") {
        debug.raw_set("getfenv", f)?;
    }
    if let Ok(f) = g.get::<Function>("setfenv") {
        debug.raw_set("setfenv", f)?;
    }
    debug.raw_set("sethook", lua.create_function(|_, _: mlua::MultiValue| Ok(()))?)?;
    debug.raw_set("gethook", lua.create_function(|_, ()| Ok(()))?)?;
    debug.raw_set("getlocal", lua.create_function(|_, _: mlua::MultiValue| Ok(()))?)?;
    debug.raw_set("getupvalue", lua.create_function(|_, _: mlua::MultiValue| Ok(()))?)?;
    debug.raw_set("debug", lua.create_function(|_, ()| Ok(()))?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn getinfo_and_metatables() {
        let g = crate::GLua::new(std::path::Path::new(".")).unwrap();
        let (line, mt, tb): (i64, bool, String) = g
            .lua
            .load(
                "local function f() return debug.getinfo(1, 'Sln').currentline end
                 local t = setmetatable({}, {__metatable = false, x = 1})
                 return f(), debug.getmetatable(t).x == 1, debug.traceback('m')",
            )
            .set_name("@t.lua")
            .eval()
            .unwrap();
        assert_eq!(line, 1);
        assert!(mt);
        assert!(tb.starts_with("m\nstack traceback:"), "{tb}");
        let v: bool = g
            .lua
            .load("return debug.getmetatable(Vector(1,2,3)) == FindMetaTable('Vector')")
            .eval()
            .unwrap();
        assert!(v);
    }
}
