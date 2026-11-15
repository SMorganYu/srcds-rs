# Compatibility matrix

Сравнение: эталонный GMod DS (2026.04.29 / 261104, win64, `sv_lan 1`, LAN-IP) vs
srcds-rs 0.1.0 (loopback). Инструмент: `packet-diff`.

| Функция | Статус |
|---|---|
| UDP bind / приём / отправка | Implemented, Tested |
| Challenge (A2S) | Implemented, Tested, формат совпадает с эталоном |
| A2S_INFO | Implemented, Tested; все поля кроме ожидаемых различий **эквивалентны** эталону |
| A2S_PLAYER (пусто) | Implemented, Tested; **побайтово** совпадает с эталоном |
| A2S_RULES + split | Implemented, Tested; энкодер/сплиттер побайтово воспроизводят эталон на его данных |
| Проверка стандартным клиентом запросов (браузер серверов GMod) | **Not verified** |
| Handshake клиента (M2) | **Verified with original client**: challenge → connect → `Connected to …` → ServerInfo принят (баннер в консоли), затем чистый `net_Disconnect` с объяснением |
| Netchannel (seq/ack/checksum/16 подканалов/фрагменты/LZSS/split) | Implemented, unit + e2e тесты |
| **Полный signon → вход в карту** | **Verified**: оригинальный клиент доходит до SignonState(FULL), грузит gm_construct, держится стабильно. Включён по умолчанию; `--minimal-signon` — только ServerInfo + чистый disconnect |
| String tables (формат GMod) | Implemented; `downloadables` побитово = эталону; полный набор отдаётся в signon |
| Собственные SendTable + ClassInfo | **Verified**: клиент принимает таблицы сервера (CWorld, CGMOD_Player, CPlayerResource) |
| Сущности (мир, игрок, player resource), полные и дельта-снапшоты | **Verified (спавн)**: `getpos` клиента = точка спавна + рост глаз |
| Клиентский Lua base/sandbox | **Verified**: загружается без ошибок (список файлов по правилам отбора, нижний регистр путей) |
| Движение | Implemented: ходьба (гравитация, трение, ступеньки, прыжок, скольжение по стенам) с коллизией по brush-геометрии BSP; noclip по команде `noclip` (V). Живая проверка: игрок ходит. Есть присед и коллизия с displacement-рельефом (односторонние треугольники, как в движке). Нет: вода, лестницы, пропы, другие игроки |
| `status` / `ping` | Implemented: в консоли игрока и сервера (карточка с логотипом srcds-rs, таблица игроков, RTT по netchannel) |
| Серверный GLua, оружие, физика, пропы | Not implemented |

## Намеренные отличия A2S_INFO (честные значения)

| Поле | Эталон | srcds-rs | Причина |
|---|---|---|---|
| `game` | `Sandbox` | `srcds-rs` | gamemode не выполняется |
| `keywords` | ` gm:sandbox gmc:other ver:261104` | ` gmc:other ver:261104 srcds-rs` | `gm:` опущен (нет gamemode); `ver:` берётся из `garrysmod.ver` при `--game-dir` |
| EDF / `steam_id` | `0xB1`, есть | `0xA1`, нет | нет Steam GameServer логина |
| `vac` | 0 | 0 | — |
| rules | 148 cvar'ов | `srcds_rs_version`, `sv_password` | только реально существующие настройки |

## Наблюдения о совместимости

* Эталон не отвечает на A2S с 127.0.0.1 (см. `RESEARCH.md`). srcds-rs отвечает — это
  отличие поведения, сознательно оставлено для тестируемости; может быть сделано
  настраиваемым, если выяснится, что клиент на это опирается.
* Клиент — build 261112, эталонный DS — 261104: [факт] клиент подключается к эталону и
  загружает gm_construct, т.е. несовпадение сборок соединению не мешает.
* [факт] Клиент не шлёт пакеты на 127.0.0.1 — тестировать с LAN-адресом хоста.

## Sandbox (M6), 2026-11-14

| Функция | Реализация | Проверка |
|---|---|---|
| Загрузка серверного Lua sandbox | GLua на LuaJIT, реальные файлы `lua/` и `gamemodes/` | авто: 0 ошибок, 585 файлов компилируются |
| Спавн игрока, лоадаут, модель | хуки gamemode | авто (e2e); живой клиент — спавн подтверждён ранее |
| `gm_spawn` → проп | Lua `CCSpawn` → `ents.Create` → сеть `CPhysicsProp` | авто (e2e на gm_construct) |
| Физика пропов | своя упрощённая | авто (падение, сон, стопка) |
| Физган | серверная логика srcds-rs + хуки sandbox | авто (захват/перенос/отпускание) |
| Тулган / SWEP | Lua SWEP + userinfo-конвары | частично авто |
| net, SendLua, чат, ChatPrint | по фактам эталона | авто (roundtrip net) |
| Смерть/респавн | `kill`, `PlayerDeathThink` | авто (kill) |
| NPC | только статичная модель | ограничение |