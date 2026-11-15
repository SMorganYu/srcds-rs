# Protocol notes

Источники: Valve Developer Wiki «Server queries» (публичная документация), чёрный ящик —
эталонный GMod DS (PatchVersion 2026.04.29, build 261104, win64). Код Valve не копировался.

Легенда: **[V]** — подтверждено эталонными пакетами и golden-тестом; **[D]** — из публичной
документации, не опровергнуто, но не проверено на GMod; **[?]** — неизвестно/гипотеза.

## Framing UDP-датаграмм

| Первые 4 байта (LE i32) | Значение | Статус |
|---|---|---|
| `-1` (`FF FF FF FF`) | connectionless | [V] |
| `-2` (`FE FF FF FF`) | фрагмент split connectionless | [V] |
| иное | netchannel (sequence) | [D] |

## Split (multi-packet) — [V]

```
i32 -2 | i32 id (bit31 = bzip2) | u8 total | u8 number | u16 size | payload
```

Эталон: `id=1`, `total=3`, `size=1248` (0x04E0), датаграммы по 1260 байт. Собранный payload
начинается с `FF FF FF FF`. srcds-rs использует те же параметры; golden-тест
`rules_split_reassemble_and_resplit_identical` проверяет **побайтовое совпадение**.

## Challenge — [V]

* Запрос A2S_INFO без challenge → `FF FF FF FF 41 <i32 challenge>` (9 байт).
* `A2S_PLAYER`/`A2S_RULES` с `-1` → тот же ответ.
* Эталон выдал одному адресу **один и тот же** challenge для всех трёх типов запросов.
* srcds-rs: challenge = SipHash(секрет, IP, эпоха 30 с), принимается текущая и предыдущая
  эпоха — без хранения состояния на адрес (защита от роста памяти при spoofing).

## A2S_INFO (`'T'` → `'I'`) — [V]

```
u8 'I' | u8 protocol=17 | str name | str map | str folder | str game | u16 appid
u8 players | u8 max | u8 bots | u8 type('d') | u8 env('w') | u8 visibility | u8 vac
str version | u8 EDF | [u16 port] [u64 steamid] [u16 tvport str tvname] [str keywords] [u64 gameid]
```

Эталон: `protocol=17`, `game="Sandbox"` (название gamemode), `version="2026.04.29"`,
`EDF=0xB1` (port, steamid, keywords, gameid), keywords `" gm:sandbox gmc:other ver:261104"`
(**с ведущим пробелом**; `ver:` = первая строка `garrysmod.ver`), `gameid=4000`.
Энкодер srcds-rs воспроизводит эталонный пакет байт-в-байт (`info_decodes_and_reencodes_byte_identical`).

Запрос: эталон принимает `"Source Engine Query\0"` + i32 challenge. Хвостовые байты
srcds-rs игнорирует [D].

## A2S_PLAYER (`'U'` → `'D'`) — [V] для пустого списка

`u8 'D' | u8 count | { u8 index | str name | i32 score | f32 duration }*`. Эталон с 0
игроков: `FF FF FF FF 44 00`. Формат записей — [D].

## A2S_RULES (`'V'` → `'E'`) — [V]

`u8 'E' | u16 count | { str name | str value }*`. Эталон: 148 правил, 3147 байт → 3 фрагмента.
Порядок: **не побайтовый** (`ttt_detective_pct` перед `ttt_det_credits_starting`),
согласуется со сравнением имён в ASCII-верхнем регистре [гипотеза, проверена на 148 записях].
`sv_password` передаётся как `0/1`.

## Не реализовано / неизвестно

| Сообщение | Статус |
|---|---|
| `A2A_PING` (`'i'`) | Valve объявила устаревшим; не отвечаем [?] |
| `A2S_SERVERQUERY_GETCHALLENGE` (`'W'`) | строка есть в engine.dll; srcds-rs отвечает challenge [D] |
| `A2S_GETCHALLENGE`/`C2S_CONNECT`, netchannel | **реализованы**, см. разделы ниже |
| bzip2-сжатые split-пакеты | отвергаются |

# Handshake и netchannel (M2) — 2026-11-14

Источник: захват оригинального клиента GMod (build 261112) ↔ копия эталонного DS
(261104) через собственный UDP-relay (`scripts/udp_relay.py`, LAN-IP), разбор
`scripts/netchan_decode.py`. Структуры сверены с публичной нумерацией сообщений Source
(Obsoletium используется только как справочник по полям, код не копируется). Метки:
**[факт]** — подтверждено захватом/тестом; **[гипотеза]** — смысл поля не установлен.

## Loopback

* [факт] Клиент GMod **не отправляет** ни одного пакета на `127.0.0.1:<port>`
  («Connection failed after 6 retries», relay на loopback пакетов не видит; ручной пакет
  через тот же relay проходит). Вместе с тем, что DS не отвечает на A2S с 127.0.0.1,
  для тестов используется LAN-адрес хоста.

## Connectionless handshake

