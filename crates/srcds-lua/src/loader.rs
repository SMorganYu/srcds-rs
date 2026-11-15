//! Server start-up sequence: base libraries, autorun scripts, the gamemode (with its
//! base gamemodes via `DeriveGamemode`), then scripted entities and weapons.

use std::path::Path;

use mlua::{Function, Table, Value};
use tracing::info;

use crate::runtime::{report, GLua};

impl GLua {
    /// Installs `DeriveGamemode` (loads a base gamemode into its own `GM` table and
    /// registers it before the current one continues).
    fn install_gamemode_loader(&self) -> mlua::Result<()> {
        let lua = &self.lua;
        let st = self.state.clone();
        lua.globals().set(
            "DeriveGamemode",
            lua.create_function(move |lua, base: String| {
                let g = lua.globals();
                let derived: Table = lua.named_registry_value("srcds_gm_derived").or_else(|_| {
                    let t = lua.create_table()?;
                    lua.set_named_registry_value("srcds_gm_derived", &t)?;
                    Ok::<_, mlua::Error>(t)
                })?;
                // remember what the gamemode being loaded derives from
                if let Value::Table(cur) = g.get::<Value>("GM")? {
                    if let Ok(name) = cur.get::<String>("FolderName") {
                        derived.set(name, base.as_str())?;
                    }
                }
                let gm_mod: Table = g.get("gamemode")?;
                let get: Function = gm_mod.get("Get")?;
                if !matches!(get.call::<Value>(base.as_str())?, Value::Nil) {
                    return Ok(());
                }
                let saved: Value = g.get("GM")?;
                let st2 = st.clone();
                load_gamemode_table(lua, &st2, &base, None)?;
                g.set("GM", saved)?;
                Ok(())
            })?,
        )
    }

    /// Runs the whole server start-up for `gamemode` (e.g. `sandbox`).
    pub fn boot(&self, gamemode: &str) -> mlua::Result<()> {
        self.install_gamemode_loader()?;
        self.run_file("includes/init.lua");
        let game_dir = self.state.borrow().game_dir.clone();
        for dir in ["autorun", "autorun/server"] {
            for f in list_lua(&game_dir.join("lua").join(dir)) {
                self.run_file(&format!("{dir}/{f}"));
            }
        }
        // the base chain comes from gamemodes/<name>/<name>.txt ("base")
        let mut chain = vec![gamemode.to_string()];
        while let Some(b) = gamemode_base(&game_dir, chain.last().unwrap()) {
            if chain.contains(&b) {
                break;
            }
            chain.push(b);
        }
        self.state.borrow_mut().gamemode_chain = chain.clone();
        // gamemode settings become convars (FCVAR_ARCHIVE | FCVAR_NOTIFY | FCVAR_REPLICATED)
        for gm in chain.iter().rev() {
            for (name, default, help) in gamemode_settings(&game_dir, gm) {
                crate::lib_misc::create_convar(&self.lua, &name, &default, 128 | 256 | 8192, &help, None, None);
            }
        }
        for (i, gm) in chain.iter().enumerate().rev() {
            load_gamemode_table(&self.lua, &self.state, gm, chain.get(i + 1).cloned())?;
        }
        let g = self.lua.globals();
        let gm_mod: Table = g.get("gamemode")?;
        let get: Function = gm_mod.get("Get")?;
        let gm: Value = get.call(gamemode)?;
        g.set("GAMEMODE", gm.clone())?;
        // scripted entities and weapons of the gamemode chain and lua/
        let derived: Option<Table> = self.lua.named_registry_value("srcds_gm_derived").ok();
        while let Some(d) = derived
            .as_ref()
            .and_then(|t| t.get::<Option<String>>(chain.last().unwrap().as_str()).ok().flatten())
        {
            if chain.contains(&d) {
                break;
            }
            chain.push(d);
        }
        let mut roots = vec!["lua".to_string()];
        for gm in chain.iter().rev() {
            roots.push(format!("gamemodes/{gm}/entities"));
        }
        for root in &roots {
            self.load_scripted(root, "entities", "ENT", "scripted_ents")?;
            self.load_scripted(root, "weapons", "SWEP", "weapons")?;
        }
        info!(
            gamemode,
            client_files = self.state.borrow().client_files.len(),
            errors = self.state.borrow().errors,
            "Lua loaded"
        );
        Ok(())
    }

