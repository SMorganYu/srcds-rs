#!/usr/bin/env python3
"""Minimal, independent A2S probe used to capture reference fixtures.

Usage: python scripts/a2s_probe.py HOST PORT [OUTDIR]

Sends A2S_INFO / A2S_PLAYER / A2S_RULES (with challenge handling as described in
the public Valve Developer Wiki "Server queries" page) and prints/saves the raw
response datagrams. Only intended for loopback test servers you control.
"""
import os
import socket
import sys

HDR = b"\xff\xff\xff\xff"


def xchg(sock, addr, payload):
    sock.sendto(payload, addr)
    pkts = []
    while True:
        try:
            data, _ = sock.recvfrom(65535)
        except socket.timeout:
            break
        pkts.append(data)
        # Single-packet responses end the exchange.
        if data[:4] == HDR:
            break
    return pkts


def query(sock, addr, base, challenge_in_body):
    pkts = xchg(sock, addr, base + (HDR if challenge_in_body else b""))
    if pkts and pkts[0][:5] == HDR + b"A":
        chal = pkts[0][5:9]
        first = pkts[0]
        if challenge_in_body:
            pkts = xchg(sock, addr, base + chal)
        else:
            pkts = xchg(sock, addr, base + chal)
        return [first] + pkts
    return pkts


def main():
    host, port = sys.argv[1], int(sys.argv[2])
    out = sys.argv[3] if len(sys.argv) > 3 else None
    addr = (host, port)
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(1.5)
    results = {
        "info": query(s, addr, HDR + b"TSource Engine Query\x00", False),
        "player": query(s, addr, HDR + b"U", True),
        "rules": query(s, addr, HDR + b"V", True),
        "ping": xchg(s, addr, HDR + b"i"),
    }
    for name, pkts in results.items():
        for i, p in enumerate(pkts):
            print(f"{name}[{i}] len={len(p)} {p[:96].hex()}")
            if out:
                os.makedirs(out, exist_ok=True)
                with open(os.path.join(out, f"{name}_{i}.bin"), "wb") as fh:
                    fh.write(p)


if __name__ == "__main__":
    main()