| Шаг | Формат (после `FF FF FF FF`) | Статус |
|---|---|---|
| `A2S_GETCHALLENGE` | `'q' u32 client_challenge "0000000000\0"` | [факт] |
| `S2C_CHALLENGE` | `'A' u32 0x5A4F4933 u32 server_ch u32 client_ch i32 auth=3 u16 0 u64 server_steamid u8 secure "000000\0"` (39 байт) | [факт]; эталон без Steam-логина шлёт steamid=1 |
| `C2S_CONNECT` | `'k' i32 protocol=24 i32 auth=3 u32 server_ch u32 client_ch u32 gmod_extra str name str password str version u16 len + steam cookie` | [факт]; `gmod_extra` — [гипотеза] смысл неизвестен |
| `S2C_CONNECTION` | `'B' u32 client_ch "0000000000\0"` | [факт] |
| `S2C_CONNREJECT` | `'9' u32 client_ch str reason` | формат классический; используется для честных отказов |

Steam cookie (SteamID + тикет сессии) — учётные данные клиента: srcds-rs его **не
логирует и не сохраняет** (только длину), тикет **не проверяет** — сервер работает
как `sv_lan 1` и принимает только loopback/частные адреса.

## Заголовок netchannel

`i32 seq | i32 ack | u8 flags | u16 checksum | u16 rel_state | [u8 choked] | [u32 challenge] | биты…`

* [факт] checksum = CRC32 всех байт после поля checksum, свёрнутый `lo16 ^ hi16`
  (сошлось на 303 пакетах эталона).
* [факт] **GMod: 16 reliable-подканалов** — `rel_state` 16 бит, индекс подканала 4 бита
  (классический Source: 8 / 3).
* [факт] challenge = серверный challenge из `S2C_CHALLENGE`, присутствует всегда.
* [факт] Хвост: биты добиваются единицами до байта, число добивки ORится в биты 5..7
  flags (пересекается с флагом CHALLENGE). srcds-rs гарантирует ≥ 6 нулевых бит (NOP)
  после последнего сообщения, поэтому любая интерпретация добивки безопасна.
* `-2` — split (формат как у A2S), `-3` — `"LZSS"` + u32 размер + поток LZSS
  (клиент шлёт сжатые пакеты; srcds-rs распаковывает с лимитом).

## Reliable-фрагменты (GMod)

* [факт] `MAX_FILE_SIZE_BITS = 30`, фрагмент **4096 байт**, счётчик фрагментов **5 бит**,
  старт фрагмента 18 бит (найдено подбором: поле несжатого размера = размеру в
  заголовке LZSS).
* single-block: `0 | compressed:1 [size:30] | varint bytes`.
* Подтверждение: отправитель переворачивает бит *i* при отправке подканала *i*;
  получатель переворачивает свой бит после полного чтения и эхо-шлёт состояние.
  Совпадение битов = ACK; если пир подтвердил более поздний seq без совпадения — повтор.

## Сообщения (тип 6 бит) — отличия GMod

| Сообщение | Отличие | Статус |
|---|---|---|
| `svc_ServerInfo` (8) | после hostname ещё две строки (`""`, имя gamemode) **и хвост**: клиент дочитывает ≥11 бит после строки gamemode (10 и меньше → переполнение чтения, клиент бросает signon). srcds-rs шлёт 16 нулевых бит | [факт] порог 11 бит измерен живым клиентом; у эталона эти биты были единицами; смысл — [гипотеза] |
| `svc_CreateStringTable` (12) | вместо u16 max: `log2(max)` в **5 битах**, затем entries в `log2+1` бит, varint длина | [факт] все 19 таблиц эталона сходятся по границам; `downloadables` srcds-rs **побитово равен** эталону (335 бит) |
| user data в таблицах | длина **19 бит** (классика 14) | [факт] |
| `player_info` (`userinfo`) | 324 байта: name[128], userID, guid[33], friendsID, friendsName[128], fake, hltv, customFiles[4], downloaded | [факт] |
| `net_File` (2) | `u32 id`, 2 флага, CRC32 вместо имени для user_custom | [факт] раскладка, смысл флагов — [гипотеза] |
| `svc_ClassInfo` (10) | эталон: 250 классов, `create_on_client=1`, SendTables не шлются | [факт] |
| `clc` 18 | `clc_GMod_ClientToServer`: u20 длина в битах + данные | [факт] |
| `svc` 33 | `svc_GMod_ServerToClient`: u20 длина + данные; в signon один байт `0x03` | [факт] формат, [гипотеза] смысл |
| `client_lua_files` | entry 0 `paths`; файлы с SHA-256(содержимое + NUL) | [факт] хэш проверен |
| MD5 карты в ServerInfo | MD5 всех lump'ов кроме entities | [факт] gm_construct → `3b5c56cf…` |

## Последовательность signon эталона

1. C: `SetConVar` + `SignonState(2)`.
2. S: `Print` + `ServerInfo`; затем `Tick`, 19 `CreateStringTable`, ~28
   `UpdateStringTable(instancebaseline)`, `svc 33`, `SetConVar`, `SignonState(3)`.
3. C: `ClientInfo` + `clc 18` + `SignonState(3)`.
4. S: `net_File` (запрос спрея), `ClassInfo(250, create)`, `BSPDecal`×19, `VoiceInit`,
   `GameEventList(84)`, `SignonState(4)`.