    /// Loads `<root>/<kind>/<name>.lua` or `<root>/<kind>/<name>/{shared,init}.lua`
    /// into a fresh table bound to `global` and registers it with `<registry>.Register`.
    fn load_scripted(&self, root: &str, kind: &str, global: &str, registry: &str) -> mlua::Result<()> {
        let game_dir = self.state.borrow().game_dir.clone();
        let dir = game_dir.join(root).join(kind);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return Ok(());
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        let lua = &self.lua;
        let g = lua.globals();
        for e in entries {
            let fname = e.file_name().to_string_lossy().to_string();
            let path = e.path();
            let (name, files): (String, Vec<String>) = if path.is_dir() {
                let mut f = Vec::new();
                for part in ["init.lua", "shared.lua"] {
                    if path.join(part).is_file() {
                        f.push(format!("{root}/{kind}/{fname}/{part}"));
                        break;
                    }
                }
                (fname.clone(), f)
            } else if let Some(n) = fname.strip_suffix(".lua") {
                (n.to_string(), vec![format!("{root}/{kind}/{fname}")])
            } else {
                continue;
            };
            if files.is_empty() {
                continue;
            }
            let t = lua.create_table()?;
            t.set("Folder", format!("{kind}/{name}"))?;
            t.set("ClassName", name.as_str())?;
            if global == "SWEP" {
                t.set("Primary", lua.create_table()?)?;
                t.set("Secondary", lua.create_table()?)?;
            }
            g.set(global, &t)?;
            for f in files {
                self.run_file(&f);
            }
            let reg: Table = g.get(registry)?;
            if let Value::Function(r) = reg.get::<Value>("Register")? {
                if let Err(e) = r.call::<()>((t.clone(), name.as_str())) {
                    report(&self.state, &e);
                }
            }
            // [факт] (includes/modules/baseclass.lua) the engine registers every
            // scripted entity/weapon as a base class, so `DEFINE_BASECLASS` /
            // `BaseClass.Think(self)` resolve to it
            // the class table inherits from its base class table (metatable __index), so
            // e.g. base_gmodentity's server-side Think comes from base_entity
            let link: Function = lua
                .load(
                    "local t, name, is_swep = ...
                     local defaults = { anim = 'base_anim', point = 'base_point', brush = 'base_brush',
                                        filter = 'base_filter', ai = 'base_ai', nextbot = 'base_nextbot' }
                     local base = t.Base or (is_swep and 'weapon_base' or defaults[t.Type or 'anim'])
                     if base and base ~= name then
                         setmetatable(t, { __index = baseclass.Get(base) })
                     end
                     baseclass.Set(name, t)",
                )
                .set_name("=srcds_baseclass_link")
                .into_function()?;
            if let Err(e) = link.call::<()>((t, name.as_str(), global == "SWEP")) {
                report(&self.state, &e);
            }
            g.set(global, Value::Nil)?;
        }
        Ok(())
    }
}

