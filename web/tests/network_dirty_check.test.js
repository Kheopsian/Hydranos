// The Network tab's unsaved-changes check. Run with:
//   node web/tests/network_dirty_check.test.js
// (from the repository root). Pulls the helpers out of app.js, so the test
// reads the shipped code, not a copy of it.
"use strict";
const fs = require("fs");
const path = require("path");
const assert = require("assert");

const src = fs.readFileSync(path.join(__dirname, "..", "static", "app.js"), "utf8");
const grab = (head) => {
    const start = src.indexOf(head);
    assert.ok(start >= 0, head + " not found in app.js");
    // A function or a const ends at the first line that is a lone closing brace.
    const end = src.indexOf("\n}", start);
    return src.slice(start, end + 2) + (head.startsWith("const") ? ";" : "");
};
const code = [
    grab("function _stableJson("),
    grab("const _NET_MODE_FIELDS = {"),
    grab("function _netOwnedFields("),
].join("\n");
const { _stableJson, _netOwnedFields } = new Function(code + "\nreturn { _stableJson, _netOwnedFields };")();

// What the page reads back from a form whose gluetun block is hidden: the
// stored value, or "hoard" when the stored one is empty (netModeCollect).
const collectHidden = (prev) => Object.assign({}, prev, {
    gluetun_port_engine: prev.gluetun_port_engine || "hoard",
});

const server = {
    race_listen_port: 6881, hoard_listen_port: 6882, enable_ipv6: false,
    race_bind_interface: "", hoard_bind_interface: "",
    socks5_host: "", socks5_port: 0, socks5_user: "", socks5_pass: "",
    race_proxy_v2_port: 0, hoard_proxy_v2_port: 0, proxy_v2_listen_addr: "",
    proxy_v2_trusted_sources: [],
    gluetun_port_forward: false, gluetun_url: "", gluetun_api_key: "", gluetun_port_engine: "hoard",
    announce_proxy: "", announce_ip: "",
};

for (const mode of ["direct", "wireguard", "socks5", "proxy_v2"]) {
    // A save keeps only what the mode owns: the gluetun engine goes blank.
    const saved = _netOwnedFields(mode, collectHidden(server));
    assert.strictEqual(saved.gluetun_port_engine, "", mode);
    const orig = _stableJson({ mode, fields: saved });
    // ...and the hidden box reads it back as "hoard". Compared raw, the page
    // looked edited right after the save: the leave prompt that fired on a
    // page with nothing left to save.
    const after = collectHidden(saved);
    assert.notStrictEqual(_stableJson({ mode, fields: after }), orig, "the raw form does differ");
    assert.strictEqual(
        _stableJson({ mode, fields: _netOwnedFields(mode, after) }), orig,
        mode + ": compared as a save would send it, nothing is left to save",
    );
}

// Loaded from a file that still carries another mode's keys (a SOCKS5 host
// left in direct mode): not an edit either.
{
    const stale = Object.assign({}, server, { socks5_host: "10.0.0.1", socks5_port: 1080 });
    const orig = _stableJson({ mode: "direct", fields: _netOwnedFields("direct", stale) });
    assert.strictEqual(_stableJson({ mode: "direct", fields: _netOwnedFields("direct", collectHidden(stale)) }), orig);
}

// A real edit is still one.
{
    const orig = _stableJson({ mode: "direct", fields: _netOwnedFields("direct", server) });
    const edited = Object.assign({}, server, { race_listen_port: 7000 });
    assert.notStrictEqual(_stableJson({ mode: "direct", fields: _netOwnedFields("direct", edited) }), orig);
}

console.log("network_dirty_check: ok");
