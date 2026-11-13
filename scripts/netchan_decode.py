#!/usr/bin/env python3
"""Offline decoder for captured Source/GMod netchannel traffic (research tool).

Reads the ``NNNN_<dir>.bin`` files written by ``udp_relay.py`` and prints, per datagram,
the netchannel header, reliable subchannel fragments, and the decoded message list of the
reassembled reliable streams and of the unreliable part.

Independent implementation written from observed traffic plus publicly documented Source
concepts; field layouts that are GMod-specific are marked HYPOTHESIS in comments.

usage: netchan_decode.py CAPTURE_DIR [--from N] [--to N] [--dir c2s|s2c] [--dump DIR]
"""

import argparse
import glob
import os
import struct
import sys
import zlib

NETMSG_TYPE_BITS = 6
# HYPOTHESIS (verified by checksum/decoding of capture): GMod uses a 16-bit reliable
# state and 4-bit subchannel index (16 subchannels instead of 8).
SUBCHAN_BITS = 4
RELSTATE_BYTES = 2
# GMod values (solved from capture: uncompressed-size field == LZSS header size):
MAX_FILE_SIZE_BITS = 30
FRAGMENT_BITS = 12
FRAGMENT_SIZE = 1 << FRAGMENT_BITS
NUM_FRAGMENT_BITS = 5


class Bits:
    def __init__(self, data, nbits=None):
        self.d = data
        self.n = len(data) * 8 if nbits is None else nbits
        self.p = 0

    def left(self):
        return self.n - self.p

    def bit(self):
        if self.p >= self.n:
            raise EOFError
        v = (self.d[self.p >> 3] >> (self.p & 7)) & 1
        self.p += 1
        return v

    def ubits(self, k):
        v = 0
        for i in range(k):
            v |= self.bit() << i
        return v

    def sbits(self, k):
        v = self.ubits(k)
        if v & (1 << (k - 1)):
            v -= 1 << k
        return v

    def byte(self):
        return self.ubits(8)

    def word(self):
        return self.ubits(16)

    def long(self):
        return self.sbits(32)

    def float(self):
        return struct.unpack("<f", struct.pack("<I", self.ubits(32)))[0]

    def varint(self):
        r = 0
        for i in range(5):
            b = self.byte()
            r |= (b & 0x7F) << (7 * i)
            if not b & 0x80:
                return r
        raise ValueError("varint too long")

    def bytes_(self, k):
        return bytes(self.byte() for _ in range(k))

    def string(self, limit=4096):
        out = bytearray()
        while True:
            c = self.byte()
            if c == 0:
                break
            out.append(c)
            if len(out) > limit:
                raise ValueError("string too long")
        return out.decode("utf-8", "replace")

    def skip(self, k):
        if k > self.left():
            raise EOFError
        self.p += k


def lzss_decompress(data):
    if data[:4] != b"LZSS":
        raise ValueError("not LZSS")
    size = struct.unpack_from("<I", data, 4)[0]
    out = bytearray()
    i = 8
    cmd = 0
    nb = 0
    while True:
        if nb == 0:
            cmd = data[i]
            i += 1
        nb = (nb + 1) & 7
        if cmd & 1:
            pos = (data[i] << 4) | (data[i + 1] >> 4)
            cnt = (data[i + 1] & 0xF) + 1
            i += 2
            if cnt == 1:
                break
            src = len(out) - pos - 1
            for k in range(cnt):
                out.append(out[src + k])
        else:
            out.append(data[i])
            i += 1
        cmd >>= 1
    if len(out) != size:
        raise ValueError(f"lzss size mismatch {len(out)} != {size}")
    return bytes(out)


NET_NAMES = {0: "NOP", 1: "Disconnect", 2: "File", 3: "Tick", 4: "StringCmd", 5: "SetConVar",
             6: "SignonState"}
