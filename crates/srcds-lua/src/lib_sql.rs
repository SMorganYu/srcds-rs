//! The `sql` library over SQLite (GMod's server database, `sv.db`).
//!
//! Behaviour follows GMod: `sql.Query` returns `false` on error (the message goes to
//! `sql.m_strError`, read by `sql.LastError`), `nil` when the statement yields no rows,
//! otherwise a sequence of rows keyed by column name with values as strings
//! (`sql.QueryTyped` keeps numbers as numbers). The database file is srcds-rs' own
//! (not the game installation's).

use std::path::Path;

use mlua::{Lua, Table, Value};
use rusqlite::types::ValueRef;

pub struct Db(pub rusqlite::Connection);

fn run(lua: &Lua, q: &str, typed: bool) -> mlua::Result<Value> {
    let db = lua
        .app_data_ref::<Db>()
        .ok_or_else(|| mlua::Error::RuntimeError("sql unavailable".into()))?;
    let mut rows_out: Vec<Vec<(String, Value)>> = Vec::new();
    let mut err: Option<String> = None;
    // execute every statement of the query; rows come from the last one with columns
    let mut rest = q.trim();
    while !rest.is_empty() {
        let stmt = db.0.prepare(rest);
        let mut stmt = match stmt {
            Ok(s) => s,
            Err(e) => {
                err = Some(e.to_string());
                break;
            }
        };
        let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let mut rows = stmt.raw_query();
        let mut got = Vec::new();
        loop {
            match rows.next() {
                Ok(Some(row)) => {
                    let mut out = Vec::new();
                    for (i, n) in names.iter().enumerate() {
                        let v = match row.get_ref(i) {
                            Ok(ValueRef::Null) => Value::Nil,
                            Ok(ValueRef::Integer(x)) => {
                                if typed {
                                    Value::Integer(x)
                                } else {
                                    Value::String(lua.create_string(x.to_string())?)
                                }
                            }
                            Ok(ValueRef::Real(x)) => {
                                if typed {
                                    Value::Number(x)
                                } else {
                                    Value::String(lua.create_string(x.to_string())?)
                                }
                            }
                            Ok(ValueRef::Text(t)) | Ok(ValueRef::Blob(t)) => Value::String(lua.create_string(t)?),
                            Err(_) => Value::Nil,
                        };
                        out.push((n.clone(), v));
                    }
                    got.push(out);
                }
                Ok(None) => break,
                Err(e) => {
                    err = Some(e.to_string());
                    break;
                }
            }
        }
        drop(rows);
        if err.is_some() {
            break;
        }
        if !names.is_empty() {
            rows_out = got;
        }
        // advance past this statement (split on ';' outside quotes)
        match split_first(rest) {
            Some(next) => rest = next.trim(),
            None => break,
        }
    }
    let sqlt: Table = lua.globals().get("sql")?;
    if let Some(e) = err {
        sqlt.raw_set("m_strError", e)?;
        return Ok(Value::Boolean(false));
    }
    sqlt.raw_set("m_strError", Value::Nil)?;
    if rows_out.is_empty() {
        return Ok(Value::Nil);
    }
    let t = lua.create_table()?;
    for (i, row) in rows_out.into_iter().enumerate() {
        let r = lua.create_table()?;
        for (k, v) in row {
            r.raw_set(k, v)?;
        }
        t.raw_set(i + 1, r)?;
    }
    Ok(Value::Table(t))
}

/// Remainder after the first statement (`;` outside string literals), if any.
fn split_first(s: &str) -> Option<&str> {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, ';') => {
                let rest = &s[i + 1..];
                return (!rest.trim().is_empty()).then_some(rest);
            }
            _ => {}
        }
    }
    None
}

/// Opens `path` (created if missing) and installs `sql.Query` / `sql.QueryTyped`.
pub fn install(lua: &Lua, path: &Path) -> mlua::Result<()> {
    let conn = rusqlite::Connection::open(path)
        .or_else(|_| rusqlite::Connection::open_in_memory())
        .map_err(|e| mlua::Error::RuntimeError(format!("sql: {e}")))?;
    lua.set_app_data(Db(conn));
    let sql: Table = match lua.globals().raw_get::<Value>("sql")? {
        Value::Table(t) => t,
        _ => {
            let t = lua.create_table()?;
            lua.globals().raw_set("sql", &t)?;
            t
        }
    };
    sql.raw_set("Query", lua.create_function(|lua, q: String| run(lua, &q, false))?)?;
    sql.raw_set("QueryTyped", lua.create_function(|lua, q: String| run(lua, &q, true))?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn query_semantics() {
        let g = crate::GLua::new(std::path::Path::new(".")).unwrap();
        super::install(&g.lua, std::path::Path::new(":memory:")).unwrap();
        let ok: bool = g
            .lua
            .load(
                "assert(sql.Query('CREATE TABLE t (k TEXT, v INTEGER); INSERT INTO t VALUES (\"a;b\", 5);') == nil)
                 local r = sql.Query('SELECT * FROM t')
                 assert(r[1].k == 'a;b' and r[1].v == '5', tostring(r[1].v))
                 assert(sql.QueryTyped('SELECT v FROM t')[1].v == 5)
                 assert(sql.Query('SELECT * FROM t WHERE k = \"x\"') == nil)
                 assert(sql.Query('SELEC nonsense') == false and sql.m_strError ~= nil)
                 return true",
            )
            .eval()
            .unwrap();
        assert!(ok);
    }
}
