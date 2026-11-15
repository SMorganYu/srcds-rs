#!/usr/bin/env python3
"""List string-table entries stored in a Source/GMod demo (``dem_stringtables`` frame).

Research tool: e.g. to learn which client Lua files a reference server announces in
``client_lua_files`` (the file *names*; srcds-rs computes hashes from its own files).

Usage: python -I dem_stringtables.py <file.dem> [table] [--names-only]
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dem_datatables as dd  # noqa: E402


def parse(data):
    b = dd.Bits(data)
    tables = {}
    for _ in range(b.u(8)):
        name = b.s()
        entries = []
        for _ in range(b.u(16)):
            s = b.s()
            ud = None
            if b.u(1):
                n = b.u(16)
                ud = bytes(b.u(8) for _ in range(n))
            entries.append((s, ud))
        if b.u(1):  # client-side entries
            for _ in range(b.u(16)):
                b.s()
                if b.u(1):
                    n = b.u(16)
                    for _ in range(n):
                        b.u(8)
        tables[name] = entries
    return tables


def main():
    buf = open(sys.argv[1], "rb").read()
    want = sys.argv[2] if len(sys.argv) > 2 and not sys.argv[2].startswith("--") else None
    names_only = "--names-only" in sys.argv
    for cmd, _tick, data in dd.frames(buf):
        if cmd != 8:
            continue
        for name, entries in parse(data).items():
            if want and name != want:
                continue
            if not names_only:
                print(f"== {name}: {len(entries)} entries")
            for s, ud in entries:
                print(s if names_only else f"  {s!r} ud={len(ud) if ud else 0}")
        return


if __name__ == "__main__":
    main()
