# Phase 0 — Reconnaissance

Дата: 2026-11-14. Все утверждения ниже помечены как **[факт]** (проверено командой/тестом
в этой сессии) или **[гипотеза]**.

## 0.1 Окружение

| Компонент | Значение | Статус |
|---|---|---|
| ОС | Windows 11 IoT Enterprise LTSC 2024 (10.0.26100), x86-64 | [факт] |
| Rust | rustc 1.99.1 stable, cargo 1.99.1, toolchain `stable-x86_64-pc-windows-msvc` | [факт] |
| Git | 2.56.0.windows.4 | [факт] |
| Python | 3.12.10 (используется только для независимого A2S-пробника) | [факт] |
| MSVC | линкер найден cargo (сборка проходит) | [факт] |
| IDA MCP | плагин `ida-mcp` присутствует (`list_databases`, `open_database`, `execute_python`, ...), но **вызов заблокирован**: классификатор auto-mode не смог проверить вызов, т.к. хук плагина модифицирует входные параметры. См. `IDA_FINDINGS.md`. | [факт] |
| Linux-окружение | WSL / Ubuntu 24.04 установлены, в этой сессии не использовались | [факт] |

## 0.2 Инвентаризация

Полная таблица (SHA-256, PE/ELF, разрядность, зависимости) — `docs/INVENTORY.md`.