5. C: `SignonState(4)` + `ListenEvents` (+ загрузка спрея в file-stream).
6. S: `SetView`, `Tick`, `SignonState(5)`; C: `SignonState(5)`.
7. S: UserMessage'ы, `userinfo`, первый `PacketEntities` (70 сущностей), `FixAngle`.

# Сущности, таблицы классов и ввод (M4) — 2026-11-14

Источники: дамп таблиц данных, который клиент сам пишет в каждое демо (`record`,
кадр `dem_datatables`; разбор — `scripts/dem_datatables.py`), демо с эталонного DS
(`scripts/dem_entities.py`, `scripts/dem_stringtables.py`), трафик клиента к srcds-rs.
Бинарники клиента не дизассемблировались.

| Что | Формат | Статус |
|---|---|---|
| Описание SendTable | имя, 10 бит число свойств; на свойство: 5 бит тип, имя, 16 бит флаги, затем имя подтаблицы (DataTable) / число элементов (Array) / low, high (float) + 7 бит ширина | [факт] 311 таблиц / 250 классов клиента разобраны бит в бит (900102 из 900104 бит, остаток — выравнивание) |
| Свои таблицы сервера | `svc_SendTable` (тип 9: 1 бит needs_decoder, 16 бит длина) для каждой таблицы, затем `svc_ClassInfo` с `create_on_client=0`; клиент связывает таблицы и свойства **по именам**, кодирование берёт из описания сервера | [факт] клиент принял подмножество (CWorld, CGMOD_Player, CPlayerResource), дошёл до FULL |
| `svc_ServerInfo.max_classes` | должно равняться числу классов в `svc_ClassInfo` (ширина id класса = log2(N)+1) | [гипотеза по SDK], работает |
| Instance baseline | запись `instancebaseline` с ключом id класса обязательна для каждого класса; srcds-rs шлёт пустой список свойств (1 байт `00`) | работает |
| Порядок свойств (flatten) | коллапсируемые базовые классы — inline, прочие подтаблицы — дочерние узлы (их свойства раньше собственных), затем `SPROP_CHANGES_OFTEN` в начало (обмен, не стабильно) | [факт] индексы эталона (m_nTickBase=3, m_flSimulationTime=11) совпали |
| `svc_PacketEntities` | max 13 бит, delta 1 (+32 from), baseline 1, updated 13, **длина 24 бита** (не 20), update_baseline 1 | [факт] с 24 все 39 пакетов эталона декодируются ровно в заявленную длину, с 20 — ни один |
| Заголовок сущности | `ubitvar(index − prev − 1)` (2 бита селектор, затем 4/8/12/32 бита), leave, enter/delete; enter: id класса, 10 бит serial | [факт] |
| Индекс свойства | бит «ещё» + `ubitvar(Δ−1)`, список завершается битом 0 | [факт] |
| `m_flSimulationTime` | тик − база; база шагает по 100 тикам со сдвигом `index % 32` | [факт] по значениям эталона |
| `m_fFlags` | у GMod `FL_CLIENT = 1<<8` (baseline игрока эталона = 256, на земле 257) | [факт] |
| `client_lua_files` | пути **в нижнем регистре**; ядро (`lua/includes/**`, `lua/derma/**`) тоже нужно объявить, иначе клиент не находит `includes/init.lua` | [факт] живой тест; почему эталон их не объявляет — [гипотеза] другой механизм |
| `client_lua_files` в демо | userdata = SHA-256 + LZMA-сжатое содержимое (в живом signon — только SHA-256) | [факт] |
| usercmd (`clc_Move`) GMod | стандартные поля с битами изменения (command, tick, 3 угла, fwd/side/up, buttons 32, impulse 8, weapon 13(+6), mouse dx/dy 16), затем хвост GMod: `[изм][u8 код клавиши]*0` (W=33, D=14, TAB=67, MOUSE_LEFT=107), `[изм][i8]` колесо мыши, `[бит]` world clicking (зажата C, не дельта), `[изм][48 бит]`, `[изм][3×f32]`, `[изм][3×f32]` (данные world clicking, смысл не установлен), `[изм]` шестого поля (не встречалось) | [факт] 16 261 из 16 261 захваченных сообщений декодируются ровно; смысл трёх полей — [гипотеза] |

## Движение игрока (M4) — сверено с предсказанием клиента

Инструмент: штатная клиентская переменная `cl_showerror 2` печатает каждое поле, где
состояние сервера расходится с предсказанием клиента.

