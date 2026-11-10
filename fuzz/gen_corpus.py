#!/usr/bin/env python3
"""Regenerate fuzz/corpus/: a few tiny, valid inputs per target.

Generated, never copied from a real .torrent or a real tracker reply: those
carry private announce URLs and passkeys. Every host here is `example.invalid`
and every key is zeros. Run from anywhere; the output is deterministic.
"""
import hashlib
import struct
from pathlib import Path

OUT = Path(__file__).resolve().parent / "corpus"


def b(x):
    if isinstance(x, int):
        return b"i%de" % x
    if isinstance(x, str):
        x = x.encode()
    if isinstance(x, bytes):
        return b"%d:" % len(x) + x
    if isinstance(x, list):
        return b"l" + b"".join(b(i) for i in x) + b"e"
    if isinstance(x, dict):
        return b"d" + b"".join(b(k) + b(x[k]) for k in sorted(x)) + b"e"
    raise TypeError(x)


def put(target, name, data):
    d = OUT / target
    d.mkdir(parents=True, exist_ok=True)
    (d / name).write_bytes(data)


PIECE = 16384
content = bytes(range(256)) * 64  # 16 KiB, one piece
info_v1 = {"name": "seed.bin", "length": len(content), "piece length": PIECE,
           "pieces": hashlib.sha1(content).digest()}
info_multi = {"name": "seed", "piece length": PIECE,
              "pieces": hashlib.sha1(content).digest() * 2,
              "files": [{"length": 20000, "path": ["a", "one.bin"]},
                        {"length": 12768, "path": ["two.bin"]}]}
ann = "http://tracker.example.invalid/announce"
torrents = {
    "v1-single": b({"announce": ann, "info": info_v1}),
    "v1-multi-tiers": b({"announce": ann,
                         "announce-list": [[ann], ["udp://tracker.example.invalid:6969/announce"]],
                         "comment": "seed", "info": info_multi}),
    "v1-minimal": b({"info": info_v1}),
    "v2-ish": b({"info": {"name": "seed.bin", "piece length": PIECE, "meta version": 2,
                          "file tree": {"seed.bin": {"": {"length": len(content),
                                                          "pieces root": b"\x00" * 32}}}}}),
}

for name, t in torrents.items():
    for tgt in ("metainfo_parse", "metainfo_info_dict", "metainfo_piece_hashes", "bencode_decode",
                "bencode_decode_prefix"):
        put(tgt, name, t)
put("bencode_decode", "int", b(42))
put("bencode_decode", "nested", b({"a": [1, 2, {"b": "c"}], "d": b"\x00\xff"}))
put("bencode_decode_prefix", "trailing", b([1, 2]) + b"trailing bytes")

# Merkle: first byte = leaf count exponent, then the content.
put("merkle_root", "empty", b"\x00")
put("merkle_root", "one-block", bytes([4]) + content)
put("merkle_root", "two-blocks", bytes([6]) + content + content[:100])

# Extension handshake / metadata / PEX
put("ext_handshake", "basic", b({"m": {"ut_pex": 1, "ut_metadata": 2}, "metadata_size": 31235, "v": "seed"}))
put("ext_handshake", "disabled", b({"m": {"ut_pex": 0}}))
put("ext_handshake", "holepunch", b({"m": {"ut_holepunch": 4, "ut_metadata": 3, "ut_pex": 1}}))
put("metadata_msg", "request", b({"msg_type": 0, "piece": 0}))
put("metadata_msg", "data", b({"msg_type": 1, "piece": 1, "total_size": 20000}) + b"\x01" * 64)
put("metadata_msg", "reject", b({"msg_type": 2, "piece": 3}))
v4 = bytes([192, 0, 2, 1]) + struct.pack(">H", 6881) + bytes([198, 51, 100, 7]) + struct.pack(">H", 51413)
v6 = bytes.fromhex("20010db8000000000000000000000001") + struct.pack(">H", 6881)
put("pex", "v4", b"\x00" + b({"added": v4, "added.f": b"\x00\x01", "dropped": v4[:6]}))
put("pex", "v6", b"\x01" + b({"added": v4, "added6": v6, "added6.f": b"\x00"}))
put("pex", "empty", b"\x01" + b({}))

