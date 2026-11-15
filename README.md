# srcds-rs

Independent, native Rust implementation of a Garry's Mod dedicated server that the
**unmodified** GMod client connects to with a normal `connect <ip>:<port>`. Early prototype.

Confirmed in game: joining from the original client on gm_construct, the stock `sandbox`
gamemode (run by the server's own Lua runtime) with client Lua download, the weapon
selector and hands, kill/respawn. Implemented but still buggy or not yet confirmed in game:
player movement (crouch, jump, ladders, noclip), props and the spawn menu, physgun,
toolgun, HL2 weapons, the gravity gun. Details and test logs: `docs/PROGRESS.md`,
`docs/TEST_RESULTS.md`.

Not done yet: Steam authentication (so only LAN / `-insecure`-style play), sounds and
decals sent by the server, NPC AI, many Lua APIs (stubbed). Expect bugs.

## Install

### 1. Requirements

* Windows 10/11 x64 (the only platform tested so far).
* [Rust](https://rustup.rs) stable (the repo pins `stable` in `rust-toolchain.toml`).
* A C compiler for the bundled LuaJIT build: Visual Studio Build Tools with the
  "Desktop development with C++" workload.
* Garry's Mod dedicated server files, which srcds-rs reads maps, Lua and VPKs from. They
  are not included here. Get them with [SteamCMD](https://developer.valvesoftware.com/wiki/SteamCMD):

  ```
  steamcmd +force_install_dir C:\gmodds +login anonymous +app_update 4020 validate +quit
  ```

  srcds-rs only reads these files; it never writes to them.

### 2. Build

```
git clone https://github.com/SMorganYu/srcds-rs.git
cd srcds-rs
cargo build --release -p srcds-server
```

The binary is `target\release\srcds-rs.exe`.

> **Known issue:** the LuaJIT build fails if the path to the repository contains `!`.
> Clone it into a plain path (for example `C:\src\srcds-rs`) or pass
> `--target-dir C:\build\srcds-rs`.

### 3. Run

```
target\release\srcds-rs.exe --ip 192.168.0.10 --port 27015 --game-dir C:\gmodds\garrysmod
```

* `--ip`: the address to listen on. Use the machine's **LAN address**, not `127.0.0.1`:
  the GMod client does not connect to a loopback server. The default is `127.0.0.1` on
  purpose, so a public address is never bound by accident.
* `--game-dir`: the `garrysmod` folder of the dedicated server install. Needed for the map,
  the gamemode and everything in game.
* Other options: `--port` (27015), `--map` (`gm_construct`), `--max-players` (32),
  `--hostname`, `--password`, `--tickrate` (66.67), `--metrics-interval` (seconds, 0 = off).
  `srcds-rs.exe --help` lists them all.
* Logs go to the console; set `RUST_LOG=debug` for more. Type `quit` or press Ctrl+C to stop.

Do not expose the port to the internet: there is no Steam authentication yet.

### 4. Connect

In Garry's Mod open the console (`~`) and type:

```
connect 192.168.0.10:27015
```

For bug reports, run `cl_showerror 2` in the client console first; prediction errors then
go to `garrysmod\console.log` (start GMod with `-condebug` to write that file).

## Development

* Tests: `cargo test --workspace`. The end-to-end sandbox tests run only when
  `SRCDS_RS_GMOD_DIR` points at a `garrysmod` folder.
* Tools: `cargo run -p packet-diff -- ref.bin ours.bin --ignore port,steam_id`,
  `python scripts/a2s_probe.py HOST PORT [OUTDIR]`.
* Docs: `docs/` (ARCHITECTURE, PROTOCOL, RESEARCH, COMPATIBILITY, LEGAL_SOURCES, ...).

## Legal

No game files, assets or Valve code are included. The server is an independent
implementation; see `docs/LEGAL_SOURCES.md` for what was used as reference and how.