| Что | Значение | Статус |
|---|---|---|
| Значения GMod по умолчанию | `sv_friction 8`, `sv_stopspeed 10`, `sv_accelerate 10`, `sv_airaccelerate 10`, `sv_gravity 600`, `sv_maxspeed 10000`, `sv_noclipspeed 5`, `sv_noclipaccelerate 5`, `sv_maxvelocity 3500` | [факт] прочитаны в консоли клиента после отключения |
| Скорости sandbox | ходьба 200, бег 400, медленно 100, присед ×0.3, DuckSpeed/UnDuckSpeed 0.1 с, прыжок 200 | [факт] `player_sandbox.lua`, `player_default.lua` |
| `m_flMaxspeed` | клиент предсказывает лимит текущих кнопок (200 при ходьбе) | [факт] `net 400 pred 200` |
| `m_bSprintEnabled` (DT_GMOD_Player) | без него клиент не разгоняется до бега | [факт] постоянная разница 60 ед/с за тик |
| Прыжок | половина гравитации до прыжка, `vz += JumpPower`, половина внутри прыжка, половина в конце: 200 − 3·4.5 = 186.5 | [факт] клиент предсказал 186.5 |
| Порядок шагов | как в открытом SDK 2013: CategorizePosition только при vz > 250 в начале, StartGravity всегда, SetGroundEntity обнуляет vz, StepMove без проверки fraction, quarter-box проверка земли, `m_surfaceFriction` 0.25 при подъёме в воздухе | после правок расхождений по скорости/позиции/флагам не осталось |
| Присед | алгоритм `Duck()` SDK (мультиплеер, без duck-jump), хулл 36, глаза 28; флаги `FL_DUCKING` (2) и `FL_ANIMDUCKING` (4) | [факт] клиент предсказал флаги 262; поведение `FL_ANIMDUCKING` — [гипотеза] |
| Displacement (рельеф) | сетка `(2^power+1)²` от угла грани, ближайшего к `startPosition`, + вектор·dist; диагональ ячейки чередуется по чётности индекса вершины; треугольник односторонний: плоскость лица + 6 осевых + до 9 «ребро × ось», «кожа» 1/32; флаг `minTess` (старший бит) `0x4` = без коллизии хулла | [факт] сброс игрока на 3 точки рельефа gm_construct: клиент и сервер приземляют одинаково (расхождений после касания нет) |
| Встряска при приземлении | при скорости падения ≥ 303 `m_vecPunchAngle.z = 0.013·v`, затем пружина (затухание 9, жёсткость 65) | [факт] клиент предсказал 5.616 при падении ~432 ед/с |

# GLua, net и пользовательские сообщения (M6) — 2026-11-14

Источники: [факт] — по эталонному `server.dll` (только интерфейс: ширины полей и порядок,
код не переносился), по записанной демке эталона или по пакетам оригинального клиента.

