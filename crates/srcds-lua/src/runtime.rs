//! The Lua state: file loading with GMod's search rules, `include` / `require` /
//! `AddCSLuaFile`, output, and stubs for engine functions that are not implemented
//! yet (each records how often it was hit, which drives what to implement next).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{Function, Lua, MultiValue, Table, Value};
use tracing::{info, warn};

use crate::syntax;

/// Shared runtime state reachable from Lua callbacks.
#[derive(Debug, Default)]
pub struct State {
    /// The `garrysmod` directory.
    pub game_dir: PathBuf,
    /// Files sent to clients via `AddCSLuaFile` (relative `lua/…` / `gamemodes/…`).
    pub client_files: Vec<String>,
    /// Directory of each file being executed (for relative includes).
    pub include_stack: Vec<String>,
    /// Engine functions called that are not implemented yet → call count.
    pub missing: BTreeMap<String, u64>,
    /// Lua errors reported so far.
    pub errors: u64,
    /// Gamemode and its bases, most derived first (e.g. sandbox, base).
    pub gamemode_chain: Vec<String>,
    /// Engine functions currently backed by stubs.
    pub stubbed: usize,
    /// The first error messages (for diagnostics).
    pub error_log: Vec<String>,
}

pub type Shared = Rc<RefCell<State>>;

/// A GLua server state.
pub struct GLua {
    pub lua: Lua,
    pub state: Shared,
}

impl GLua {
    pub fn new(game_dir: &Path) -> mlua::Result<Self> {
        // host-access parts of the standard library are removed, as in GMod; the
        // `debug` subset GMod scripts use is provided by lib_debug (safe Rust)
        let lua = Lua::new();
        {
            let g = lua.globals();
            for name in ["io", "dofile", "loadfile"] {
                g.raw_set(name, Value::Nil)?;
            }
            if let Ok(os) = g.get::<Table>("os") {
                for name in ["execute", "remove", "rename", "exit", "getenv", "tmpname", "setlocale"] {
                    os.raw_set(name, Value::Nil)?;
                }
            }
            if let Ok(pkg) = g.get::<Table>("package") {
                pkg.raw_set("loadlib", Value::Nil)?;
                pkg.raw_set("cpath", "")?;
            }
        }
        let state: Shared = Rc::new(RefCell::new(State {
            game_dir: game_dir.to_path_buf(),
            ..Default::default()
        }));
        let g = Self { lua, state };
        g.install_core()?;
        crate::meta::install_metatables(&g.lua)?;
        crate::vecang::install(&g.lua)?;
        crate::lib_file::install(&g.lua, &g.state)?;
        crate::lib_misc::install(&g.lua, &g.state)?;
        crate::ammo::install(&g.lua)?;
        crate::lib_debug::install(&g.lua)?;
        crate::entity::install(&g.lua)?;
        crate::lib_ents::install(&g.lua)?;
        crate::lib_net::install(&g.lua)?;
        // in-memory until the host opens its database file
        crate::lib_sql::install(&g.lua, std::path::Path::new(":memory:"))?;
        crate::meta::install_spec(&g.lua, &g.state)?;
        Ok(g)
    }

    fn install_core(&self) -> mlua::Result<()> {
        let lua = &self.lua;
        let g = lua.globals();
        g.set("SERVER", true)?;
        g.set("CLIENT", false)?;
        g.set("MENU_DLL", false)?;
        g.set("VERSION", 250_429)?;
        g.set("VERSIONSTR", "2026.04.29")?;
        g.set("BRANCH", "unknown")?;
        g.set("NULL", Value::Nil)?;
        // GMod's table.insert returns the index the value was inserted at
        lua.load(
            "local ins = table.insert
             table.insert = function(t, ...)
                 local n = select('#', ...)
                 if n == 1 then ins(t, (...)) return #t end
                 if n == 2 then local pos, v = ... ins(t, pos, v) return pos end
                 return ins(t, ...)
             end",
        )
        .set_name("=srcds_prelude")
        .exec()?;

        // output
        let print_like = |sep: &'static str, newline: bool| {
            move |lua: &Lua, args: MultiValue| -> mlua::Result<()> {
                let tostring: Function = lua.globals().get("tostring")?;
                let mut parts = Vec::new();
                for a in args {
                    parts.push(tostring.call::<String>(a)?);
                }
                let line = parts.join(sep);
                let line = if newline { line } else { line.trim_end_matches('\n').to_string() };
                info!(target: "lua", "{line}");
                Ok(())
            }
        };
        g.set("print", lua.create_function(print_like("\t", true))?)?;
        g.set("Msg", lua.create_function(print_like("", false))?)?;
        g.set("MsgN", lua.create_function(print_like("", true))?)?;
        g.set("MsgAll", lua.create_function(print_like("", false))?)?;
        g.set(
            "MsgC",
            lua.create_function(|lua, args: MultiValue| {
                // colour tables are skipped
                let tostring: Function = lua.globals().get("tostring")?;
                let mut s = String::new();
                for a in args {
                    if matches!(a, Value::Table(_)) {
                        continue;
                    }
                    s.push_str(&tostring.call::<String>(a)?);
                }
                info!(target: "lua", "{}", s.trim_end_matches('\n'));
                Ok(())
            })?,
        )?;
        let state = self.state.clone();
        g.set(
            "ErrorNoHalt",
            lua.create_function(move |_, args: MultiValue| {
                state.borrow_mut().errors += 1;
                let s: Vec<String> = args.iter().map(|v| format!("{v:?}")).collect();
                warn!(target: "lua", "[ERROR] {}", s.join(" "));
                Ok(())
            })?,
        )?;
        g.set("ErrorNoHaltWithStack", g.get::<Function>("ErrorNoHalt")?)?;

