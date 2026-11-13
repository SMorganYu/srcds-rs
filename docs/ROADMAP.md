# Roadmap

| Milestone | Критерий | Статус |
|---|---|---|
| M0 Bootstrap | Rust-процесс, UDP, корректный shutdown | **Достигнут** (тесты) |
| M1 Query compatibility | Корректные A2S_INFO/PLAYER/RULES | **Достигнут на уровне протокола** (golden-тесты с эталоном); проверка браузером серверов клиента — не выполнена |
| M2 Network handshake | Оригинальный клиент проходит стадии соединения | Не начат (исследование) |
| M3 World loading | Клиент загружает gm_construct | — |
| M4 Player simulation | Спавн и движение | — |
| M5 Multiplayer | Два клиента видят друг друга | — |
| M6 GMod runtime | GLua + Sandbox | — |
| M7 Extended | Аддоны, режимы | — |
| M8 Optimization | Честные сравнительные нагрузочные тесты | — |

## Ближайшие задачи (M1 → M2)

1. Проверить srcds-rs в браузере серверов оригинального клиента (вкладка LAN / `connect`),
   зафиксировать, что видно (требует участия пользователя).
2. Разблокировать IDA MCP или продолжить анализ строк/импортов.
3. Захватить эталонный handshake: оригинальный клиент → копия эталонного DS (`sv_lan 1`,
   LAN-IP), запись UDP-трафика собственным relay/proxy на loopback (без изменения клиента).
4. Реализовать разбор `A2S_GETCHALLENGE`/`C2S_CONNECT` по захвату; отвечать `S2C_CONNREJECT`
   с понятной причиной, пока netchannel не готов (честный отказ — первый проверяемый шаг).
5. `srcds-protocol::netchan`: заголовок netchannel (sequence/ack/flags/checksum),
   reliable-подканалы, фрагментация — по захватам.
6. `srcds-steam`: FFI к официальному `steam_api64.dll` GameServer (для не-LAN режима).
