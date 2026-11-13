# Test results

## 2026-11-14 — M0/M1

Команды (Windows 11, rustc 1.99.1):

```
cargo fmt --all -- --check                                  # OK
cargo clippy --workspace --all-targets -- -D warnings       # OK, 0 warnings
cargo test --workspace                                      # 49 passed, 0 failed
```

| Набор | Тестов | Что проверяет |
|---|---|---|
| `srcds-protocol` unit | 25 | byte/bit буферы (known vectors, roundtrip, truncation, overflow, malformed varint/string), framing, A2S encode/decode, усечение в каждой позиции, враждебные счётчики, split/reassembly |
| `srcds-protocol` golden (`tests/reference_fixtures.rs`) | 5 | декодирование эталонных пакетов GMod DS; **побайтовое** совпадение нашего энкодера для INFO, PLAYER, RULES и split-фрагментов |
| `srcds-net` unit | 6 | loopback send/recv, timeout, challenge (выдача/проверка/истечение/привязка к IP), rate limiting (ограниченная таблица) |
| `srcds-core` unit | 8 | challenge-flow INFO/PLAYER/RULES, split больших rules, malformed/unhandled, per-IP лимит, метрики p50/p99, steam.inf |
| `srcds-server` integration (`tests/process.rs`) | 3 | запуск настоящего `srcds-rs`, A2S по UDP, мусорные датаграммы без ответа и без падения, `quit` → код 0, пароль не утекает, конфликт порта → ненулевой код |
| `packet-diff` unit | 2 | обнаружение/игнорирование различий, ошибки декодирования |

## Ручная проверка против эталона

1. Эталон: `srcds_win64.exe -console -game garrysmod -insecure -nohltv +ip 192.168.0.174
   -port 27115 +sv_lan 1 +maxplayers 8 +gamemode sandbox +map gm_construct` (копия
   `D:\!gmod\srcds-ref`). Захват: `python scripts/a2s_probe.py 192.168.0.174 27115 <dir>`.
2. srcds-rs: `srcds-rs --port 27215 --max-players 8 --hostname "srcds-ref vanilla"
   --game-dir <garrysmod>`; захват тем же пробником на 127.0.0.1.
3. `packet-diff ref/info_1.bin rs/info_1.bin --ignore port,steam_id,_len,edf,game,keywords`
   → `equivalent (22 fields compared)`; challenge и player — `equivalent`.
4. Метрики srcds-rs за 5 с: 334 тика (66.8 Гц), p50/p99 длительности тика < 1 мкс
   (тик пустой).

## Не проверено

* Браузер серверов клиента GMod против srcds-rs.
* Linux-сборка (WSL доступен, не запускался).
* Ctrl+C на Windows (проверен только `quit` через stdin).
