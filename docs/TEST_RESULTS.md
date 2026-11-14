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

## 2026-11-14 — M2 (handshake, netchannel)

```
cargo fmt --all -- --check                                  # OK
cargo clippy --workspace --all-targets -- -D warnings       # OK
cargo test --workspace                                      # 77 passed, 0 failed
```

Новые наборы: `srcds-protocol` (+22: LZSS, handshake-векторы из захвата, netchannel —
checksum эталонного keep-alive, reliable single/fragmented с потерей и повтором,
порядок батчей, враждебные размеры; net-сообщения; string tables — `downloadables`
равен эталону по длине 335 бит), `srcds-core` (+5: BSP MD5/модели/skyname, signon) и
**e2e `tests/connect.rs`**: UDP-handshake + netchannel против реального `Server` на
синтетической карте → `svc_ServerInfo` и чистый `net_Disconnect`.

### Живой тест с оригинальным клиентом (не изменялся)

* srcds-rs: `--ip 192.168.0.174 --port 27215 --game-dir <копия garrysmod>`; клиент:
  `gmod.exe -hijack +connect 192.168.0.174:27017` (relay → 27215).
* Результат: `Connecting to …` → `Connected to 192.168.0.174:27017` → баннер srcds-rs
  (svc_Print) → ServerInfo принят (MD5 карты совпал) → `Disconnect: srcds-rs: connected,
  server info received. Full signon … is not implemented yet` → клиент жив, без краша.
* С `--experimental-signon` (полный набор таблиц) клиент падает до `SignonState(3)`;
  с 0 таблиц — чистая ошибка `Host_Error: Invalid download file table`. Поэтому по
  умолчанию полный signon выключен (не слать клиенту данные, которые он не может
  обработать).

## 2026-11-14 — M3 (вход клиента в карту)

```
cargo fmt / clippy -D warnings / test --workspace     # 78 passed, 0 failed
```

Найден блокер M2→M3: `svc_ServerInfo` у GMod имеет хвост после строки gamemode —
клиент доходит до SignonState(FULL) только если присутствует ≥ 11 доп. бит (бисекция
живым клиентом: 10 → краш/бросает signon, 11 → FULL). Реализовано как 16-битный нулевой
хвост (`SERVERINFO_GMOD_TRAILER_BITS`).

### Живой тест с оригинальным клиентом (дефолтная конфигурация, без флагов)

`srcds-rs --ip <LAN> --port 27215 --game-dir <копия garrysmod>`; клиент
`gmod.exe -hijack +connect <LAN>:27017` (relay → 27215):

* `Connected to …` → все состояния signon 2→3→4→5→6, сервер логирует `client fully
  connected`, клиент: `Map: gm_construct`. Клиент жив, краш-дампов не прибавилось,
  сервер клиента не отключал (стабильно > 10 c).
* Ограничение: клиентский Lua падает (нет includes/derma/Color/concommand) — геймод не
  инициализируется; сущности не передаются. Игрок подключён, но не играбелен. Это M4+.

Тест `tests/connect.rs` расширен: `handshake_then_graceful_disconnect_minimal`
(`--minimal-signon`) и `full_signon_sends_tables_and_advances_state` (ServerInfo +
svc_CreateStringTable, без disconnect).
