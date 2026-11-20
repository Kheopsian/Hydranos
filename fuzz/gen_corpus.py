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

# ---- MSE handshake -------------------------------------------------------
# Byte 0 = selector (bit 0: we initiate, bit 1: RC4 only, bit 2: raw), bytes
# 1..3 = padding length, then the padding and the PLAINTEXT of the encrypted
# part: the target encrypts it with the fixed session keys (see the target).
bt_hs = (bytes([19]) + b"BitTorrent protocol" + bytes([0, 0, 0, 0, 0, 0x10, 0, 0x04]) +
         b"\x33" * 20 + b"-XX0000-" + b"0" * 12)


def mse(sel, pad, tail):
    return bytes([sel]) + struct.pack(">H", len(pad)) + pad + tail


vc = b"\x00" * 8
# incoming: VC, crypto_provide, len(padC), padC, len(IA), IA
put("mse_handshake", "in-rc4-plain", mse(0, b"\x07" * 5, vc + struct.pack(">IH", 3, 4) + b"\x01" * 4 +
                                         struct.pack(">H", 68) + bt_hs))
put("mse_handshake", "in-plain-only", mse(0, b"", vc + struct.pack(">IH", 1, 0) + struct.pack(">H", 68) + bt_hs))
put("mse_handshake", "in-rc4-only", mse(2, b"", vc + struct.pack(">IH", 2, 0) + struct.pack(">H", 68) + bt_hs))
put("mse_handshake", "in-no-ia", mse(0, b"", vc + struct.pack(">IH", 2, 0) + struct.pack(">H", 0)))
# outgoing: VC, crypto_select, len(padD), padD, the BitTorrent handshake
put("mse_handshake", "out-rc4", mse(1, b"\x09" * 7, vc + struct.pack(">IH", 2, 3) + b"\x02" * 3 + bt_hs))
put("mse_handshake", "out-plain", mse(1, b"", vc + struct.pack(">IH", 1, 0) + bt_hs))
put("mse_handshake", "out-rc4-only-plain-selected", mse(3, b"", vc + struct.pack(">IH", 1, 0) + bt_hs))
put("mse_handshake", "raw-garbage", mse(4, b"", b"\x00" * 120))

# ---- LSD (BEP 14) -----------------------------------------------------------
ih1 = "00" * 20
ih2 = "ab" * 20
put("lsd_announce", "one", ("BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 6881\r\n"
                            "Infohash: %s\r\ncookie: seed-cookie\r\n\r\n\r\n" % ih1).encode())
put("lsd_announce", "two-bare-lf", ("BT-SEARCH * HTTP/1.1\nHost: [ff15::efc0:988f]:6771\nport: 51413\n"
                                    "Infohash: %s\nInfoHash: %s\n\n" % (ih1, ih2.upper())).encode())
put("lsd_announce", "v2-hash-ignored", ("BT-SEARCH * HTTP/1.1\r\nPort: 6881\r\nInfohash: %s\r\n"
                                        "Infohash: %s\r\n\r\n" % ("11" * 32, ih2)).encode())
put("lsd_announce", "other-protocol", b"M-SEARCH * HTTP/1.1\r\nHost: 239.255.255.250:1900\r\n\r\n")

# ---- PROXY protocol v2 ----------------------------------------------------------
sig = bytes([0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A])
a4 = bytes([192, 0, 2, 1]) + bytes([198, 51, 100, 2]) + struct.pack(">HH", 51413, 6881)
a6 = (bytes.fromhex("20010db8000000000000000000000001") + bytes.fromhex("20010db8000000000000000000000002") +
      struct.pack(">HH", 51413, 6881))
put("proxy_protocol", "v4", sig + bytes([0x21, 0x11]) + struct.pack(">H", len(a4)) + a4)
put("proxy_protocol", "v6", sig + bytes([0x21, 0x21]) + struct.pack(">H", len(a6)) + a6)
put("proxy_protocol", "v4-tlv", sig + bytes([0x21, 0x11]) + struct.pack(">H", len(a4) + 7) + a4 + b"\x04\x00\x04abcd")
put("proxy_protocol", "local", sig + bytes([0x20, 0x00, 0x00, 0x00]))
put("proxy_protocol", "udp-unsupported", sig + bytes([0x21, 0x12]) + struct.pack(">H", len(a4)) + a4)

# ---- magnet links -----------------------------------------------------------------
H1 = "00" * 20
put("magnet_uri", "hex", ("magnet:?xt=urn:btih:%s&dn=seed+file%%20name&tr=udp%%3A%%2F%%2Ftracker.example.invalid%%3A6969"
                          "%%2Fannounce&tr.1=http%%3A%%2F%%2Ftracker.example.invalid%%2Fannounce&x.pe=192.0.2.5:6881" % H1).encode())
