//! Client Lua announced in the `client_lua_files` string table.
//!
//! The client runs only Lua files the server lists there (path + SHA-256 of the file
//! content followed by a NUL byte; [факт] hash verified against the reference server).
//! Until a GLua runtime decides this through `AddCSLuaFile`, srcds-rs either reads an
//! explicit manifest or discovers client files under `--game-dir` with path rules that
//! keep server-only code (gamemode `init.lua`, entity `init.lua`, `sv_*`, `server/`
//! folders) off the list, so it is never offered to clients.
//!
//! [факт] (live test 2026-11-14) the client needs the core libraries
//! (`lua/includes/**`, `lua/derma/**`) listed as well: with only the reference
//! server's list it fails on `includes/init.lua`; with them added the base and sandbox
//! client code loads. Why the reference client does not need them listed is not
//! established ([гипотеза]: a mechanism outside this table).

use std::path::Path;

use sha2::{Digest, Sha256};
use tracing::{info, warn};

/// Hashes `rel` the way the client checks it: SHA-256 of `content + NUL`.
pub fn hash_file(game_dir: &Path, rel: &str) -> std::io::Result<[u8; 32]> {
    let mut data = std::fs::read(game_dir.join(rel))?;
    data.push(0);
    Ok(Sha256::digest(&data).into())
}

/// True for relative `a/b/c.lua` paths without `..`, empty or absolute components.
pub fn is_safe_lua_path(rel: &str) -> bool {
    !rel.starts_with('/')
        && rel.ends_with(".lua")
        && !rel
            .split('/')
            .any(|c| c == ".." || c.is_empty() || c.contains(':'))
}

/// Whether a discovered file is (or may be) needed by clients.
fn is_client_file(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    let file = parts.last().copied().unwrap_or("");
    if parts.iter().any(|p| p.eq_ignore_ascii_case("server")) || file.starts_with("sv_") {
        return false;
    }
    // Menu-state code is the client's own and is not part of a game session.
    if parts.get(1).is_some_and(|p| *p == "menu") || file == "init_menu.lua" {
        return false;
    }
    // `init.lua` is the server half of a gamemode or of a scripted entity / weapon /
    // effect folder; the core library entry points stay.
    if file == "init.lua" {
        let in_lib = parts.len() == 3
            && parts[0] == "lua"
            && matches!(parts[1], "includes" | "derma" | "vgui" | "skins");
        return in_lib;
    }
    true
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<String>, depth: u32) {
    if depth > 16 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        let child = format!("{rel}/{name}");
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk(&e.path(), &child, out, depth + 1);
        } else if ft.is_file()
            && name.ends_with(".lua")
            && is_safe_lua_path(&child)
            && is_client_file(&child)
        {
            out.push(child);
        }
    }
}

/// Client Lua under `game_dir`: `lua/` plus the `base` gamemode and `gamemode`.
pub fn discover(game_dir: &Path, gamemode: &str) -> Vec<String> {
    let mut out = Vec::new();
    walk(&game_dir.join("lua"), "lua", &mut out, 0);
    let mut gms = vec!["base"];
    if gamemode != "base" && is_safe_lua_path(&format!("{gamemode}/x.lua")) {
        gms.push(gamemode);
    }
    for gm in gms {
        walk(
            &game_dir.join("gamemodes").join(gm),
            &format!("gamemodes/{gm}"),
            &mut out,
            0,
        );
    }
    out
}

/// Reads a manifest (one relative path per line, `#` comments).
pub fn read_manifest(path: &Path) -> std::io::Result<Vec<String>> {
    Ok(std::fs::read_to_string(path)?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.replace('\\', "/"))
        .collect())
}

/// The client Lua files to announce, with their hashes. `manifest` overrides
/// discovery. Unsafe or unreadable paths are skipped with a warning.
pub fn load(game_dir: &Path, gamemode: &str, manifest: Option<&Path>) -> Vec<(String, [u8; 32])> {
    let (paths, source) = match manifest {
        Some(m) => match read_manifest(m) {
            Ok(p) => (p, "manifest"),
            Err(e) => {
                warn!(path = %m.display(), error = %e, "could not read client Lua manifest");
                return Vec::new();
            }
        },
        None => (discover(game_dir, gamemode), "discovered"),
    };
    let mut out = Vec::new();
    for rel in paths {
        if !is_safe_lua_path(&rel) {
            warn!(path = %rel, "skipping unsafe client Lua path");
            continue;
        }
        // [факт] the reference announces paths in lower case and the client looks
        // them up that way (an announced `lua/vgui/DPanPanel.lua` made the client try
        // to download it and fail). The file itself is read by its on-disk name.
        match hash_file(game_dir, &rel) {
            Ok(h) => out.push((rel.to_ascii_lowercase(), h)),
            Err(e) => warn!(path = %rel, error = %e, "client Lua file not found; skipped"),
        }
    }
    info!(files = out.len(), source, "client Lua files");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_only_files_excluded() {
        for p in [
            "gamemodes/base/gamemode/init.lua",
            "gamemodes/sandbox/entities/entities/sent_ball/init.lua",
            "lua/entities/x/init.lua",
            "lua/autorun/server/admin.lua",
            "gamemodes/sandbox/gamemode/sv_stuff.lua",
            "lua/menu/menu.lua",
            "lua/includes/init_menu.lua",
        ] {
            assert!(!is_client_file(p), "{p}");
        }
        for p in [
            "lua/includes/init.lua",
            "lua/derma/init.lua",
            "lua/includes/modules/notification.lua",
            "gamemodes/base/gamemode/cl_init.lua",
            "gamemodes/base/gamemode/shared.lua",
            "lua/autorun/client/x.lua",
        ] {
            assert!(is_client_file(p), "{p}");
        }
    }

    #[test]
    fn unsafe_paths_rejected() {
        assert!(is_safe_lua_path("lua/a.lua"));
        for p in [
            "/lua/a.lua",
            "lua/../a.lua",
            "lua//a.lua",
            "c:/a.lua",
            "lua/a.txt",
        ] {
            assert!(!is_safe_lua_path(p), "{p}");
        }
    }

    #[test]
    fn discovers_and_hashes() {
        let d = std::env::temp_dir().join(format!("srcds_rs_lua_{}", std::process::id()));
        for (p, body) in [
            ("lua/includes/init.lua", "x"),
            ("lua/vgui/DPanel.lua", "p"),
            ("gamemodes/base/gamemode/init.lua", "server"),
            ("gamemodes/base/gamemode/cl_init.lua", "client"),
            ("gamemodes/sandbox/gamemode/shared.lua", "shared"),
        ] {
            let f = d.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, body).unwrap();
        }
        let got = discover(&d, "sandbox");
        assert_eq!(
            got,
            [
                "lua/includes/init.lua",
                "lua/vgui/DPanel.lua",
                "gamemodes/base/gamemode/cl_init.lua",
                "gamemodes/sandbox/gamemode/shared.lua"
            ]
        );
        let all = load(&d, "sandbox", None);
        let want: [u8; 32] = Sha256::digest(b"x\0").into();
        assert_eq!(all[0].1, want);
        assert_eq!(all[1].0, "lua/vgui/dpanel.lua");
        let _ = std::fs::remove_dir_all(&d);
    }
}