SVC_NAMES = {7: "Print", 8: "ServerInfo", 9: "SendTable", 10: "ClassInfo", 11: "SetPause",
             12: "CreateStringTable", 13: "UpdateStringTable", 14: "VoiceInit", 15: "VoiceData",
             17: "Sounds", 18: "SetView", 19: "FixAngle", 20: "CrosshairAngle", 21: "BSPDecal",
             23: "UserMessage", 24: "EntityMessage", 25: "GameEvent", 26: "PacketEntities",
             27: "TempEntities", 28: "Prefetch", 29: "Menu", 30: "GameEventList",
             31: "GetCvarValue", 32: "CmdKeyValues", 33: "GMod_ServerToClient"}
CLC_NAMES = {8: "ClientInfo", 9: "Move", 10: "VoiceData", 11: "BaselineAck", 12: "ListenEvents",
             13: "RespondCvarValue", 14: "FileCRCCheck", 15: "SaveReplay", 16: "CmdKeyValues",
             17: "FileMD5Check", 18: "GMod_ClientToServer"}


def name_of(t, s2c):
    if t in NET_NAMES:
        return "net_" + NET_NAMES[t]
    tbl = SVC_NAMES if s2c else CLC_NAMES
    return ("svc_" if s2c else "clc_") + tbl.get(t, f"?{t}")


def parse_messages(b, s2c, out, ctx):
    """Decode messages until the buffer ends or an unknown message appears."""
    while b.left() >= NETMSG_TYPE_BITS:
        start = b.p
        t = b.ubits(NETMSG_TYPE_BITS)
        nm = name_of(t, s2c)
        try:
            info = decode_one(t, s2c, b, ctx)
        except (EOFError, ValueError) as e:
            out.append(f"    @{start} {nm} <decode error: {e}>")
            return False
        if info is None:
            out.append(f"    @{start} {nm} <unknown layout, {b.left()} bits left> next={b.d[b.p>>3:(b.p>>3)+16].hex()}")
            return False
        if t != 0:
            out.append(f"    @{start} {nm} {info}")
    return True


def decode_one(t, s2c, b, ctx):
    if t == 0:
        return ""
    if t == 1:
        return repr(b.string())
    if t == 2:
        tid = b.ubits(32)
        # GMod: two flag bits; observed 1,1 followed by a CRC32 naming user_custom/<crc>.dat
        # instead of a filename string (HYPOTHESIS for the 0,x case: plain string).
        f1 = b.bit()
        if f1:
            fn = f"custom crc={b.ubits(32):#010x}"
        else:
            fn = b.string()
        req = b.bit()
        return f"transfer={tid} customcrc={f1} file={fn!r} request={req}"
    if t == 3:
        tick = b.long()
        ft = b.word()
        sd = b.word()
        return f"tick={tick} frametime={ft} stddev={sd}"
    if t == 4:
        return repr(b.string())
    if t == 5:
        n = b.byte()
        vs = [(b.string(), b.string()) for _ in range(n)]
        return f"{n} vars {vs[:60]}"
    if t == 6:
        st = b.byte()
        sc = b.long()
        return f"state={st} spawncount={sc}"
    if s2c:
        return decode_svc(t, b, ctx)
    return decode_clc(t, b, ctx)


def blob(b, nbits):
    start = b.p
    b.skip(nbits)
    return start


