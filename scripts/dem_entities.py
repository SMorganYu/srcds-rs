#!/usr/bin/env python3
"""Decode svc_PacketEntities from a Source/GMod demo using the demo's own data tables.

Research tool for verifying srcds-rs' understanding of the entity wire format
(delta header, prop-index encoding, prop encoders, flattening order) against traffic
produced by the reference dedicated server. A packet is considered understood when the
decoder consumes *exactly* the bit length the message declares.

Usage: python -I dem_entities.py <file.dem> [--packets N] [--verbose]
"""
import math
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dem_datatables as dd  # noqa: E402
import netchan_decode as nd  # noqa: E402

SPROP_UNSIGNED = 1 << 0
SPROP_COORD = 1 << 1
SPROP_NOSCALE = 1 << 2
SPROP_NORMAL = 1 << 5  # also "varint" for integer props
SPROP_EXCLUDE = 1 << 6
SPROP_INSIDEARRAY = 1 << 8
SPROP_CHANGES_OFTEN = 1 << 10
SPROP_COLLAPSIBLE = 1 << 12
SPROP_COORD_MP = 1 << 13
SPROP_COORD_MP_LOW = 1 << 14
SPROP_COORD_MP_INT = 1 << 15
EDICT_BITS = 13
SERIAL_BITS = 10


def ubitvar(b):
    # GMod (protocol 24 era) variable-length uint: 6 bits = low nibble + 2-bit selector
    # in bits 4..5; selector 1/2/3 appends 4/8/28 more high bits.
    v = b.ubits(6)
    sel = v >> 4
    if sel:
        v = (v & 15) | (b.ubits((0, 4, 8, 28)[sel]) << 4)
    return v


def varint(b, n=5):
    r = 0
    for i in range(n):
        c = b.ubits(8)
        r |= (c & 0x7F) << (7 * i)
        if not c & 0x80:
            return r
    return r


# ---------------------------------------------------------------- flattening
def flatten(tables, name):
    excludes = set()

    def gather_ex(tn, seen):
        if tn in seen:
            return
        seen.add(tn)
        for p in tables[tn]["props"]:
            if p["flags"] & SPROP_EXCLUDE:
                excludes.add((p["dt"], p["name"]))
            elif p["type"] == "DataTable":
                gather_ex(p["dt"], seen)

    gather_ex(name, set())
    out = []

    def iterate(tn, nondt):
        props = tables[tn]["props"]
        for i, p in enumerate(props):
            if p["flags"] & (SPROP_EXCLUDE | SPROP_INSIDEARRAY) or (tn, p["name"]) in excludes:
                continue
            if p["type"] == "DataTable":
                if p["flags"] & SPROP_COLLAPSIBLE:
                    iterate(p["dt"], nondt)
                else:
                    build(p["dt"])
            else:
                q = dict(p)
                if p["type"] == "Array":
                    q["elem"] = props[i - 1]
                nondt.append(q)

    def build(tn):
        nondt = []
        iterate(tn, nondt)
        out.extend(nondt)

    build(name)
    # priority: CHANGES_OFTEN props move to the front (selection-style, as observed)
    start = 0
    while True:
        for i in range(start, len(out)):
            if out[i]["flags"] & SPROP_CHANGES_OFTEN:
                out[i], out[start] = out[start], out[i]
                start += 1
                break
        else:
            return out


# ---------------------------------------------------------------- prop decoders
def read_float(b, p):
    f = p["flags"]
    if f & SPROP_COORD:
        i, fr = b.bit(), b.bit()
        if not (i or fr):
            return 0.0
        s = b.bit()
        iv = b.ubits(14) + 1 if i else 0
        fv = b.ubits(5) if fr else 0
        v = iv + fv / 32.0
        return -v if s else v
    if f & (SPROP_COORD_MP | SPROP_COORD_MP_LOW | SPROP_COORD_MP_INT):
        integral = bool(f & SPROP_COORD_MP_INT)
        low = bool(f & SPROP_COORD_MP_LOW)
        inb = b.bit()
        intv = b.bit()
        if integral:
            if intv:
                s = b.bit()
                v = b.ubits(11 if inb else 14) + 1
                return -v if s else v
            return 0.0
        s = b.bit()
        iv = 0
        if intv:
            iv = b.ubits(11 if inb else 14) + 1
        fv = b.ubits(3 if low else 5)
        v = iv + fv / (8.0 if low else 32.0)
        return -v if s else v
    if f & SPROP_NOSCALE:
        return b.float()
    if f & SPROP_NORMAL:
        s = b.bit()
        v = b.ubits(11) / 2047.0
        return -v if s else v
    bits = p["bits"]
    raw = b.ubits(bits)
    return p["low"] + (p["high"] - p["low"]) * raw / ((1 << bits) - 1)