/// Loads `gamemodes/<name>/gamemode/init.lua` into a new `GM` table and registers it.
/// The `base` key of `gamemodes/<name>/<name>.txt` (KeyValues), if not empty.
pub fn gamemode_base(game_dir: &Path, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(game_dir.join("gamemodes").join(name).join(format!("{name}.txt"))).ok()?;
    let toks = kv_tokens(&text);
    // top level: "name" { key value ... } -- only depth-1 pairs are considered
    let mut depth = 0;
    let mut i = 0;
    while i < toks.len() {
        match toks[i].as_str() {
            "{" => depth += 1,
            "}" => depth -= 1,
            k if depth == 1 && k.eq_ignore_ascii_case("base") => {
                let v = toks.get(i + 1)?.clone();
                return (!v.is_empty() && v != "{").then_some(v);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Convars declared in the `settings` block of `gamemodes/<name>/<name>.txt`:
/// (name, default, help).
pub fn gamemode_settings(game_dir: &Path, name: &str) -> Vec<(String, String, String)> {
    let Ok(text) = std::fs::read_to_string(game_dir.join("gamemodes").join(name).join(format!("{name}.txt"))) else {
        return Vec::new();
    };
    let toks = kv_tokens(&text);
    let mut out = Vec::new();
    // find "settings" at depth 1, then each entry block at depth 3
    let mut depth = 0;
    let mut in_settings = false;
    let mut settings_depth = 0;
    let mut cur: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i].as_str();
        match t {
            "{" => depth += 1,
            "}" => {
                if in_settings && depth == settings_depth + 2 {
                    let get = |k: &str| cur.iter().find(|(a, _)| a.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone());
                    if let Some(n) = get("name") {
                        out.push((n, get("default").unwrap_or_default(), get("help").unwrap_or_default()));
                    }
                    cur.clear();
                }
                depth -= 1;
                if in_settings && depth == settings_depth {
                    in_settings = false;
                }
            }
            _ if depth == 1 && t.eq_ignore_ascii_case("settings") => {
                in_settings = true;
                settings_depth = 1;
            }
            _ if in_settings && depth == settings_depth + 2 => {
                if let Some(v) = toks.get(i + 1).filter(|v| *v != "{" && *v != "}") {
                    cur.push((t.to_string(), v.clone()));
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

pub(crate) fn kv_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '"' => {
                let mut s = String::new();
                for c in it.by_ref() {
                    if c == '"' {
                        break;
                    }
                    s.push(c);
                }
                out.push(s);
            }
            '{' | '}' => out.push(c.to_string()),
            '/' if it.peek() == Some(&'/') => {
                for c in it.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            c if c.is_whitespace() => {}
            c => {
                let mut s = c.to_string();
                while let Some(&n) = it.peek() {
                    if n.is_whitespace() || n == '"' || n == '{' || n == '}' {
                        break;
                    }
                    s.push(n);
                    it.next();
                }
                out.push(s);
            }
        }
    }
    out
}

fn load_gamemode_table(lua: &mlua::Lua, st: &crate::runtime::Shared, name: &str, base: Option<String>) -> mlua::Result<()> {
    let g = lua.globals();
    let gm = lua.create_table()?;
    gm.set("FolderName", name)?;
    gm.set("Folder", format!("gamemodes/{name}"))?;
    gm.set("Name", name)?;
    g.set("GM", &gm)?;
    let path = format!("{name}/gamemode/init.lua");
    let inc: Function = g.get("include")?;
    if let Err(e) = inc.call::<()>(path) {
        report(st, &e);
    }
    let derived: Option<String> = lua
        .named_registry_value::<Table>("srcds_gm_derived")
        .ok()
        .and_then(|t| t.get::<Option<String>>(name).ok().flatten())
        .or(base);
    if let Some(b) = &derived {
        let t: Table = match lua.named_registry_value::<Table>("srcds_gm_derived") {
            Ok(t) => t,
            Err(_) => {
                let t = lua.create_table()?;
                lua.set_named_registry_value("srcds_gm_derived", &t)?;
                t
            }
        };
        t.set(name, b.as_str())?;
    }
    let gm_mod: Table = g.get("gamemode")?;
    if let Value::Function(reg) = gm_mod.get::<Value>("Register")? {
        if let Err(e) = reg.call::<()>((gm, name, derived)) {
            report(st, &e);
        }
    }
    Ok(())
}

fn list_lua(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_file())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.ends_with(".lua"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    /// Boots the real sandbox gamemode (`SRCDS_RS_GMOD_DIR=<garrysmod>`) and prints
    /// the most-hit missing engine functions.
    #[test]
    fn boot_real_sandbox() {
        let Ok(root) = std::env::var("SRCDS_RS_GMOD_DIR") else {
            return;
        };
        let g = crate::GLua::new(std::path::Path::new(&root)).unwrap();
        g.boot("sandbox").unwrap();
        let s = g.state.borrow();
        eprintln!("errors: {}, client files: {}", s.errors, s.client_files.len());
        for e in s.error_log.iter().take(40) {
            eprintln!("  ERR {e}");
        }
        drop(s);
        for (k, n) in g.missing_report(60) {
            eprintln!("  {n:6} {k}");
        }
    }
}