| Набор | Путь | Содержимое | Статус |
|---|---|---|---|
| Эталонный DS (Windows) | `D:\steammd\steamapps\common\billytest` | `srcds.exe` (x86), `srcds_win64.exe` (x86-64), `bin\*.dll` (x86), `bin\win64\*.dll` (x86-64), `garrysmod\`, `sourceengine\` (HL2/CSS VPK). Установка содержит сторонние аддоны и кастомный Lua (`lua\bin`, `lua\prot`, `autorun\000_prot.lua`, ...), поэтому **не является ванильной**. | [факт] |
| Depot 4022 | `D:\!gmod\depots\4022\26623757` | Windows-бинарники GMod (включая `client.dll`, `hammer_dll.dll`, `gmod_audio.dll` → клиентский набор + `srcds*.exe`). Хэши `bin\win64\*.dll` совпадают с установленным клиентом. | [факт] |
| Depot 4023 | `D:\!gmod\depots\4023\26623757` | Linux DS: `bin/linux32/*.so` (ELF32 i386), `bin/linux64/*.so` (ELF64 x86-64), `srcds_run`, `srcds_run_x64`. | [факт] |
| Клиент | `C:\Program Files (x86)\Steam\steamapps\common\GarrysMod` | Только чтение (хэши, `steam.inf`, `garrysmod.ver`). **Не изменялся.** | [факт] |
| CS partner src | `C:\Users\Morgan\Downloads\csgo_partner\src` | Существует (каталоги `appframework`, `bonesetup`, `common`, `datacache`, ...). Правовой статус: неофициальная публикация → **не использовался** (см. `LEGAL_SOURCES.md`). | [факт] |
| nillerusr/source-engine | — | Поиск (глубина 4 по `D:\` и профилю пользователя) копию не нашёл; **не использовался**. | [факт] |
| Source SDK 2013 | `D:\!gmod\source-sdk-2013-master` | Найден; в этой сессии не читался (M1 опирается на публичную документацию A2S). | [факт] |
| Facepunch/garrysmod | `D:\!gmod\garrysmod-master` | Найден; понадобится для M6. | [факт] |

Ключевые зависимости (импорт-таблицы, [факт]):

* `dedicated.dll` → `WSOCK32`, `steam_api(64)`, `tier0`, `vstdlib`.
* `engine.dll` → `WS2_32`, `WININET`, `WINMM`, `steam_api(64)`, `tier0`, `vstdlib`.
* `server.dll` → `WS2_32`, `CRYPT32`, `steam_api(64)`, `tier0`, `vstdlib`.
* `lua_shared.dll`, `vphysics.dll` → только `tier0`/`vstdlib` (+ `KERNEL32`).
* `srcds_win64.exe` → только `USER32`, `KERNEL32` (загрузчик; модули грузятся динамически).
* Linux: всё зависит от `libtier0.so`, `libvstdlib.so`, `libsteam_api.so`, glibc/libstdc++.

## 0.3 Версии

| Сборка | PatchVersion (`steam.inf`) | `garrysmod.ver` (1-я строка) | engine.dll win64 SHA-256 (префикс) |
|---|---|---|---|
| Клиент | 2026.04.29 | **261112** | `6432f653…` |
| Depot 4022 | — | — | `6432f653…` (= клиент) |
| Эталонный DS (billytest) | 2026.04.29 | **261104** | `4525b257…` |

[факт] Эталонный DS и клиент имеют **одинаковый PatchVersion, но разные сборки**
(`garrysmod.ver` 261104 vs 261112; различаются `engine.dll`, `server.dll`, `dedicated.dll`,
`lua_shared.dll`; `srcds_win64.exe` идентичен). Перед тестами M2 нужно выяснить, примет ли
клиент 261112 сервер 261104 (и что сообщать в `ver:`) — **открытый вопрос**.

[гипотеза] Depot 4023 соответствует тому же набору манифестов, что 4022 (одинаковое имя
каталога `26623757`); сборка Linux DS не идентифицирована.

## 0.4 Карта подсистем (подтверждённые зависимости)

```
srcds_win64.exe ──loads──> dedicated.dll ──> engine.dll ──> server.dll (game)
                               │                 │              ├─ lua_shared.dll (GLua runtime)
                               │                 │              └─ vphysics.dll
                               │                 ├─ filesystem_stdio.dll (VPK/FS)
                               │                 ├─ datacache, materialsystem(shaderapiempty), studiorender
                               │                 └─ steam_api64.dll (Steam GameServer)
                               └─ tier0.dll / vstdlib.dll (общие для всех модулей)
```

Порядок загрузки (`loads`) — [гипотеза], основан на импортах и общем устройстве Source
(SDK 2013); строгое подтверждение требует отладки/IDA. Наличие ссылок на модули — [факт].

## 0.5 Эталонное поведение

Создана изолированная тестовая копия `D:\!gmod\srcds-ref` (вне репозитория): hard-link'и
на read-only файлы billytest (VPK/BSP/DLL), **без** аддонов, кастомного Lua, `data`, `cfg`;
ванильная часть `lua/` (includes, vgui, ...) и минимальный `server.cfg`. Исходная установка
не изменялась.

Наблюдения [факт]:

1. При `+ip 127.0.0.1` эталонный сервер **не отвечает** на A2S_INFO/PLAYER/RULES, в 4
   конфигурациях (`sv_lan 1`, `sv_lan 0 + hide_server 1`, с `sv_hibernate_think 1`,
   с `sv_logblocks 1` — блокировки в лог не пишутся). TCP RCON при этом отвечает,
   CPU показывает тики. Правил Windows Firewall для пути нет, но наш Rust-процесс без правил
   на loopback отвечает — значит, причина не в firewall.
2. При `+ip 192.168.0.174` (LAN-адрес хоста, `sv_lan 1`) сервер отвечает корректно.
   Ответы сохранены в `tests/fixtures/a2s/ref_gmod_2026.04.29/`.
3. Вывод [гипотеза]: движок GMod отбрасывает connectionless-запросы с loopback-адреса
   (или с адреса, совпадающего с собственным). Важно для M2: `connect 127.0.0.1` с
   эталоном может вести себя иначе, чем с LAN-адресом.

Расшифровка эталонных ответов — `PROTOCOL.md`, `COMPATIBILITY.md`.

## Технические препятствия перед M2

1. **IDA MCP недоступен** из-за хука плагина (см. выше) → статический анализ протокола
   соединения пока только через строки/импорты.
2. **Steam-аутентификация**: публичный сервер обязан проверять тикеты через Steamworks
   GameServer API (`BeginAuthSession`). Для Rust нужен `srcds-steam` (FFI к официальному
   `steam_api64.dll`, распространяемому Valve со Steamworks SDK). Для локальных тестов
   существует штатный режим `sv_lan 1` («no authentication», строка в engine.dll) —
   это официальный режим сервера, а не обход клиента.
3. **Несовпадение сборок** клиента (261112) и эталонного DS (261104).
4. Номер сетевого протокола (не A2S) для C2S_CONNECT: строка `"Network protocol: %i"` есть,
   значение **не подтверждено**.
5. Захват handshake требует запуска оригинального клиента пользователя — нужно
   согласование (клиент в момент исследования был запущен пользователем).