def read_prop(b, p):
    t, f = p["type"], p["flags"]
    if t in ("Int", "Int64"):
        if f & SPROP_NORMAL:
            v = varint(b, 5 if t == "Int" else 10)
            return v if f & SPROP_UNSIGNED else (v >> 1) ^ -(v & 1)
        return b.ubits(p["bits"]) if f & SPROP_UNSIGNED else b.sbits(p["bits"])
    if t == "Float":
        return round(read_float(b, p), 3)
    if t in ("Vector", "VectorXY"):
        x, y = read_float(b, p), read_float(b, p)
        if t == "VectorXY":
            return (round(x, 3), round(y, 3))
        if f & SPROP_NORMAL:
            return (round(x, 3), round(y, 3), "-" if b.bit() else "+")
        return (round(x, 3), round(y, 3), round(read_float(b, p), 3))
    if t == "Quaternion":
        return tuple(round(read_float(b, p), 3) for _ in range(4))
    if t == "String":
        n = b.ubits(9)
        return bytes(b.ubits(8) for _ in range(n)).decode("utf-8", "replace")
    if t == "Array":
        n = b.ubits(int(math.log2(p["elements"])) + 1)
        return [read_prop(b, p["elem"]) for _ in range(n)]
    raise ValueError(f"type {t}")


def read_props(b, flat, verbose):
    last = -1
    vals = []
    while b.bit():
        last += 1 + ubitvar(b)
        p = flat[last]
        v = read_prop(b, p)
        vals.append((p["name"], v))
    return vals


# ---------------------------------------------------------------- entities
def decode_entities(b, n_updates, is_delta, classes, flats, class_bits, verbose):
    base = -1
    ents = []
    for _ in range(n_updates):
        ent = base + 1 + ubitvar(b)
        base = ent
        if b.bit():
            delete = b.bit()
            ents.append((ent, "leave+delete" if delete else "leave", None))
            continue
        enter = b.bit()
        if enter:
            cid = b.ubits(class_bits)
            serial = b.ubits(SERIAL_BITS)
            cname = classes[cid]["name"]
            vals = read_props(b, flats(cid), verbose)
            ents.append((ent, f"enter {cname} serial={serial}", vals))
        else:
            ents.append((ent, "delta", None))  # needs per-entity class; not tracked here
            raise ValueError("delta update of a known entity (class tracking not implemented)")
    if is_delta:
        while b.bit():
            ents.append((b.ubits(EDICT_BITS), "deleted", None))
    return ents


def main():
    path = sys.argv[1]
    verbose = "--verbose" in sys.argv
    limit = int(sys.argv[sys.argv.index("--packets") + 1]) if "--packets" in sys.argv else 3
    buf = open(path, "rb").read()
    tables = classes = None
    cache = {}
    done = 0
    for cmd, tick, data in dd.frames(buf):
        if cmd == 6:
            tl, classes, _, _ = dd.parse_datatables(data)
            tables = {t["name"]: t for t in tl}
            class_bits = int(math.log2(len(classes))) + 1

            def flats(cid):
                if cid not in cache:
                    cache[cid] = flatten(tables, classes[cid]["dt"])
                return cache[cid]
        if cmd != 2 or tables is None:
            continue
        b = nd.Bits(data)
        ctx = {"edict_bits": EDICT_BITS}
        while b.left() >= 6:
            t = b.ubits(6)
            if t == 26:
                maxe = b.ubits(EDICT_BITS)
                delta = b.bit()
                frm = b.long() if delta else -1
                baseline = b.bit()
                upd = b.ubits(EDICT_BITS)
                ln = b.ubits(20)
                updb = b.bit()
                start = b.p
                sub = nd.Bits(data, start + ln)
                sub.p = start
                try:
                    ents = decode_entities(sub, upd, delta, classes, flats, class_bits, verbose)
                    status = "OK" if sub.p == start + ln else f"MISMATCH used={sub.p - start}"
                except (EOFError, ValueError, IndexError) as e:
                    ents, status = [], f"ERROR {e!r} at {sub.p - start}"
                print(f"tick {tick}: PacketEntities max={maxe} delta={delta} from={frm} baseline={baseline} "
                      f"updated={upd} bits={ln} updbase={updb} -> {status}")
                for e in ents if verbose else ents[:12]:
                    print(f"   ent {e[0]:5} {e[1]}" + (f" {e[2][:8]}" if e[2] and not verbose else (f" {e[2]}" if e[2] else "")))
                b.p = start + ln
                done += 1
                if done >= limit:
                    return
                continue
            try:
                info = nd.decode_one(t, True, b, ctx)
            except (EOFError, ValueError) as e:
                print(f"tick {tick}: stop at msg {t}: {e}")
                break
            if info is None:
                print(f"tick {tick}: unknown msg {t}, rest skipped")
                break


if __name__ == "__main__":
    main()