put("magnet_uri", "base32", b"MAGNET:?xt=urn:btih:" + b"A" * 32 + b"&dn=seed")
put("magnet_uri", "hybrid", ("magnet:?xt=urn:btih:%s&xt=urn:btmh:1220%s&dn=seed" % (H1, "00" * 32)).encode())
put("magnet_uri", "v2-only", ("magnet:?xt=urn:btmh:1220%s" % ("00" * 32)).encode())
put("magnet_uri", "no-hash", b"magnet:?dn=nothing&tr=udp://tracker.example.invalid:1")
put("magnet_uri", "not-a-magnet", b"http://host.example.invalid/x.torrent")

# ---- RSS / Atom ---------------------------------------------------------------------
put("rss_parse", "rss2", b"\x00" + b"""<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed"><channel><title>Seed feed</title>
<item><title>Seed &amp; &#65; item</title><guid>g-1</guid><link>http://host.example.invalid/page/1</link>
<description><![CDATA[<b>seed</b> text]]></description><pubDate>Sat, 01 Jan 2000 00:00:00 +0000</pubDate>
<category>Seed</category><author>nobody@example.invalid</author>
<enclosure url="http://host.example.invalid/1.torrent" length="12345" type="application/x-bittorrent"/>
<torznab:attr name="size" value="12345"/></item>
<item><title>Magnet item</title><link>magnet:?xt=urn:btih:""" + H1.encode() + b"""</link></item>
</channel></rss>""")
put("rss_parse", "atom", b"\x00" + b"""<feed xmlns="http://www.w3.org/2005/Atom"><title>Seed atom</title>
<entry><title>Entry</title><id>urn:seed:1</id><updated>2000-01-01T00:00:00Z</updated>
<link rel="enclosure" href="http://host.example.invalid/2.torrent" length="2048"/>
<link rel="alternate" href="http://host.example.invalid/page/2"/><summary>seed</summary>
<author><name>nobody</name></author></entry></feed>""")
put("rss_parse", "rdf", b"\x00" + b"""<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
xmlns="http://purl.org/rss/1.0/"><channel><title>Seed rdf</title></channel>
<item><title>R</title><link>http://host.example.invalid/r</link></item></rdf:RDF>""")
put("rss_parse", "date-2822", b"\x01Sat, 01 Jan 2000 00:00:00 +0000")
put("rss_parse", "date-3339", b"\x012000-01-01T00:00:00+02:00")

# ---- IP filter ----------------------------------------------------------------------------
probe = bytes([192, 0, 2, 1]) + b"\x00" * 12
put("ipfilter_parse", "mixed", probe + b"""# seed list
Seed range:192.0.2.0-192.0.2.255
198.51.100.0/24
2001:db8::/32
203.0.113.5
2001:db8:1::1 - 2001:db8:1::ff
""")
put("ipfilter_parse", "emule-dat", probe + b"""192.000.002.000 - 192.000.002.255 , 000 , seed
198.051.100.000 - 198.051.100.255 , 200 , allowed
""")
put("ipfilter_parse", "peerguardian", probe + b"Seed:name:with:colons:192.0.2.1-192.0.2.9\nbroken line\n")

# ---- BEP 55 hole punch ----------------------------------------------------------------------
put("holepunch", "rendezvous-v4", bytes([0, 0]) + bytes([192, 0, 2, 7]) + struct.pack(">H", 6881))
put("holepunch", "connect-v4-public", bytes([1, 0]) + bytes([198, 51, 100, 9]) + struct.pack(">H", 6881))
put("holepunch", "connect-v6", bytes([1, 1]) + bytes.fromhex("20010db8000000000000000000000009") + struct.pack(">H", 6881))
put("holepunch", "error", bytes([2, 0]) + bytes([192, 0, 2, 7]) + struct.pack(">HI", 6881, 1))

# ---- TOML editor: doc NUL section NUL key NUL value NUL id ---------------------------------------
cfg = b"""# Seed config
[daemon]
port = 8080   # the API port
name = "seed"

[tracker."tracker.example.invalid"]
user = "nobody"

[[engine]]
name = "e1"
engine_id = "e1"
role = "race"
  [engine.session]
  rate = 5

[[engine]]
name = "e2"
role = "hoard"
"""
put("tomledit", "set-value", cfg + b"\x00daemon\x00port\x009090\x00e1")
put("tomledit", "quoted-key", cfg + b"\x00tracker.\"tracker.example.invalid\"\x00user\x00\"someone\"\x00e2")
put("tomledit", "new-table", cfg + b"\x00race\x00rate\x007\x00e1")
put("tomledit", "agent-session", cfg + b"\x00\x00rate\x009\x00e2")
put("tomledit", "empty", b"\x00\x00\x00\x00")
