//! Engine metatables (`FindMetaTable`) and the API specification: numeric constants,
//! library tables, and counting stubs for engine functions not implemented yet.

use mlua::{Function, Lua, MultiValue, Table, Value};

use crate::runtime::Shared;

/// The recorded server API surface (see the file header).
pub const SPEC: &str = include_str!("../data/gmod_api.tsv");

fn key(name: &str) -> String {
    format!("srcds_meta_{name}")
}

/// The metatable `name`, created on first use.
pub fn get(lua: &Lua, name: &str) -> mlua::Result<Table> {
    if let Ok(t) = lua.named_registry_value::<Table>(&key(name)) {
        return Ok(t);
    }
    let t = lua.create_table()?;
    t.raw_set("MetaName", name)?;
    t.raw_set("__index", &t)?;
    lua.set_named_registry_value(&key(name), &t)?;
    Ok(t)
}

/// Creates the metatables of the spec, wires `MetaBaseClass` and `FindMetaTable`.
pub fn install_metatables(lua: &Lua) -> mlua::Result<()> {
    for line in SPEC.lines() {
        let p: Vec<&str> = line.split('\t').collect();
        if p.first() == Some(&"meta") && p.len() >= 3 {
            let t = get(lua, p[1])?;
            t.raw_set("MetaID", p[2].parse::<i64>().unwrap_or(0))?;
            if let Some(base) = p.get(3) {
                t.raw_set("MetaBaseClass", get(lua, base)?)?;
            }
        }
    }
    let g = lua.globals();
    g.set(
        "FindMetaTable",
        lua.create_function(|lua, name: String| {
            Ok(lua.named_registry_value::<Table>(&key(&name)).ok())
        })?,
    )?;
    g.set(
        "RegisterMetaTable",
        lua.create_function(|lua, (name, t): (String, Table)| {
            t.raw_set("MetaName", name.as_str())?;
            lua.set_named_registry_value(&key(&name), t)
        })?,
    )?;
    // type() stays the raw Lua one: includes/util.lua wraps it and reads MetaName
    // through getmetatable (see lib_debug)
    Ok(())
}

fn num(s: &str) -> Value {
    match s.parse::<i64>() {
        Ok(i) => Value::Integer(i),
        Err(_) => Value::Number(s.parse::<f64>().unwrap_or(0.0)),
    }
}

fn stub(lua: &Lua, st: &Shared, name: String) -> mlua::Result<Function> {
    let st = st.clone();
    lua.create_function(move |_, _: MultiValue| {
        *st.borrow_mut().missing.entry(name.clone()).or_default() += 1;
        Ok(())
    })
}

/// Constants, library tables, and stubs for every engine function the spec lists that
/// nothing has defined yet. Call after the real implementations are installed.
pub fn install_spec(lua: &Lua, st: &Shared) -> mlua::Result<()> {
    let g = lua.globals();
    let lib = |name: &str| -> mlua::Result<Table> {
        match g.raw_get::<Value>(name)? {
            Value::Table(t) => Ok(t),
            _ => {
                let t = lua.create_table()?;
                g.raw_set(name, &t)?;
                Ok(t)
            }
        }
    };
    let mut stubbed = 0usize;
    for line in SPEC.lines() {
        let p: Vec<&str> = line.split('\t').collect();
        match p.as_slice() {
            ["const", name, v] => {
                if g.raw_get::<Value>(*name)?.is_nil() {
                    g.raw_set(*name, num(v))?;
                }
            }
            ["lib", name] => {
                lib(name)?;
            }
            ["libconst", l, name, v] => {
                let t = lib(l)?;
                if t.raw_get::<Value>(*name)?.is_nil() {
                    t.raw_set(*name, num(v))?;
                }
            }
            ["cfunc", name] => {
                if g.raw_get::<Value>(*name)?.is_nil() {
                    g.raw_set(*name, stub(lua, st, name.to_string())?)?;
                    stubbed += 1;
                }
            }
            ["libfunc", l, name] => {
                let t = lib(l)?;
                if t.raw_get::<Value>(*name)?.is_nil() {
                    t.raw_set(*name, stub(lua, st, format!("{l}.{name}"))?)?;
                    stubbed += 1;
                }
            }
            ["method", m, name] => {
                let t = get(lua, m)?;
                if t.raw_get::<Value>(*name)?.is_nil() && inherited(&t, name)?.is_nil() {
                    t.raw_set(*name, stub(lua, st, format!("{m}:{name}"))?)?;
                    stubbed += 1;
                }
            }
            _ => {}
        }
    }
    st.borrow_mut().stubbed = stubbed;
    Ok(())
}

/// `name` from the `MetaBaseClass` chain of `t` (not `t` itself).
fn inherited(t: &Table, name: &str) -> mlua::Result<Value> {
    let mut cur = t.clone();
    for _ in 0..8 {
        match cur.raw_get::<Value>("MetaBaseClass")? {
            Value::Table(b) => {
                let v: Value = b.raw_get(name)?;
                if !v.is_nil() {
                    return Ok(v);
                }
                cur = b;
            }
            _ => break,
        }
    }
    Ok(Value::Nil)
}

/// Counts of the spec: (implemented, stubbed) engine functions.
pub fn coverage(lua: &Lua, st: &Shared) -> (usize, usize) {
    let _ = lua;
    let total = SPEC
        .lines()
        .filter(|l| l.starts_with("cfunc\t") || l.starts_with("libfunc\t") || l.starts_with("method\t"))
        .count();
    let missing = st.borrow().stubbed;
    (total - missing, missing)
}

#[allow(dead_code)]
fn _unused(_: MultiValue) {}

#[cfg(test)]
mod tests {
    #[test]
    fn spec_parses() {
        let n = super::SPEC.lines().filter(|l| l.starts_with("method\t")).count();
        assert!(n > 1000, "{n}");
    }
}