def decode_svc(t, b, ctx):
    if t == 7:
        return repr(b.string())
    if t == 8:
        proto = b.sbits(16)
        sc = b.long()
        hltv = b.bit()
        ded = b.bit()
        crc = b.ubits(32)
        maxcls = b.word()
        md5 = b.bytes_(16).hex()
        slot = b.byte()
        maxcl = b.byte()
        ti = b.float()
        os_ = chr(b.byte())
        gd = b.string(); mp = b.string(); sky = b.string(); host = b.string()
        # GMod appends two strings (observed: '' and 'sandbox'); HYPOTHESIS: loading URL, gamemode
        x1 = b.string(); gm = b.string()
        ctx["maxclasses"] = maxcls
        return (f"proto={proto} servercount={sc} hltv={hltv} dedicated={ded} clientcrc={crc:#x} "
                f"maxclasses={maxcls} mapmd5={md5} slot={slot} maxclients={maxcl} tick={ti} os={os_} "
                f"gamedir={gd!r} map={mp!r} sky={sky!r} host={host!r} extra={x1!r} gamemode={gm!r}")
    if t == 9:
        nd = b.bit()
        ln = b.word()
        blob(b, ln)
        return f"needsdecoder={nd} bits={ln}"
    if t == 10:
        n = b.word()
        create = b.bit()
        if create:
            return f"classes={n} create_on_client=1"
        bits = n.bit_length()
        cls = []
        for _ in range(n):
            cid = b.ubits(bits)
            cls.append((cid, b.string(), b.string()))
        return f"classes={n} create_on_client=0 first={cls[:3]}"
    if t == 11:
        return f"paused={b.bit()}"
    if t == 12:
        fn = False
        name = b.string()
        # GMod: log2(max_entries) in 5 bits instead of a 16-bit max (solved from capture)
        enc = b.ubits(5)
        maxe = 1 << enc
        num = b.ubits(enc + 1)
        ln = b.varint()
        fixed = b.bit()
        uds = udsb = 0
        if fixed:
            uds = b.ubits(12)
            udsb = b.ubits(4)
        comp = b.bit()
        st = blob(b, ln)
        ctx.setdefault("tables", []).append(name)
        return (f"name={name!r} max={maxe} entries={num} bits={ln} fixed={fixed} "
                f"udsize={uds} udbits={udsb} compressed={comp}")
    if t == 13:
        tid = b.ubits(5)
        ch = b.word() if b.bit() else 1
        ln = b.ubits(20)
        blob(b, ln)
        tn = ctx.get("tables", [])
        return f"table={tid}({tn[tid] if tid < len(tn) else '?'}) changed={ch} bits={ln}"
    if t == 14:
        codec = b.string()
        q = b.byte()
        sr = b.sbits(16) if q == 255 else None
        return f"codec={codec!r} quality={q} rate={sr}"
    if t == 15:
        cl = b.byte(); prox = b.byte(); ln = b.word(); blob(b, ln)
        return f"from={cl} bits={ln}"
    if t == 17:
        rel = b.bit()
        if rel:
            n = 1; ln = b.ubits(8)
        else:
            n = b.ubits(8); ln = b.word()
        blob(b, ln)
        return f"reliable={rel} n={n} bits={ln}"
    if t == 18:
        return f"ent={b.ubits(ctx.get('edict_bits', 13))}"
    if t == 19:
        rel = b.bit(); a = [b.ubits(16) for _ in range(3)]
        return f"relative={rel} angle_raw={a}"
    if t == 21:
        def coord():
            i = b.bit(); f = b.bit()
            if not (i or f):
                return 0.0
            sign = b.bit()
            iv = b.ubits(14) + 1 if i else 0
            fv = b.ubits(5) if f else 0
            v = iv + fv / 32.0
            return -v if sign else v
        flags = [b.bit() for _ in range(3)]
        pos = [coord() if fl else 0.0 for fl in flags]
        di = b.ubits(ctx.get('decal_bits', 9))
        ent = mdl = 0
        if b.bit():
            ent = b.ubits(ctx.get('edict_bits', 13)); mdl = b.ubits(ctx.get('model_bits', 13))
        lp = b.bit()
        return f"pos={pos} decal={di} ent={ent} model={mdl} lowprio={lp}"
    if t == 23:
        mt = b.byte(); ln = b.ubits(11 + ctx.get('um_extra', 0)); blob(b, ln)
        return f"type={mt} bits={ln}"
    if t == 25:
        ln = b.ubits(11); blob(b, ln)
        return f"bits={ln}"
    if t == 26:
        eb = ctx.get("edict_bits", 13)
        maxe = b.ubits(eb)
        delta = b.bit()
        frm = b.long() if delta else -1
        base = b.bit()
        upd = b.ubits(eb)
        ln = b.ubits(20)
        ub = b.bit()
        blob(b, ln)
        return f"max={maxe} delta={delta} from={frm} baseline={base} updated={upd} bits={ln} updbase={ub}"
    if t == 27:
        n = b.ubits(8); ln = b.varint(); blob(b, ln)
        return f"n={n} bits={ln}"
    if t == 30:
        n = b.ubits(9); ln = b.ubits(20); blob(b, ln)
        return f"events={n} bits={ln}"
    if t == 31:
        ck = b.long(); nm = b.string()
        return f"cookie={ck} cvar={nm!r}"
    if t == 32:
        n = b.long(); b.skip(n * 8)
        return f"bytes={n}"
    if t == 33:
        # svc_GMod_ServerToClient: 20-bit payload length in bits + payload (from capture)
        ln = b.ubits(20)
        st = b.p
        first = b.ubits(min(8, ln)) if ln else None
        b.p = st + ln
        return f"GMod_ServerToClient bits={ln} first_byte={first}"
    return None