# Handshake: the real layout, reserved bits for fast + extended.
hs = bytes([19]) + b"BitTorrent protocol" + bytes([0, 0, 0, 0, 0, 0x10, 0, 0x04]) + b"\x11" * 20 + b"-XX0000-" + b"0" * 12
put("handshake", "valid", hs)
put("handshake", "no-extensions", hs[:20] + b"\x00" * 8 + hs[28:])
put("handshake", "bad-proto", bytes([19]) + b"BitTorrenT protocol" + hs[20:])

# Peer messages: length-prefixed frames, whole stream in one input.
def frame(mid, payload=b""):
    return struct.pack(">I", 1 + len(payload)) + bytes([mid]) + payload

put("peer_message", "keepalive-choke", struct.pack(">I", 0) + frame(0) + frame(1))
put("peer_message", "have-request", frame(4, struct.pack(">I", 3)) + frame(6, struct.pack(">III", 0, 0, 16384)))
put("peer_message", "bitfield-piece", frame(5, b"\xff\x80") + frame(7, struct.pack(">II", 0, 0) + b"\x00" * 32))
put("peer_message", "fast", frame(0x0E) + frame(0x0F) + frame(0x0D, struct.pack(">I", 2)) +
    frame(0x11, struct.pack(">I", 1)) + frame(0x10, struct.pack(">III", 0, 0, 16384)))
put("peer_message", "extended", frame(20, b"\x00" + b({"m": {"ut_pex": 1}})))

# HTTP tracker replies
peers4 = bytes([192, 0, 2, 10]) + struct.pack(">H", 6881)
put("http_announce", "compact", b({"interval": 1800, "min interval": 900, "complete": 5, "incomplete": 2,
                                    "peers": peers4, "tracker id": "00000000"}))
put("http_announce", "dict-peers", b({"interval": 60, "peers": [{"ip": "192.0.2.20", "port": 6881, "peer id": "0" * 20}]}))
put("http_announce", "failure", b({"failure reason": "seed refusal"}))
put("http_announce", "warning-v6", b({"interval": 1800, "peers": b"", "peers6": bytes.fromhex("20010db8000000000000000000000001") + struct.pack(">H", 6881),
                                       "warning message": "seed warning"}))
put("http_scrape", "one", b({"files": {b"\x00" * 20: {"complete": 3, "downloaded": 9, "incomplete": 1}}}))
put("http_scrape", "two", b({"files": {b"\x00" * 20: {"complete": 0, "incomplete": 0}, b"\x01" * 20: {"complete": 7, "incomplete": 4}}}))
put("http_scrape", "empty", b({"files": {}}))

# UDP tracker: first byte = selector (kind in the low 2 bits, 0x80 = take the tx
# from the packet), then the reply. tx 0 is used when 0x80 is clear.
def udp(action, tx, body=b""):
    return struct.pack(">II", action, tx) + body

put("udp_tracker", "connect", bytes([0x80]) + udp(0, 7, struct.pack(">Q", 0x41727101980)))
put("udp_tracker", "announce4", bytes([0x81]) + udp(1, 7, struct.pack(">III", 1800, 2, 5) + peers4 * 2))
put("udp_tracker", "announce6", bytes([0x82]) + udp(1, 7, struct.pack(">III", 1800, 2, 5) + v6))
put("udp_tracker", "scrape", bytes([0x83]) + udp(2, 7, struct.pack(">III", 5, 9, 2) * 3))
put("udp_tracker", "error", bytes([0x81]) + udp(3, 7, b"seed error"))
put("udp_tracker", "zero-tx", bytes([0x00]) + udp(0, 0, struct.pack(">Q", 1)))
