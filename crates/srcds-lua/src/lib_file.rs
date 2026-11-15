//! `file` library over the game directory with GMod path IDs.
//!
//! * `LUA` / `lsv` / `LuaMenu`: the Lua search roots (`lua/`, `gamemodes/`, and the
//!   `entities/` folders of the loaded gamemode chain);
//! * `GAME` / `MOD` / `BASE_PATH`: the `garrysmod` directory;
//! * `DATA`: `garrysmod/data` — the only place scripts may write.
//!
//! Paths are confined to their root (`..`, absolute paths and drive letters are refused).

use std::path::{Path, PathBuf};

use mlua::{Lua, Table, Value};

use crate::runtime::Shared;

fn safe_rel(p: &str) -> Option<String> {
    let p = p.replace('\\', "/");
    let p = p.trim_start_matches('/').to_string();
    if p.split('/').any(|c| c == "..") || p.contains(':') {
        return None;
    }
    Some(p)
}

/// Root directories for a path ID.
pub fn roots(st: &Shared, path_id: &str) -> Vec<PathBuf> {
    let s = st.borrow();
    let g = s.game_dir.clone();
    match path_id.to_ascii_uppercase().as_str() {
        "LUA" | "LSV" | "LUAMENU" | "LCL" => {
            let mut v = vec![g.join("lua"), g.join("gamemodes")];
            for gm in &s.gamemode_chain {
                v.push(g.join("gamemodes").join(gm).join("entities"));
            }
            v
        }
        "DATA" => vec![g.join("data")],
        _ => vec![g.clone()],
    }
}

fn wildcard(pattern: &str, name: &str) -> bool {
    // '*' only (what GMod scripts use); case-insensitive like the Windows FS
    let (p, n) = (pattern.to_ascii_lowercase(), name.to_ascii_lowercase());
    let parts: Vec<&str> = p.split('*').collect();
    if parts.len() == 1 {
        return p == n;
    }
    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !n.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if i == parts.len() - 1 {
            return n.len() >= pos + part.len() && n.ends_with(part);
        } else {
            match n[pos..].find(part) {
                Some(f) => pos += f + part.len(),
                None => return false,
            }
        }
    }
    true
}

fn find(st: &Shared, pattern: &str, path_id: &str) -> (Vec<String>, Vec<String>) {
    let Some(pat) = safe_rel(pattern) else {
        return (Vec::new(), Vec::new());
    };
    let (dir, mask) = match pat.rsplit_once('/') {
        Some((d, m)) => (d.to_string(), m.to_string()),
        None => (String::new(), pat.clone()),
    };
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for root in roots(st, path_id) {
        let Ok(rd) = std::fs::read_dir(root.join(&dir)) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !wildcard(&mask, &name) {
                continue;
            }
            let list = if e.path().is_dir() { &mut dirs } else { &mut files };
            if !list.contains(&name) {
                list.push(name);
            }
        }
    }
    files.sort();
    dirs.sort();
    (files, dirs)
}

fn locate(st: &Shared, rel: &str, path_id: &str) -> Option<PathBuf> {
    let rel = safe_rel(rel)?;
    roots(st, path_id)
        .into_iter()
        .map(|r| r.join(&rel))
        .find(|p| p.exists())
}

fn data_path(st: &Shared, rel: &str) -> Option<PathBuf> {
    let rel = safe_rel(rel)?.to_ascii_lowercase();
    Some(st.borrow().game_dir.join("data").join(rel))
}

