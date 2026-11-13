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
| Handshake клиента (M2) | Not implemented |
| Загрузка карты, сущности, игроки, Lua | Not implemented |

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
* Клиент — build 261112, эталонный DS — 261104 (одинаковый PatchVersion). Влияние на
  подключение не проверено.