def decode_clc(t, b, ctx):
    if t == 8:
        sc = b.long(); crc = b.ubits(32); hltv = b.bit(); fid = b.ubits(32); fname = b.string()
        cf = []
        for _ in range(4):
            cf.append(b.ubits(32) if b.bit() else 0)
        return (f"servercount={sc} sendtablecrc={crc:#x} hltv={hltv} friendsid={fid} name={fname!r} "
                f"customfiles={[hex(x) for x in cf]} next_bits={peek(b, 32)}")
    if t == 9:
        nn = b.ubits(4); nb = b.ubits(3); ln = b.word(); blob(b, ln)
        return f"new={nn} backup={nb} bits={ln}"
    if t == 10:
        ln = b.word(); blob(b, ln)
        return f"bits={ln}"
    if t == 11:
        return f"tick={b.long()} nr={b.bit()}"
    if t == 12:
        ev = [b.ubits(32) for _ in range(16)]
        return f"mask={''.join(f'{x:08x}' for x in ev)}"
    if t == 13:
        ck = b.long(); st = b.sbits(4); n = b.string(); v = b.string()
        return f"cookie={ck} status={st} {n!r}={v!r}"
    if t == 16:
        n = b.long(); b.skip(n * 8)
        return f"bytes={n}"
    if t == 18:
        # clc_GMod_ClientToServer: 20-bit payload length in bits + payload (from capture)
        ln = b.ubits(20)
        st = b.p
        first = b.ubits(min(8, ln)) if ln else None
        b.p = st + ln
        return f"GMod_ClientToServer bits={ln} first_byte={first}"
    return None


def peek(b, k):
    p = b.p
    k = min(k, b.left())
    bits = "".join(str((b.d[(p + i) >> 3] >> ((p + i) & 7)) & 1) for i in range(k))
    return bits


