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
| `svc_ServerInfo` (8) | после hostname ещё две строки: `""` и имя gamemode (`sandbox`) | [факт] длина 752 бит совпадает; смысл первой строки — [гипотеза] |
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