        // file loading
        let st = self.state.clone();
        g.set(
            "include",
            lua.create_function(move |lua, path: String| include(lua, &st, &path))?,
        )?;
        let st = self.state.clone();
        g.set(
            "AddCSLuaFile",
            lua.create_function(move |_, path: Option<String>| {
                let mut s = st.borrow_mut();
                let resolved = match path {
                    Some(p) => resolve(&s, &p).map(|(rel, _)| rel),
                    // no argument: the current file
                    None => s.include_stack.last().cloned(),
                };
                if let Some(r) = resolved {
                    if !s.client_files.contains(&r) {
                        s.client_files.push(r);
                    }
                }
                Ok(())
            })?,
        )?;
        let st = self.state.clone();
        g.set(
            "CompileFile",
            lua.create_function(move |lua, path: String| {
                let s = st.borrow();
                match resolve(&s, &path) {
                    Some((rel, full)) => {
                        let src = read_lossy(&full);
                        drop(s);
                        Ok(Some(compile(lua, &src, &rel)?))
                    }
                    None => Ok(None),
                }
            })?,
        )?;
        g.set(
            "CompileString",
            lua.create_function(|lua, (code, name): (String, Option<String>)| {
                compile(lua, &code, name.as_deref().unwrap_or("CompileString"))
            })?,
        )?;
        g.set(
            "RunString",
            lua.create_function(|lua, (code, name): (String, Option<String>)| {
                let f = compile(lua, &code, name.as_deref().unwrap_or("RunString"))?;
                f.call::<()>(())
            })?,
        )?;

        // GMod's `require` loads includes/modules/<name>.lua once
        let st = self.state.clone();
        g.set(
            "require",
            lua.create_function(move |lua, name: String| {
                let loaded: Table = lua.named_registry_value("srcds_required").or_else(|_| {
                    let t = lua.create_table()?;
                    lua.set_named_registry_value("srcds_required", &t)?;
                    Ok::<_, mlua::Error>(t)
                })?;
                if loaded.contains_key(name.as_str())? {
                    return Ok(());
                }
                loaded.set(name.as_str(), true)?;
                let path = format!("includes/modules/{name}.lua");
                if resolve(&st.borrow(), &path).is_none() {
                    // binary modules (gmsv_*) are not supported
                    warn!(target: "lua", "require: module '{name}' not found");
                    return Ok(());
                }
                include(lua, &st, &path).map(|_| ())
            })?,
        )?;

        Ok(())
    }

    /// Runs a file (path relative to the Lua search roots); errors are reported, not
    /// propagated, as GMod does.
    pub fn run_file(&self, path: &str) -> bool {
        match include(&self.lua, &self.state, path) {
            Ok(_) => true,
            Err(e) => {
                report(&self.state, &e);
                false
            }
        }
    }

    /// Calls a global hook-style function path such as `hook.Call`.
    pub fn missing_report(&self, top: usize) -> Vec<(String, u64)> {
        let mut v: Vec<_> = self
            .state
            .borrow()
            .missing
            .iter()
            .map(|(k, n)| (k.clone(), *n))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v.truncate(top);
        v
    }
}

pub fn report(st: &Shared, e: &mlua::Error) {
    let msg = e.to_string();
    {
        let mut s = st.borrow_mut();
        s.errors += 1;
        if s.error_log.len() < 500 {
            s.error_log.push(msg.lines().take(3).collect::<Vec<_>>().join(" | "));
        }
    }
    warn!(target: "lua", "[ERROR] {}", msg.lines().next().unwrap_or(""));
}