class Stream:
    def __init__(self):
        self.buf = None
        self.size = 0
        self.got = 0
        self.comp = None
        self.fname = None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dir")
    ap.add_argument("--from", dest="frm", type=int, default=0)
    ap.add_argument("--to", type=int, default=10**9)
    ap.add_argument("--dump", default=None)
    ap.add_argument("--quiet-unreliable", action="store_true")
    a = ap.parse_args()
    files = sorted(glob.glob(os.path.join(a.dir, "*_*.bin")))
    streams = {"c2s": [Stream(), Stream()], "s2c": [Stream(), Stream()]}
    splits = {}
    ctx = {"c2s": {}, "s2c": {}}
    nrel = {"c2s": 0, "s2c": 0}
    for f in files:
        base = os.path.basename(f)
        idx = int(base[:4])
        d = base[5:8]
        if idx < a.frm or idx > a.to:
            continue
        data = open(f, "rb").read()
        s2c = d == "s2c"
        hdr = struct.unpack_from("<i", data, 0)[0]
        if hdr == -2:
            sid, total, num, sz = struct.unpack_from("<iBBH", data, 4)
            key = (d, sid)
            parts = splits.setdefault(key, {})
            parts[num] = data[12:]
            if len(parts) < total:
                continue
            data = b"".join(parts[i] for i in range(total))
            del splits[key]
            hdr = struct.unpack_from("<i", data, 0)[0]
            print(f"{idx:04d} {d} [reassembled split id={sid} n={total}] {len(data)} bytes")
        if hdr == -3:
            data = lzss_decompress(data[4:])
            hdr = struct.unpack_from("<i", data, 0)[0]
            print(f"{idx:04d} {d} [lzss packet] -> {len(data)} bytes")
        if hdr == -1:
            print(f"{idx:04d} {d} OOB {chr(data[4])!r} len={len(data)}")
            continue
        seq, ack, flags = struct.unpack_from("<iiB", data, 0)
        ck = struct.unpack_from("<H", data, 9)[0]
        c = zlib.crc32(data[11:])
        okck = ((c & 0xFFFF) ^ (c >> 16)) == ck
        relstate = int.from_bytes(data[11:11 + RELSTATE_BYTES], "little")
        p = 11 + RELSTATE_BYTES
        choked = 0
        if flags & 0x10:
            choked = data[p]; p += 1
        chal = None
        if flags & 0x20:
            chal = struct.unpack_from("<I", data, p)[0]; p += 4
        pad = (flags >> 5) & 7  # NOTE: overlaps CHALLENGE flag bit; see PROTOCOL.md
        nbits = len(data) * 8
        b = Bits(data, nbits)
        b.p = p * 8
        lines = []
        if flags & 0x01:
            sub = b.ubits(SUBCHAN_BITS)
            for si in range(2):
                if not b.bit():
                    continue
                st = streams[d][si]
                single = b.bit() == 0
                start = num = 0
                if not single:
                    start = b.ubits(MAX_FILE_SIZE_BITS - FRAGMENT_BITS)
                    num = b.ubits(NUM_FRAGMENT_BITS)
                offset = start * FRAGMENT_SIZE
                length = num * FRAGMENT_SIZE
                if offset == 0:
                    st.fname = None
                    st.comp = None
                    if single:
                        if b.bit():
                            st.comp = b.ubits(MAX_FILE_SIZE_BITS)
                        st.size = b.varint()
                    else:
                        if b.bit():
                            tid = b.ubits(32)
                            st.fname = (tid, b.string())
                        if b.bit():
                            st.comp = b.ubits(MAX_FILE_SIZE_BITS)
                        st.size = b.ubits(MAX_FILE_SIZE_BITS)
                    st.buf = bytearray(st.size)
                    st.got = 0
                    nfr = (st.size + FRAGMENT_SIZE - 1) // FRAGMENT_SIZE
                    if single:
                        num = nfr
                        length = num * FRAGMENT_SIZE
                nfr = (st.size + FRAGMENT_SIZE - 1) // FRAGMENT_SIZE
                if start + num == nfr:
                    rest = FRAGMENT_SIZE - (st.size % FRAGMENT_SIZE)
                    if rest < FRAGMENT_SIZE:
                        length -= rest
                st.buf[offset:offset + length] = b.bytes_(length)
                st.got += num
                lines.append(f"  rel sub={sub} stream={si} single={int(single)} frag={start}+{num} "
                             f"size={st.size} comp={st.comp} file={st.fname} ({st.got}/{nfr})")
                if st.got >= nfr:
                    payload = bytes(st.buf)
                    if st.comp is not None:
                        payload = lzss_decompress(payload)
                    nrel[d] += 1
                    lines.append(f"  == reliable[{si}] #{nrel[d]} complete {len(payload)} bytes")
                    if a.dump:
                        os.makedirs(a.dump, exist_ok=True)
                        open(os.path.join(a.dump, f"{d}_rel{nrel[d]:03d}_{idx:04d}_s{si}.bin"), "wb").write(payload)
                    if si == 0:
                        parse_messages(Bits(payload), s2c, lines, ctx[d])
                    st.buf = None
        # unreliable remainder (pad bits are ones at the end; NOP=0 padding ignored)
        ul = []
        parse_messages(b, s2c, ul, ctx[d])
        if a.quiet_unreliable and not lines:
            continue
        print(f"{idx:04d} {d} seq={seq} ack={ack} flags={flags:#04x} ck={'ok' if okck else 'BAD'} "
              f"rel={relstate:#06x} choked={choked} len={len(data)}")
        for l in lines + ul:
            print(l)


if __name__ == "__main__":
    main()