| Что | Формат | Статус |
|---|---|---|
| GMod-пакет (`svc` 33 / `clc` 18) | u20 длина в битах + данные; первый байт данных — тип | [факт] |
| Тип 0: Lua `net` | u8 0, u16 id из таблицы `networkstring`, тело сообщения | [факт] `net.Start` пишет 8 бит 0 и 16 бит id |
| Тип 2 (клиент → сервер) | текст Lua-ошибки клиента | [факт] |
| Тип 5 (сервер → клиент) | Lua-код для выполнения, строка с NUL (`SendLua`/`BroadcastLua`), буфер 6144 | [факт] |
| Входящее net-сообщение | в Lua вызывается `net.Incoming(len, ply)`, `len` = биты после байта типа | [факт] |
| `net.WriteVector` / `WriteAngle` | BitVec3Coord: 3 флага наличия, затем для каждой ненулевой компоненты: флаг целой, флаг дробной, знак, (целая−1) 14 бит, дробь 5 бит (1/32) | [факт] |
| `net.WriteNormal` | флаги x, y; для каждой — знак + 11 бит `|v|·2047`; знак z | [факт] |
| `net.WriteString` | байты до NUL включительно | [факт] |
| `net.WriteFloat/Double` | сырые 32/64 бита IEEE | [факт] |
| `svc_UserMessage` (23) | u8 тип, u11 длина в битах, данные | [факт] демка эталона разбирается ровно |
| Индексы user messages | порядок регистрации: 0 Geiger, 1 Train, 2 HudText, 3 SayText, 4 TextMsg, 5 HudMsg, 6 ResetHUD, … 13 Battery | [факт] размеры Train/ResetHUD/Battery совпадают с демкой |
| `SayText` (чат) | u8 индекс отправителя, текст, u8 1, u8 team, u8 dead | [факт] |
| `TextMsg` | u8 назначение (1 notify, 2 console, 3 chat, 4 center), текст, 4 строки-параметра | [факт] `ChatPrint` = назначение 3 |
| `svc_UpdateStringTable` | первая запись всегда с явным индексом (приёмник начинает с «до записи 0») | [факт] исправлено в srcds-rs |
| `DPT_Array` в описании таблицы | шаблон элемента (флаг `INSIDEARRAY`) перед массивом; у массива 10 бит числа элементов; значение = длина `floor(log2 n)+1` бит + элементы | реализовано (`m_hViewModel`) |
| usercmd GMod: флаг после 48-битного поля | обычный бит (без бита изменения), стоит 1 в каждой команде, пока открыт чат | [факт] 40/40 пакетов разбираются ровно; смысл «печатает» — [гипотеза] |
| usercmd GMod: 48-битное поле при world clicking | направление курсора: три i16 = единичный вектор ×1000 (x, y, z) | [факт] захват с курсором в центре: (−957, −289, 20) = направление взгляда pitch −1.15°, yaw −163.19°; используется в `GetAimVector`, физгане |
| Список зажатых клавиш usercmd | изменения → хуки `PlayerButtonDown` / `PlayerButtonUp` (код клавиши как есть, `KEY_PAD_1` = 38); sandbox передаёт их в `numpad` | реализовано; без этого не работали трастер, камера, лампа |
| Пустые слоты `m_hMyWeapons` | `INVALID_EHANDLE` (23 бита единиц); 0 = мир (индекс 0, serial 0) | [факт] живой клиент завис после смерти с `m_hMyWeapons - EHandles differ (net) 0x… (pred) 0`; [факт] замена 0 на `INVALID_EHANDLE` во *всех* незаданных EHANDLE роняла клиент при входе (виновное поле не установлено) |
| Спринт / медленный шаг | `m_flMaxspeed` меняется только при начале/конце спринта (IN_SPEED) или шага (IN_WALK), в том числе в noclip | [факт] ошибки предсказания `m_fIsSprinting net false pred true` в noclip и `m_flMaxspeed net 400 pred 200` после него |
| Полное обновление сущностей и дельта-база | полное (не дельта) `svc_PacketEntities` заставляет клиента выбросить все более старые кадры; сервер держит в пути одно полное обновление и не строит дельту от тика старше последнего полного | [факт] свежезапущенный клиент грузится долго: сервер слал полное обновление каждый тик, поздний отчёт клиента о тике 2983 пришёл после того, как клиент разобрал полные 2984–3002 → `Update delta not found.` → крэш движка (engine.dll → tier0.dll, чтение `0xFFFF…`) |
| NetworkVar (DTVar) | `DT_BaseEntity`: таблицы `m_GMOD_bool` (1 бит), `m_GMOD_float`, `m_GMOD_int` (32 бита), `m_GMOD_Vector`, `m_GMOD_QAngle`, `m_GMOD_EHANDLE` (23 бита) по 32 слота + строки `m_GMOD_String0..3` | [факт] эталонные send tables; без них клиент не видит NetworkVar SENT'ов (слежение камеры, вкл/выкл ламп, трастеры) — реализовано |
| Руки и цвета игрока | `DT_GMOD_Player`: `m_Hands` (EHANDLE `gmod_hands`), `m_PlayerColor`, `m_WeaponColor` | [факт] эталон; без `m_Hands` клиент не рисует руки на view model |
| Анимация игрока | `DT_BaseAnimating.m_bClientSideAnimation` = 1 у игроков; модель игрока = выбранная Lua (`player_manager`), пока клиент не подтвердил precache — модель по умолчанию | [факт] без этого клиент рисовал модель в исходной позе, а модель была всегда kleiner |
| Последовательности анимации | из `.mdl`: число/смещение последовательностей в заголовке 188/192, описание 212 байт (имя +4, активность +8), include-модели 336/340; порядок клиента — свои, затем include без повторов имён [гипотеза] | [факт] у реальных моделей последовательность 0 = `ragdoll`: без `m_nSequence` NPC стояли в позе рэгдолла; NPC теперь получают `ACT_IDLE` (у гражданина — 1), анимирует клиент |
| Брашевые энтити карты | `func_brush`, `func_wall`, `func_illusionary`, `func_button` и т.п. — класс `CBaseEntity` (`DT_BaseEntity`), модель `*N`; коллизия — BSP-дерево подмодели (`dmodel_t.headnode`), смещённое на origin | [факт] эталон: отдельного класса нет; на gm_construct 10 таких (6 `func_brush`, 2 `func_illusionary`, кнопка, зеркало) — раньше не передавались вовсе |
| `m_bWearingSuit` (`DT_Local`) | 1 | [факт] без него HL2-HUD (здоровье, патроны, выбор оружия) скрыт |
| Прыжковый буст sandbox | `player_sandbox` StartMove/FinishMove: при прыжке с земли добавляется скорость по yaw (½ forwardmove, ⅒ в приседе), горизонтальная скорость ≤ maxspeed·(1+доля) | реализовано в движении сервера (клиент выполняет это в предсказании) |
| Noclip: fall velocity и земля | берутся до перемещения; IN_DUCK в noclip не считается «удержанным» | [факт] `cl_showerror 2`: значения клиента отставали ровно на тик; присед в воздухе сразу после выхода из noclip |
| Лестница HL2 (`func_useableladder`) | клиентский класс `CFuncLadder` (`DT_FuncLadder`: `m_vecPlayerMountPositionTop/Bottom`, `m_vecLadderDir`, `m_bFakeLadder`), у игрока `DT_HL2Local.m_hLadder`; конвары `sv_ladder_useonly` (по умолчанию выкл.), `sv_ladderautomountdot` = 0.4 (авто-залезание, если смотреть на лестницу) | [факт] эталонные таблицы и server.dll; движение по лестнице не реализовано — на gm_construct одна такая лестница и 4 `info_ladder_dismount`. [факт] поведение (server.dll, для своей реализации): поиск — ближайшая включённая лестница в пределах 64 по ближайшей точке отрезка низ–верх, видимость проверяется трассой коробки 8×8×8 от origin+5, иначе хуллом; залезание — без E при forwardmove>0, если угол к точке <15° (или ближе 4) и ближе 32, либо |dot(ось, взгляд)|>0.4, либо с конца лестницы ближе 16 при dot>0.4; с E — dot ≥ 0; подход к точке — принудительное перемещение, линейная интерполяция по времени длительностью расстояние/скорость_лазания (MOVETYPE_NONE на время, затем MOVETYPE_LADDER и `m_hLadder`); на лестнице — скорость вдоль оси = скорость лазания игрока, знак вниз, если dot(ось, направление ввода с учётом наклона) < −0.3 (лестницы ≥64 в высоту), ввод — только кнопки W/S/A/D; отход дальше 36 от оси — слезание; прыжок — слезание со скоростью вдоль взгляда (+50 вверх, если взгляд не вниз); слезание на концах через `info_ladder_dismount` в пределах 60 |
| Serial игрока | serial сетевой сущности игрока = serial его Lua-сущности (из неё строятся все handle на игрока) | [факт] у них были разные serial: клиент не считал оружие своим — не предсказывал выстрелы (нет звука, отверстий, расхода патронов), рисовал world-модель оружия под игроком |
| Типы патронов | id = позиция в списке движка + 1: AR2 1, AR2AltFire 2, Pistol 3, SMG1 4, 357 5, XBowBolt 6, Buckshot 7, RPG_Round 8, SMG1_Grenade 9, Grenade 10, slam 11, … (37 встроенных), затем `game.AddAmmoType` | [факт] server.dll: конструктор CAmmoDef начинает счёт с 1 |
| Патроны | `DT_LocalWeaponData.m_iPrimaryAmmoType/m_iSecondaryAmmoType`, `DT_LocalPlayerExclusive.m_iAmmo[id]` (16 бит); обойма при выдаче = `default_clip`, а если его нет в скрипте — `clip_size` | [факт] `scripts/weapons/weapon_pistol.txt`: только `clip_size` 18 |
| Оружие: модель и предсказание | у оружия в руках игрока `m_nModelIndex` = view-модель (world-модель — в `m_iWorldModelIndex`): клиент берёт из неё последовательности и их длительность | [факт] с world-моделью клиент считал длительность idle = 0 и запускал idle каждый тик (`m_flTimeWeaponIdle pred = curtime`, `m_nSequence pred 0`) |
| Времена оружия | `DT_LocalActiveWeaponData`: `m_flNextPrimaryAttack`, `m_flNextSecondaryAttack`, `m_flTimeWeaponIdle` — double как `DPT_VectorXY` NOSCALE (младшее, старшее слово); поля в `CBaseCombatWeapon` по смещениям 0x1728/0x1730/0x1738 | [факт] server.dll (SendProp и прокси, datadesc) |
| Логика оружия по командам | на каждую команду в её время (tickbase × интервал): конец перезарядки → обойма; вторичная/основная атака по времени; R → перезарядка (следующая атака через длительность анимации); без кнопок атаки и после idle-времени → `ACT_VM_IDLE`; каждая анимация: последовательность по активности из view-модели, parity+1, idle = сейчас + длительность | своя реализация правил базового оружия; длительность последовательности = (кадры − 1) / fps её анимации |
| Длительность последовательностей | `.mdl`: у seqdesc `animindexindex` (+60) → short-индекс анимации; описания анимаций по 100 байт с `localanimindex` (+184): fps +8, кадры +16 | [факт] c_pistol: выстрел 0.4 с, idle 3.33 с; c_toolgun выстрел 0.63 с |
| Спавн игрока в Lua | при `SIGNONSTATE_FULL` и с user-info клиента | [факт] sandbox создаёт `cl_playercolor`/`cl_weaponcolor`/`cl_playerskin` в клиентском Lua: при раннем спавне `GetInfo` пуст — игрок чёрный, модель по умолчанию |
| Физган | луч — отдельная сущность `CPhysBeam` (`DT_PhysBeam`: `m_hTargetEnt`, `m_HoldPos`, `m_bIsOn`, `m_iPhysBone`), у физгана `m_hPhysBeam`; захват — пока зажата кнопка и луч касается объекта | [факт] эталонные таблицы; `cl_showerror`: клиент предсказывал захват (`m_hGrabbedEntity`), которого не было на сервере; смысл `m_HoldPos` — [гипотеза] |
| Гравипушка | `DT_WeaponPhysCannon`: `m_bActive`, `m_hAttachedObject`, `m_EffectState` (4 бита), `m_bOpen`, `m_bPhyscannonState`; в покое `m_EffectState` = 2 | [факт] эталон и `cl_showerror` (`net 0 pred 2`) |
| Гравипушка: кадр | без захвата: клешни открываются (анимация idle, `m_bOpen` 1, эффект 2) на подбираемый объект в `physcannon_tracelength` (250), закрываются (эффект 1) через 0.5 с после потери цели; ПКМ (с антидребезгом после захвата) — следующая вторичная через 0.1 с при «не найдено», эффект 3; без ПКМ эффект 2; ЛКМ при `t ≥ next primary`: next primary = t + 0.5, hull-трасса ±8 на 250 — объект → пинок (эффект 4, `ACT_VM_SECONDARYATTACK`), иначе холостой (`ACT_VM_PRIMARYATTACK`); иначе idle (`ACT_VM_RELOAD` при удержании объекта, иначе `ACT_VM_IDLE`); `physcannon_maxforce` 1500, `physcannon_maxmass` 250 | [факт] server.dll `CWeaponPhysCannon` (ItemPostFrame, Primary/SecondaryAttack, WeaponIdle, Open/CloseElements, конвары); сила пинка как изменение скорости — [гипотеза]; удержание объекта не реализовано |
| Базовое оружие: кадр | порядок: CheckReload (обойма после перезарядки) → ПКМ → ЛКМ при `t ≥ next primary` (пустая обойма → «огонь вхолостую»: первый раз только отметка, далее перезарядка; свежее нажатие или отпускание ПКМ → next primary = t) → R при `t ≥ next primary` → без кнопок: пустая обойма с запасом сама перезаряжается, иначе idle | [факт] server.dll `CBaseCombatWeapon::ItemPostFrame` |
| Пулемётная база (`CHL2MPMachineGun`) | `m_nShotsFired` сбрасывается без ЛКМ; выстрел: +1 счётчик, вспышка, столько выстрелов, сколько темп укладывает до t (`next primary += rate`), обойма −= выстрелы, анимация по активности оружия; `DT_HL2MPMachineGun.m_nShotsFired` | [факт] server.dll, эталонные таблицы |
| Пистолет | темп 0.5 с; выстрелы в пределах 0.5 с считаются (`m_nNumShotsFired`): анимация `PRIMARYATTACK` / `RECOIL1-3`; после выстрела «не раньше» = t + 0.1, штраф точности +0.2 (спадает по кадру без ЛКМ, 0..1.5); ПКМ → все времена t + 0.1; пустая обойма → `DRYFIRE`, next primary = t + длительность; без ЛКМ и после «не раньше» → next primary = t − 0.1 (стрельба с темпом кликов); idle `ACT_VM_IDLE_EMPTY` без патронов; R обнуляет штраф. `DT_WeaponPistol`: `m_flSoonestPrimaryAttack`, `m_flLastAttackTime` (double), `m_flAccuracyPenalty`, `m_nNumShotsFired` | [факт] server.dll `CWeaponPistol`, эталонные таблицы; отдача камеры (`ViewPunch` со shared random) не реализована |
| SMG1 | темп 0.075 с; анимация по числу выстрелов очереди: < 2 `PRIMARYATTACK`, 2 `RECOIL1`, ≥ 3 `RECOIL3`; ПКМ (подствольная граната, нужен запас `SMG1_Grenade`): прерывает перезарядку, `SECONDARYATTACK`, −1 граната, next primary +0.5, next secondary +1.0; перезарядка не трогает next secondary | [факт] server.dll `CWeaponSMG1`; сама граната не реализована |
| AR2 | темп 0.1 с; < 2 `PRIMARYATTACK`, 2 `RECOIL1`, 3 `RECOIL2`, ≥ 4 `RECOIL3`; ПКМ: заряд (`FIDGET`), все времена +0.5, через 0.5 с выстрел (`SECONDARYATTACK`, −1 `AR2AltFire`, времена +1.0); во время заряда перезарядки нет; поля заряда не передаются по сети | [факт] server.dll `CWeaponAR2`; энергетический шар не реализован |
| 357 | базовый кадр; выстрел: `PRIMARYATTACK`, обе атаки через 0.75 с, −1 патрон | [факт] server.dll `CWeapon357` |
| Дробовик | своя машина состояний: после выстрела нужен затвор (`ACT_SHOTGUN_PUMP`, по окончании анимации выстрела); перезарядка по одному патрону (`ACT_SHOTGUN_RELOAD_START` → `ACT_VM_RELOAD` на патрон → `ACT_SHOTGUN_RELOAD_FINISH`); ЛКМ/ПКМ во время перезарядки — выстрел после текущего патрона (`m_bDelayedFire1/2`); R до затвора — перезарядка после него (`m_bDelayedReload`); ПКМ — дуплет (2 патрона, `SECONDARYATTACK`); 7 / 12 дробин; пустой без запаса — `DRYFIRE`. `DT_WeaponShotgun`: `m_bNeedPump`, `m_bDelayedFire1`, `m_bDelayedFire2`, `m_bDelayedReload` | [факт] server.dll `CWeaponShotgun`, эталонные имена полей |
| Арбалет | выстрел: −1 болт, `PRIMARYATTACK`, обе атаки через 0.75 с, `m_bMustReload`; idle-время ставится по «последовательности 181» (номер активности как индекс последовательности) — перезарядка почти сразу; ПКМ — прицел (`m_bInZoom`) | [факт] server.dll `CWeaponCrossbow`; болт как снаряд и FOV прицела не реализованы ([гипотеза] несуществующая последовательность = 0.1 с) |
| Монтировка | пока зажата ЛКМ — удар каждые 0.4 с (`HITCENTER` / `MISSCENTER`), next secondary = конец анимации; иначе idle | [факт] server.dll `CWeaponCrowbar` |
| Гравипушка: захват | ПКМ ищет вдоль взгляда на 4 × 250: объект ≤ 250 и подбираемый (масса ≤ 250, `GravGunPickupAllowed`) — захват: `m_hAttachedObject`, эффект 3, `GravGunOnPickedUp`, `PRIMARYATTACK`, next secondary +0.5, антидребезг ПКМ до отпускания; дальше — притягивание (`physcannon_pullforce` 4000, лёгкие < 50 слабее); с объектом: ПКМ (нажатие) — отпустить (эффект 2, `GravGunOnDropped`), ЛКМ — бросок (эффект 4, `SECONDARYATTACK`); idle с объектом — `ACT_VM_RELOAD`; при смене оружия и смерти объект отпускается | [факт] server.dll `FindObject`/`AttachObject`/`DetachObject`, конвары; дистанция удержания (радиус + 24) и сила броска/притягивания как скорость — [гипотеза] |
| Снаряды HL2 | болт арбалета (3500 u/s, 1500 под водой) и граната SMG1 (1000 u/s) — отдельные сущности, сервер двигает их каждый тик трассой; болт ранит цель или застревает, граната взрывается при касании | [факт] скорости из server.dll; урон, радиус, гравитация болта — [гипотеза]; клиентские классы `CCrossbowBolt`/`CGrenadeAR2` не используются (показ как `CDynamicProp`) |
| Анимация draw | idle-время = длительность / скорость развёртывания (`sv_defaultdeployspeed` 4), view-модель играет с `m_flPlaybackRate` 4 | [факт] server.dll `SendWeaponAnim`, конвар |
| Вспышка выстрела | `m_nMuzzleFlashParity` (2 бита) в `DT_BaseAnimating` и `DT_BaseViewModel` | [факт] эталонные таблицы |
| `svc_SetView` | `Player:SetViewEntity` → индекс сущности камеры (свой индекс = обычный вид) | реализовано (камера sandbox) |
| Верёвка (`CRopeKeyframe`) | клиент разрешает материал и концы один раз при создании: верёвка ждёт подтверждения precache материала и сетевой отправки обеих концевых сущностей | [факт] иначе первая верёвка лампы шла в (0,0,0) и была без текстуры |
| Конвары gamemode `settings` (`sandbox.txt`) | движок создаёт конвары (`sbox_max*` и т.д.) из блока `settings` | [факт] без них `gm_spawn` упирается в лимит |
| Базовый gamemode | из ключа `base` в `<gm>.txt`, загружается раньше производного | [факт] |

