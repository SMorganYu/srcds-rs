#!/usr/bin/env python3
"""Logging UDP relay for capturing the Source connection handshake.

The unmodified game client connects to the relay (e.g. ``connect 127.0.0.1:27016``); the
relay forwards every datagram to the real server and back, and writes each datagram to
``<out>/NNNN_<dir>.bin`` plus a line in ``<out>/log.txt``.  Only traffic of the local test
client is relayed; nothing is modified.

usage: udp_relay.py LISTEN_IP LISTEN_PORT SERVER_IP SERVER_PORT OUT_DIR [MAX_SAVED]
"""

import os
import select
import socket
import sys
import time


def main() -> int:
    if len(sys.argv) < 6:
        print(__doc__)
        return 2
    lip, lport, sip, sport, out = sys.argv[1], int(sys.argv[2]), sys.argv[3], int(sys.argv[4]), sys.argv[5]
    max_saved = int(sys.argv[6]) if len(sys.argv) > 6 else 400
    os.makedirs(out, exist_ok=True)
    front = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    front.bind((lip, lport))
    back = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    back.bind(("0.0.0.0", 0))
    server = (sip, sport)
    client = None
    n = 0
    t0 = time.time()
    log = open(os.path.join(out, "log.txt"), "a", buffering=1)
    print(f"relay {lip}:{lport} -> {sip}:{sport}, out={out}", flush=True)
    while True:
        r, _, _ = select.select([front, back], [], [], 1.0)
        for s in r:
            try:
                data, addr = s.recvfrom(65535)
            except ConnectionResetError:
                continue
            if s is front:
                client = addr
                back.sendto(data, server)
                d = "c2s"
            else:
                if addr != server or client is None:
                    continue
                front.sendto(data, client)
                d = "s2c"
            if n < max_saved:
                with open(os.path.join(out, f"{n:04d}_{d}.bin"), "wb") as f:
                    f.write(data)
            log.write(f"{n:04d} {time.time() - t0:9.3f} {d} {len(data):5d} {data[:24].hex()}\n")
            n += 1


if __name__ == "__main__":
    sys.exit(main())
