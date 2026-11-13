# srcds-rs

Independent, native Rust implementation of a Garry's Mod dedicated server — **early prototype**.

Current state (see `docs/PROGRESS.md`): M0 (bootstrap) and M1 (Source query protocol,
verified byte-for-byte against captures from the real GMod dedicated server). Game clients
cannot connect yet.

```
cargo run --release -p srcds-server -- --port 27015 --map gm_construct --max-players 32
```

Options: `--ip` (default `127.0.0.1`), `--hostname`, `--password`, `--game-dir <garrysmod>`
(reads `steam.inf` / `garrysmod.ver` for reported version), `--tickrate`, `--metrics-interval`.
Type `quit` on stdin or press Ctrl+C to stop.

Tools: `cargo run -p packet-diff -- ref.bin ours.bin --ignore port,steam_id`,
`python scripts/a2s_probe.py HOST PORT [OUTDIR]`.

Docs: `docs/` (ARCHITECTURE, PROTOCOL, RESEARCH, COMPATIBILITY, LEGAL_SOURCES, ...).
