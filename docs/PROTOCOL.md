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
| `A2S_GETCHALLENGE` (`'q'`), `C2S_CONNECT` (`'k'`) | handshake клиента — M2, формат GMod **не подтверждён**; srcds-rs пока считает как `unhandled` |
| Netchannel, сообщения `net_*`/`clc_*`/`svc_*` | имена найдены в engine.dll (см. `IDA_FINDINGS.md`), форматы не исследованы |
| bzip2-сжатые split-пакеты | отвергаются |
