// Race and Hoard show the same strip, in the same order, from one painter.
// Run with: node web/tests/engine_strip.test.js (from the repository root).
"use strict";
const fs = require("fs");
const path = require("path");
const assert = require("assert");
const src = fs.readFileSync(path.join(__dirname, "..", "static", "app.js"), "utf8");
const html = fs.readFileSync(path.join(__dirname, "..", "templates", "index.html"), "utf8").split("\r\n").join("\n");

// The two strips carry the same cells in the same order.
const cells = pfx => {
    const block = html.match(new RegExp(`<div class="engine-stats engine-${pfx}">([\\s\\S]*?)\\n            </div>`));
    assert.ok(block, pfx + " strip not found");
    return [...block[1].matchAll(/id="[a-z]+-st-([a-z]+)"/g)].map(m => m[1]);
};
const order = ["total", "up", "down", "ul", "ratio", "peers", "withpeers", "ann", "share"];
assert.deepStrictEqual(cells("race"), order);
assert.deepStrictEqual(cells("hoard"), order);

const from = src.indexOf("function _paintEngineStrip(");
const to = src.indexOf("\n}\n", from) + 3;
assert.ok(from > 0 && to > from);
function paint(pfx, d) {
    const els = {};
    new Function("els", "d", "pfx", `
        const fmtInt = n => String(n);
        const formatSpeed = n => n + " B/s";
        const formatCount = n => String(n);
        const esc = x => x;
        const t = (k, v) => v ? k.replace(/\{(\w+)\}/g, (_, n) => v[n]) : k;
        const document = { getElementById: id => (els[id] = els[id] || { innerHTML: "", parentElement: { title: "" } }) };
        ${src.slice(from, to)}
        _paintEngineStrip(pfx, d);`)(els, d, pfx);
    return els;
}

let e = paint("hoard", { total: 51184, uploading: 7, downloading: 21, upload_rate: 5000, ratio: 0.8123,
    peers: 475, swarm: 628, with_peers: 600, announced: 16762, avg_share: 0.0421 });
assert.strictEqual(e["hoard-st-total"].innerHTML, "51184");
assert.strictEqual(e["hoard-st-up"].innerHTML, "7");
assert.strictEqual(e["hoard-st-down"].innerHTML, "21");
assert.strictEqual(e["hoard-st-ul"].innerHTML, "5000 B/s");
assert.strictEqual(e["hoard-st-ratio"].innerHTML, "0.81");
assert.ok(e["hoard-st-peers"].innerHTML.startsWith("475 <small>/ 628"));
assert.strictEqual(e["hoard-st-withpeers"].innerHTML, "600");
assert.ok(e["hoard-st-ann"].innerHTML.startsWith("33&nbsp;%"), "33 % announced");
assert.strictEqual(e["hoard-st-share"].innerHTML, "4.2&nbsp;%");

// What the server does not know yet is a dash, never a zero.
e = paint("race", { total: 10, uploading: 0, downloading: null, upload_rate: 0, ratio: null,
    peers: 3, swarm: 0, with_peers: 2, announced: 10, avg_share: null });
assert.strictEqual(e["race-st-down"].innerHTML, "-");
assert.strictEqual(e["race-st-ratio"].innerHTML, "-");
assert.strictEqual(e["race-st-share"].innerHTML, "-");
assert.strictEqual(e["race-st-up"].innerHTML, "0", "a real zero stays a zero");
assert.strictEqual(e["race-st-ann"].innerHTML, "100&nbsp;%");
assert.strictEqual(e["race-st-peers"].innerHTML, "3");
console.log("engine_strip: ok");
