// What the category form sends on save. Run with: node web/tests/category_payload.test.js
// (from the repository root). Pulls the pure helper out of app.js, so the test
// reads the shipped code, not a copy of it.
"use strict";
const fs = require("fs");
const path = require("path");
const assert = require("assert");

const src = fs.readFileSync(path.join(__dirname, "..", "static", "app.js"), "utf8");
const start = src.indexOf("function _catBuildPayload(");
assert.ok(start >= 0, "_catBuildPayload not found in app.js");
// The function ends at the first line that is a lone closing brace.
const end = src.indexOf("\n}\n", start);
const _catBuildPayload = new Function(src.slice(start, end + 2) + "\nreturn _catBuildPayload;")();

const form = (over) => Object.assign({
    name: "tv", save_path: "/data/tv", mode: "hoard", graduate_to: "", transit: false,
    targets: [], spread: "", reserve_bytes: 0,
}, over);

// The server merges a PUT into the stored entry: a key left out keeps its
// stored value. Clearing the spread must therefore SEND the empty values, which
// the server drops, or the old spread survives the save.
{
    const stored = { name: "tv", mode: "hoard", targets: ["vpn1", "vpn2"], spread: "fill", reserve_bytes: 1 << 30 };
    const out = _catBuildPayload(stored, form());
    assert.deepStrictEqual(out.targets, [], "cleared targets are sent as []");
    assert.strictEqual(out.spread, "", "a cleared strategy is sent as an empty string");
    assert.strictEqual(out.reserve_bytes, 0, "a cleared reserve is sent as 0");
}

// A category nobody set a spread on is saved with empty values only: the
// server drops them, so a plain save turns nothing on.
{
    const out = _catBuildPayload({ name: "tv", mode: "hoard" }, form());
    assert.deepStrictEqual(out.targets, []);
    assert.strictEqual(out.spread, "");
    assert.strictEqual(out.reserve_bytes, 0);
}

// What the user set is sent as set.
{
    const out = _catBuildPayload(null, form({ targets: ["vpn1", "nas:vpn1"], spread: "count", reserve_bytes: 5 }));
    assert.deepStrictEqual(out.targets, ["vpn1", "nas:vpn1"]);
    assert.strictEqual(out.spread, "count");
    assert.strictEqual(out.reserve_bytes, 5);
}

// The 3.x routing keys the form does not show go back as they were stored.
{
    const stored = { name: "tv", mode: "hoard", placement: "x", agents: ["a"], strategy: "all", min_free_bytes: 7 };
    const out = _catBuildPayload(stored, form());
    assert.strictEqual(out.placement, "x");
    assert.deepStrictEqual(out.agents, ["a"]);
    assert.strictEqual(out.strategy, "all");
    assert.strictEqual(out.min_free_bytes, 7);
}

console.log("category_payload: ok");
