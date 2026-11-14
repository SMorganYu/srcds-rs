# Roadmap

| Milestone | Критерий | Статус |
|---|---|---|
| M0 Bootstrap | Rust-процесс, UDP, корректный shutdown | **Достигнут** (тесты) |
| M1 Query compatibility | Корректные A2S_INFO/PLAYER/RULES | **Достигнут на уровне протокола** (golden-тесты с эталоном); проверка браузером серверов клиента — не выполнена |
| M2 Network handshake | Оригинальный клиент проходит стадии соединения | **Достигнут** (живой тест: `Connected to`, ServerInfo принят, чистый disconnect; e2e-тест) |
| M3 World loading | Клиент загружает gm_construct | **Достигнут** (живой тест: оригинальный клиент доходит до SignonState(FULL), `Map: gm_construct`, без краша) |
| M4 Player simulation | Спавн и движение | — |
| M5 Multiplayer | Два клиента видят друг друга | — |
| M6 GMod runtime | GLua + Sandbox | — |
| M7 Extended | Аддоны, режимы | — |
| M8 Optimization | Честные сравнительные нагрузочные тесты | — |

## Ближайшие задачи (M3 → M4)

1. **svc_ServerInfo хвост**: клиент GMod дочитывает ≥11 бит после строки gamemode;
   реализовано как 16-битный нулевой хвост (`SERVERINFO_GMOD_TRAILER_BITS`). Выяснить
   смысл этих полей.
2. Datatables (`svc_SendTable`/CRC) и реальные baseline'ы классов — пока клиент входит
   в карту с `ClassInfo(create_on_client)` и пустыми таблицами; сущности и GLua-геймод
   не работают (ошибки Lua на клиенте: нет includes/derma/Color и т.п.).
3. Отдавать содержимое client Lua (base/sandbox) и завести GLua-рантайм (M6).
4. `PacketEntities` с реальными сущностями, движение игрока (M4).
5. `srcds-steam`: FFI к официальному `steam_api64.dll` для не-LAN режима.
