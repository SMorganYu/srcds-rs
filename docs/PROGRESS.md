# SRCDS-RS Progress

## Completed
- Phase 0: окружение, инвентаризация бинарников (SHA-256, PE/ELF, зависимости), версии
  клиента/DS, карта подсистем — `RESEARCH.md`, `INVENTORY.md`.
- Изолированная копия эталонного DS (`D:\!gmod\srcds-ref`, hard-link'и, без аддонов).
- Эталонные A2S-фикстуры GMod DS 2026.04.29/261104 (`tests/fixtures/a2s/ref_gmod_2026.04.29`).
- Cargo workspace: `srcds-protocol`, `srcds-net`, `srcds-core`, `srcds-server`, `packet-diff`.
- M0: исполняемый `srcds-rs` (CLI, tracing, UDP, тик-цикл 66.67 Гц, метрики, Ctrl+C, `quit`).
- M1 (протокол): challenge, A2S_INFO/PLAYER/RULES, split; rate limiting.

- M2: handshake + netchannel + начало signon; **оригинальный клиент GMod подключается**
  (`Connected to 192.168.0.174:…`, ServerInfo принят, чистый disconnect с причиной).

- M3: **оригинальный клиент входит в карту** (SignonState FULL, gm_construct, без краша).

- M4 (часть): свои таблицы классов, сущности мира/игрока/player resource, спавн в `info_player_start`, клиентский Lua sandbox грузится, usercmd GMod декодируется, noclip-движение на сервере.

- M4: ходьба/присед/прыжок по BSP и displacement-рельефу (сверено с предсказанием клиента), пинг, `status`/`ping`.

- M6 (часть): **серверный GLua** — LuaJIT (mlua), диалект GMod (`!=`, `continue`, `//`…),
  порядок загрузки GMod; `lua/includes` + autorun + base/sandbox (+ энтити/оружие)
  грузятся без ошибок. Entity/Player/Weapon/PhysObj API, `ents`/`player`/`util.Trace*`,
  хуки (`PlayerInitialSpawn`, `PlayerSpawn`, `PlayerLoadout`, `PlayerSelectSpawn`,
  `PlayerSay`, `PlayerDeath`, `PlayerDeathThink`…), `concommand` (`gm_spawn` и т.д.),
  конвары, таймеры, `net`, `SendLua`/`BroadcastLua`, `ChatPrint`/`PrintMessage`, чат.
  Пропы: динамический `modelprecache`, `CPhysicsProp`, своя простая физика.
  Оружие: сетевые классы HL2/physgun/`CWeaponSWEP`, viewmodel, переключение, физган,
  hitscan-урон HL2-оружия (приблизительный), смерть и респавн.

## In progress
- M6: живая проверка в оригинальном клиенте (пропы, оружие, физган, тулган, чат).

## Verified tests
- 143 теста (unit, golden против эталона, интеграционные с реальным процессом, e2e handshake, e2e sandbox на реальной gm_construct) — `TEST_RESULTS.md`.
- Энкодер побайтово воспроизводит эталонные INFO/PLAYER/RULES/split.

## Known limitations
- SteamID в `player_info` — заявленный клиентом (в LAN не проверяется).
- Физика пропов — своя упрощённая (AABB, без вращения при контакте, без констрейнтов), не VPhysics.
- NPC: без AI (показываются как статичные модели `CDynamicProp`); рэгдоллы — статичная поза.
- Урон HL2-оружия: приблизительные значения/темп [гипотеза], без `CTakeDamageInfo`/хуков урона.
- Констрейнты (weld/rope и т.д.) тулгана: Lua создаёт энтити, физического эффекта нет.
- `sql`, `game.AddParticles`, `game.AddDecal`, `file.Open` — заглушки.
- `game`/`keywords`/rules отражают реальное (минимальное) состояние, а не Sandbox.
- Нет Steam GameServer интеграции (нет SteamID в A2S_INFO).
- Не проверено браузером серверов оригинального клиента.
- Linux-сборка не проверялась.


## Technical findings
- GMod DS не отвечает на A2S с 127.0.0.1; отвечает на LAN-IP.
- A2S protocol byte = 17; keywords `" gm:<gm> gmc:<cat> ver:<garrysmod.ver>"`.
- Rules упорядочены по ASCII-uppercase имени; split по 1248 байт.
- Клиент (261112) и эталонный DS (261104) — разные сборки при одном PatchVersion.
- engine.dll содержит GMod-сообщения `clc_GMod_ClientToServer` / `svc_GMod_ServerToClient`.

- [факт] Клиент GMod не шлёт пакеты на 127.0.0.1.
- [факт] GMod netchannel: 16 подканалов, фрагмент 4096, MAX_FILE_SIZE_BITS 30, userdata 19 бит.
- [факт] svc_ServerInfo GMod: после gamemode ещё ≥11 бит хвоста (иначе клиент бросает signon).

## Next tasks
- См. `ROADMAP.md` §«Ближайшие задачи».
