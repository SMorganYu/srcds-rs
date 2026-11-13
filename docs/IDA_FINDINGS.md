# Binary analysis findings

## IDA MCP status

* Плагин `ida-mcp` обнаружен; инструменты: `list_databases`, `open_database`,
  `execute_python`, `reference`, `close_database`, `save_database`. [факт]
* Вызов `list_databases` дважды отклонён средой: *«a hook changed this call's input after
  auto mode reviewed it»* — хук плагина добавляет `_meta.claude_session_path`, и auto-mode не
  может проверить вызов. **Ни одна база IDA не открывалась, ни один бинарник не
  анализировался в IDA.** [факт]
* Как разблокировать: выключить этот хук/плагин для сессии или выйти из auto-mode и
  одобрять вызовы IDA вручную.

## Анализ без IDA (собственный парсер PE/ELF + поиск строк)

### engine.dll (эталонный DS, win64)

| Поле | Значение |
|---|---|
| Файл | `D:\steammd\steamapps\common\billytest\bin\win64\engine.dll` |
| SHA-256 | `4525b25701dd14eb7c2d4ea49a559797a41bbc07e8d73f631c711f1b809dbdba` |
| Архитектура | PE32+ x86-64 |
| Версия | GMod PatchVersion 2026.04.29, build 261104 (по файлам установки) |
| Импорты | USER32, ADVAPI32, GDI32, WS2_32, WINMM, WININET, steam_api64, tier0, RPCRT4, vstdlib, KERNEL32 |

Найденные строки (факт наличия; назначение — **гипотеза** по именам, уверенность средняя):

* Сетевые сообщения: `net_SetConVar`, `net_StringCmd`, `net_Tick`, `net_SignonState`,
  `clc_ClientInfo`, `clc_Move`, `clc_VoiceData`, `clc_BaselineAck`, `clc_ListenEvents`,
  `clc_RespondCvarValue`, `clc_FileCRCCheck`, `clc_FileMD5Check`, `clc_CmdKeyValues`,
  **`clc_GMod_ClientToServer`**, `svc_Print`, `svc_ServerInfo`, `svc_SendTable`,
  `svc_ClassInfo`, `svc_SetPause`, `svc_CreateStringTable`, `svc_UpdateStringTable`,
  `svc_VoiceInit`, `svc_VoiceData`, `svc_Sounds`, `svc_SetView`, `svc_FixAngle`,
  `svc_CrosshairAngle`, `svc_BSPDecal`, `svc_UserMessage`, `svc_EntityMessage`,
  `svc_GameEvent`, `svc_PacketEntities`, `svc_TempEntities`, `svc_Prefetch`,
  `svc_GameEventList`, `svc_GetCvarValue`, `svc_CmdKeyValues`,
  **`svc_GMod_ServerToClient`**.
  → Набор соответствует ветке Source 2007/SDK 2013 плюс GMod-специфичные сообщения
  (`*_GMod_*`) и поздние добавления (`CmdKeyValues`, `FileMD5Check`).
* Handshake: `A2S_GETCHALLENGE`, `A2S_SERVERQUERY_GETCHALLENGE`, «Unexepected authentication
  protocol %i!», «Server connection did not have the correct challenge», `Network protocol: %i`,
  `#GameUI_ServerRejectOldProtocol` / `NewProtocol`.
* Query-защита: `sv_max_queries_sec`, `sv_max_queries_window`, `sv_max_queries_sec_global`,
  `sv_logblocks`, `sv_enableoldqueries`.
* LAN: «Server is a lan server. ( no authentication, no non-class C addresses )»,
  «Initializing Steam libraries for LAN server».
* Hibernation: `sv_hibernate_think`, `sv_hibernate_drop_bots`.
* Сеть: `net_maxroutable`, `net_splitpacket_maxrate`, `net_compresspackets(_minsize)`,
  `net_maxfragments`, `net_queued_packet_thread`, `net_udp_rcvbuf`.

Связанные эксперименты: `RESEARCH.md` §0.5 (loopback-запросы игнорируются).

Декомпилированный код в репозиторий не включался.