fn read_lossy(p: &Path) -> String {
    std::fs::read(p)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// Compiles GMod-dialect source as a chunk named like GMod does (`@path`).
pub fn compile(lua: &Lua, src: &str, name: &str) -> mlua::Result<Function> {
    let code = syntax::to_standard_lua(src.trim_start_matches('\u{feff}'));
    lua.load(&code).set_name(format!("@{name}")).into_function()
}

/// Finds a Lua file: relative to the current file's directory, then under `lua/`,
/// then `gamemodes/`. Returns the path relative to the game directory and the full
/// path.
pub fn resolve(s: &State, path: &str) -> Option<(String, PathBuf)> {
    let p = path.replace('\\', "/");
    if p.split('/').any(|c| c == "..") {
        return None;
    }
    let mut candidates = Vec::new();
    if let Some(cur) = s.include_stack.last() {
        if let Some(dir) = cur.rsplit_once('/').map(|(d, _)| d) {
            candidates.push(format!("{dir}/{p}"));
        }
    }
    candidates.push(format!("lua/{p}"));
    candidates.push(format!("gamemodes/{p}"));
    for gm in &s.gamemode_chain {
        candidates.push(format!("gamemodes/{gm}/entities/{p}"));
    }
    candidates.push(p.clone());
    for c in candidates {
        let full = s.game_dir.join(&c);
        if full.is_file() {
            return Some((c, full));
        }
    }
    None
}

fn include(lua: &Lua, st: &Shared, path: &str) -> mlua::Result<MultiValue> {
    let found = resolve(&st.borrow(), path);
    let Some((rel, full)) = found else {
        let e = format!("Couldn't include file '{path}' - File not found");
        warn!(target: "lua", "[ERROR] {e}");
        let mut s = st.borrow_mut();
        s.errors += 1;
        if s.error_log.len() < 500 {
            s.error_log.push(e);
        }
        drop(s);
        return Ok(MultiValue::new());
    };
    let src = read_lossy(&full);
    let f = match compile(lua, &src, &rel) {
        Ok(f) => f,
        Err(e) => {
            report(st, &e);
            return Ok(MultiValue::new());
        }
    };
    st.borrow_mut().include_stack.push(rel);
    let r = f.call::<MultiValue>(());
    st.borrow_mut().include_stack.pop();
    match r {
        Ok(v) => Ok(v),
        Err(e) => {
            report(st, &e);
            Ok(MultiValue::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_tree(files: &[(&str, &str)]) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "srcds_lua_rt_{}_{}",
            std::process::id(),
            files.len()
        ));
        for (p, body) in files {
            let f = d.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, body).unwrap();
        }
        d
    }

    #[test]
    fn include_require_and_cs_files() {
        let d = tmp_tree(&[
            ("lua/a/main.lua", "AddCSLuaFile() include('sub.lua') require('mod') RESULT = SUB .. MOD"),
            ("lua/a/sub.lua", "SUB = 'sub'"),
            ("lua/includes/modules/mod.lua", "MOD = 'mod' AddCSLuaFile('a/sub.lua')"),
        ]);
        let g = GLua::new(&d).unwrap();
        assert!(g.run_file("a/main.lua"));
        let r: String = g.lua.globals().get("RESULT").unwrap();
        assert_eq!(r, "submod");
        assert_eq!(g.state.borrow().client_files, ["lua/a/main.lua", "lua/a/sub.lua"]);
    }

    #[test]
    fn stubs_count_missing_engine_functions() {
        let d = tmp_tree(&[("lua/s.lua", "game.AddDecal('x') game.AddDecal('y') util.AddNetworkString('x') local ok = (GAMEMODE == nil) OK = ok")]);
        let g = GLua::new(&d).unwrap();
        assert!(g.run_file("s.lua"));
        let m = g.missing_report(10);
        assert!(m.contains(&("game.AddDecal".to_string(), 2)), "{m:?}");
        assert!(!m.iter().any(|(n, _)| n == "util.AddNetworkString"));
        // ALL_CAPS globals stay nil
        assert!(g.lua.globals().get::<bool>("OK").unwrap());
    }

    #[test]
    fn errors_are_reported_and_loading_continues() {
        let d = tmp_tree(&[("lua/e.lua", "error('boom')"), ("lua/ok.lua", "include('e.lua') AFTER = true")]);
        let g = GLua::new(&d).unwrap();
        assert!(g.run_file("ok.lua"));
        assert!(g.lua.globals().get::<bool>("AFTER").unwrap());
        assert_eq!(g.state.borrow().errors, 1);
    }
}