Классы сущностей srcds-rs (свои таблицы, клиент связывает по имени):
`CPhysicsProp`/`DT_PhysicsProp`, `CDynamicProp`/`DT_DynamicProp`, `CSENT_anim`/`DT_SENT_anim`
(`m_ScriptedEntity.m_strScriptName` = Lua-класс), `CPredictedViewModel`, оружие
`CWeaponPhysGun`, HL2 (`CWeaponPistol` …), `CWeaponSWEP` (скриптовое). Модели оружия —
из `scripts/weapons/<class>.txt` (`viewmodel`, `playermodel`), читаются из VPK.
## Движение и ввод: отличия GMod от стока (M6, сверено с живым клиентом и эталонным server.dll)

| Что | GMod | Статус |
|---|---|---|
| Проверка земли (`CategorizePosition`) | трасса от ног +2 до ног −1 (сток: от ног до −2) | [факт] server.dll; со стоковой сервер приземлялся на тик раньше клиента |
| `sv_sticktoground` | конвар (по умолчанию 1, replicated): не отрывать от земли при vz > 140 | [факт] server.dll; без него «взлёт» на склонах |
| Noclip: переключение | клиент шлёт `impulse 154` в usercmd и сам предсказывает смену movetype; сервер спрашивает хук `PlayerNoClip` | [факт] лог сервера при нажатии V |
| Noclip: движение | направление = норм(вперёд·знак fwd) + норм(вправо·знак side) + вверх при IN_JUMP; скорость = `sv_noclipspeed`·100, ×3 с IN_SPEED, ×0.1 с IN_DUCK; без ускорения | [факт] server.dll |
| Фонарик | `impulse 100`, хук `PlayerSwitchFlashlight`, `EF_DIMLIGHT` | живой клиент: работает |
| Спринт | клиент не бежит (и играет звук отказа), если `m_HL2Local.m_flSuitPower` < 10; `m_fIsSprinting` = IN_SPEED | [факт] живой клиент |
| `FL_ANIMDUCKING` | повторяет зажатую клавишу приседа (снимается сразу при отпускании) | [факт] предсказание клиента; живой клиент: ошибок нет |
| Список зажатых клавиш usercmd | не более 5, при 5 элементах нет терминатора 0 | [факт] 32/32 пакета |
| `CheckFalling` | удар камеры только при скорости падения ≥ 303 | [факт] server.dll |