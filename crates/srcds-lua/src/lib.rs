//! GLua (Garry's Mod Lua) server runtime on LuaJIT, the same VM GMod uses.

pub mod ammo;
pub mod entity;
pub mod game;
pub mod lib_debug;
pub mod lib_ents;
pub mod lib_file;
pub mod lib_misc;
pub mod lib_net;
pub mod lib_sql;
pub mod loader;
pub mod meta;
pub mod runtime;
pub mod syntax;
pub mod vecang;

pub use runtime::GLua;

pub use mlua;

#[cfg(test)]
mod tests {
    #[test]
    fn luajit_runs() {
        let lua = mlua::Lua::new();
        let v: String = lua.load("return jit and jit.version or _VERSION").eval().unwrap();
        assert!(v.contains("LuaJIT"), "{v}");
    }
}