pub fn install(lua: &Lua, st: &Shared) -> mlua::Result<()> {
    let file: Table = match lua.globals().raw_get::<Value>("file")? {
        Value::Table(t) => t,
        _ => {
            let t = lua.create_table()?;
            lua.globals().raw_set("file", &t)?;
            t
        }
    };
    let s = st.clone();
    file.raw_set(
        "Find",
        lua.create_function(move |lua, (pattern, path, _sort): (String, Option<String>, Option<String>)| {
            let (f, d) = find(&s, &pattern, path.as_deref().unwrap_or("GAME"));
            Ok((lua.create_sequence_from(f)?, lua.create_sequence_from(d)?))
        })?,
    )?;
    let s = st.clone();
    file.raw_set(
        "Exists",
        lua.create_function(move |_, (p, path): (String, Option<String>)| {
            Ok(locate(&s, &p, path.as_deref().unwrap_or("GAME")).is_some())
        })?,
    )?;
    let s = st.clone();
    file.raw_set(
        "IsDir",
        lua.create_function(move |_, (p, path): (String, Option<String>)| {
            Ok(locate(&s, &p, path.as_deref().unwrap_or("GAME")).is_some_and(|p| p.is_dir()))
        })?,
    )?;
    let s = st.clone();
    file.raw_set(
        "Read",
        lua.create_function(move |lua, (p, path): (String, Option<String>)| {
            let path = path.unwrap_or_else(|| "DATA".into());
            match locate(&s, &p, &path).filter(|p| p.is_file()) {
                Some(f) => match std::fs::read(f) {
                    Ok(b) => Ok(Value::String(lua.create_string(&b)?)),
                    Err(_) => Ok(Value::Nil),
                },
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    let s = st.clone();
    file.raw_set(
        "Size",
        lua.create_function(move |_, (p, path): (String, Option<String>)| {
            Ok(locate(&s, &p, path.as_deref().unwrap_or("GAME"))
                .and_then(|f| f.metadata().ok())
                .map_or(-1, |m| m.len() as i64))
        })?,
    )?;
    let s = st.clone();
    file.raw_set(
        "Time",
        lua.create_function(move |_, (p, path): (String, Option<String>)| {
            Ok(locate(&s, &p, path.as_deref().unwrap_or("GAME"))
                .and_then(|f| f.metadata().ok())
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64))
        })?,
    )?;
    let s = st.clone();
    file.raw_set(
        "Write",
        lua.create_function(move |_, (p, data): (String, mlua::String)| {
            if let Some(f) = data_path(&s, &p) {
                if let Some(dir) = f.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(f, data.as_bytes());
            }
            Ok(())
        })?,
    )?;
    let s = st.clone();
    file.raw_set(
        "Append",
        lua.create_function(move |_, (p, data): (String, mlua::String)| {
            if let Some(f) = data_path(&s, &p) {
                use std::io::Write;
                if let Ok(mut fh) = std::fs::OpenOptions::new().create(true).append(true).open(f) {
                    let _ = fh.write_all(&data.as_bytes());
                }
            }
            Ok(())
        })?,
    )?;
    let s = st.clone();
    file.raw_set(
        "CreateDir",
        lua.create_function(move |_, p: String| {
            if let Some(f) = data_path(&s, &p) {
                let _ = std::fs::create_dir_all(f);
            }
            Ok(())
        })?,
    )?;
    let s = st.clone();
    file.raw_set(
        "Delete",
        lua.create_function(move |_, p: String| {
            if let Some(f) = data_path(&s, &p) {
                if f.is_file() {
                    let _ = std::fs::remove_file(f);
                } else if f.is_dir() {
                    let _ = std::fs::remove_dir(f);
                }
            }
            Ok(())
        })?,
    )?;
    Ok(())
}

#[allow(dead_code)]
fn _p(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::wildcard;

    #[test]
    fn wildcards() {
        assert!(wildcard("*.lua", "a.lua"));
        assert!(wildcard("*.LUA", "init.lua"));
        assert!(!wildcard("*.lua", "a.txt"));
        assert!(wildcard("*", "x"));
        assert!(wildcard("cl_*.lua", "cl_init.lua"));
        assert!(!wildcard("cl_*.lua", "sv_init.lua"));
        assert!(wildcard("init.lua", "INIT.lua"));
    }
}
