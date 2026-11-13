# Roadmap

| Milestone | Критерий | Статус |
|---|---|---|
| M0 Bootstrap | Rust-процесс, UDP, корректный shutdown | **Достигнут** (тесты) |
| M1 Query compatibility | Корректные A2S_INFO/PLAYER/RULES | **Достигнут на уровне протокола** (golden-тесты с эталоном); проверка браузером серверов клиента — не выполнена |
| M2 Network handshake | Оригинальный клиент проходит стадии соединения | **Достигнут** (живой тест: `Connected to`, ServerInfo принят, чистый disconnect; e2e-тест) |
| M3 World loading | Клиент загружает gm_construct | — |
| M4 Player simulation | Спавн и движение | — |
| M5 Multiplayer | Два клиента видят друг друга | — |
| M6 GMod runtime | GLua + Sandbox | — |
| M7 Extended | Аддоны, режимы | — |
| M8 Optimization | Честные сравнительные нагрузочные тесты | — |

## Ближайшие задачи (M2 → M3)

1. Выяснить, на какой таблице/данных клиент падает в полном signon: сравнить состав
   таблиц с эталоном по одной (диагностика `SRCDS_RS_MAX_TABLES` + `--experimental-signon`);
   уже установлено, что `downloadables` побитово верен и что 0 таблиц даёт чистую ошибку
   `Invalid download file table`, а 1+ — падение до `SignonState(3)`.
2. Datatables/baselines: класс-таблицы GMod (250 классов, create-on-client) — нужно
   описание SendTable'ов сервера; источник данных — сам эталонный сервер (`sv_sendtables`
   / захват), без декомпиляции.
3. `instancebaseline`, `PacketEntities` для worldspawn и игрока (M3/M4).
4. `client_lua_files` для base/sandbox (SHA-256 содержимого + NUL) и GLua (M6).
5. `srcds-steam`: FFI к официальному `steam_api64.dll` для не-LAN режима.
6. Разблокировать IDA MCP (хук плагина) для анализа серверного `engine.dll`.
