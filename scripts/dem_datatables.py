#!/usr/bin/env python3
"""Dump the SendTables / server class list stored in a Source demo (.dem).

The client engine writes its data tables into every demo it records (``dem_datatables``
frame); this tool decodes that frame so srcds-rs can learn table/prop *names* and
nesting of the classes it wants to network. Output is a text dump for local analysis
(keep it out of the repository).

Usage: python -I dem_datatables.py <file.dem> [--json out.json]
"""
import json
import struct
import sys

PROPINFOBITS_NUMPROPS = 10
PROPINFOBITS_TYPE = 5
PROPINFOBITS_FLAGS = 16
PROPINFOBITS_NUMBITS = 7
PROPINFOBITS_NUMELEMENTS = 10
DPT_ARRAY, DPT_DATATABLE = 5, 6
SPROP_EXCLUDE = 1 << 6
TYPES = ["Int", "Float", "Vector", "VectorXY", "String", "Array", "DataTable", "Quaternion", "Int64"]


class Bits:
    def __init__(self, data):
        self.d = data
        self.p = 0

    def u(self, n):
        v = 0
        for i in range(n):
            byte = self.d[self.p >> 3]
            v |= ((byte >> (self.p & 7)) & 1) << i
            self.p += 1
        return v

    def s(self):
        out = bytearray()
        while True:
            c = self.u(8)
            if c == 0:
                return out.decode("utf-8", "replace")
            out.append(c)

    def f(self):
        return struct.unpack("<f", struct.pack("<I", self.u(32)))[0]


def read_infos(b):
    name = b.s()
    n = b.u(PROPINFOBITS_NUMPROPS)
    props = []
    for _ in range(n):
        t = b.u(PROPINFOBITS_TYPE)
        pname = b.s()
        flags = b.u(PROPINFOBITS_FLAGS)
        p = {"type": TYPES[t] if t < len(TYPES) else t, "name": pname, "flags": flags}
        if t == DPT_DATATABLE or flags & SPROP_EXCLUDE:
            p["dt"] = b.s()
        elif t == DPT_ARRAY:
            p["elements"] = b.u(PROPINFOBITS_NUMELEMENTS)
        else:
            p["low"], p["high"], p["bits"] = b.f(), b.f(), b.u(PROPINFOBITS_NUMBITS)
        props.append(p)
    return {"name": name, "props": props}


def parse_datatables(data):
    b = Bits(data)
    tables = []
    while b.u(1):
        needs = b.u(1)
        t = read_infos(b)
        t["needs_decoder"] = bool(needs)
        tables.append(t)
    classes = []
    n = b.u(16)
    for _ in range(n):
        cid = b.u(16)
        classes.append({"id": cid, "name": b.s(), "dt": b.s()})
    return tables, classes, b.p, len(data) * 8


def frames(buf):
    assert buf[:8] in (b"HL2DEMO\0", b"GMODEMO\0"), "not a demo"
    demo_proto, net_proto = struct.unpack_from("<ii", buf, 8)
    p = 1072
    while p < len(buf):
        cmd = buf[p]
        tick = struct.unpack_from("<i", buf, p + 1)[0]
        p += 5
        if cmd in (1, 2):
            p += 76 + 8
        elif cmd == 3:
            yield cmd, tick, b""
            continue
        elif cmd == 5:
            p += 4
        elif cmd == 7:
            return
        (ln,) = struct.unpack_from("<i", buf, p)
        p += 4
        yield cmd, tick, buf[p:p + ln]
        p += ln


def main():
    buf = open(sys.argv[1], "rb").read()
    for cmd, tick, data in frames(buf):
        if cmd == 6:
            tables, classes, used, total = parse_datatables(data)
            print(f"# {len(tables)} tables, {len(classes)} classes, {used}/{total} bits used")
            for c in classes:
                print(f"class {c['id']:4} {c['name']} -> {c['dt']}")
            for t in tables:
                print(f"\ntable {t['name']} needs_decoder={t['needs_decoder']} props={len(t['props'])}")
                for p in t["props"]:
                    extra = p.get("dt") or p.get("elements") or f"bits={p.get('bits')} [{p.get('low'):g},{p.get('high'):g}]"
                    print(f"  {p['type']:<10} {p['name']:<40} flags={p['flags']:#06x} {extra}")
            if "--json" in sys.argv:
                out = sys.argv[sys.argv.index("--json") + 1]
                json.dump({"tables": tables, "classes": classes}, open(out, "w"), indent=1)
            return
    print("no dem_datatables frame")


if __name__ == "__main__":
    main()
