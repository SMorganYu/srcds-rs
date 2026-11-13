# Architecture

## Crates (созданы только необходимые)

| Crate | Назначение |
|---|---|
| `srcds-protocol` | Форматы проводов: `ByteReader/Writer`, `BitReader/Writer` (LSB-first), framing, A2S, split. Без I/O, без паник на входных данных, все аллокации ограничены размером входа. |
| `srcds-net` | `UdpTransport` (std UDP, переиспользуемый буфер, обработка WSAECONNRESET), `ChallengeIssuer` (stateless), `TokenBucket`/`PerIpLimiter` (ограниченная таблица). |
| `srcds-core` | `ServerConfig`, `QueryState/QueryHandler`, `Server` (главный цикл), `Metrics` (счётчики + p50/p95/p99 тиков). |
| `srcds-server` | Исполняемый файл `srcds-rs`: CLI (clap), логирование (tracing), Ctrl+C, консоль `quit`. |
| `tools/packet-diff` | Семантическое сравнение датаграмм с игнорированием динамических полей. |

Планируемые (не созданы): `srcds-steam`, `srcds-entities`, `srcds-bsp`, `srcds-filesystem`
(VPK), `srcds-lua`, `srcds-physics`, `srcds-math`.

## Модель исполнения (прототип)

```
main ─ parse CLI ─ init tracing ─ Server::bind(UDP) ─ ctrlc/console threads
        └─ Server::run (один поток симуляции):
             loop {
               if now >= next_tick { tick(); record metrics; next_tick += interval }
               else recv(timeout = next_tick - now) → classify → handle → send
             }
```

* Один поток владеет всем состоянием → нет блокировок, детерминированный порядок:
  пакеты, полученные до тика N, обрабатываются до тика N.
* Тик 15 мс (66.67 Гц, дефолт GMod). При отставании > 4 интервалов — ресинхронизация
  (`tick_overruns`), а не «догоняющая» серия тиков.
* Shutdown: `AtomicBool`, проверяется на каждой итерации (латентность ≤ 1 тик).

Почему не async/многопоточность сейчас: нагрузка M1 — единицы пакетов; однопоточный цикл
проще доказать корректным. Точки расширения: отдельный сетевой поток с bounded-очередью
в поток симуляции (M2+, когда появятся netchannel'ы), worker pool для сериализации
снапшотов по клиентам (M4+, только для независимых данных).

## Безопасность входных данных

* Размер датаграммы ограничен 65507; ответы ограничены `ByteWriter` (1400 для одиночных,
  32×1248 для split).
* Ответ без challenge — 9 байт (< запроса 25 байт) → нет усиления.
* Rate limit: 30 ответов/с на IP, 2000/с глобально; таблица IP ≤ 16384 записей, при
  переполнении — fail closed.
* `#![forbid(unsafe_code)]` на уровне workspace.
