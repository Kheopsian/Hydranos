// The engine's active_changed event drives the Actively Seeding list and its
// chip. Run with: node web/tests/hoard_active_events.test.js (repo root).
"use strict";
const fs = require("fs");
const path = require("path");
const assert = require("assert");
const src = fs.readFileSync(path.join(__dirname, "..", "static", "app.js"), "utf8");
const from = src.indexOf("function _applyActiveChanged(");
const to = src.indexOf("let _hoardMembershipTimer");
assert.ok(from > 0 && to > from, "_applyActiveChanged not found in app.js");

function env(filter, rows, extra = {}) {
    return new Function("filter", "rows", "extra", `
        const HOARD_RENDER_LIMIT = 500, HOARD_REFRESH_LIVE_MS = 5000;
        const _isLocalAgent = a => !a || a === "local";
        let _hoardStateFilter = filter, _hoardAllTorrents = rows, _hoardPageApplied = 1;
        let _hoardServerFiltered = 0, _hoardFacets = { active: 0 };
        const painted = { counts: 0, renders: 0, asked: 0 };
        const _hoardOnlyStateFilter = () => !extra.narrowed;
        const _renderHoardCounts = () => { painted.counts++; };
        const _scheduleHoardRender = () => { painted.renders++; };
        const _scheduleHoardMembership = () => { painted.asked++; };
        ${src.slice(from, to)}
        return { apply: _applyActiveChanged, state: () => ({ rows: _hoardAllTorrents, f: _hoardServerFiltered, facets: _hoardFacets, painted }) };`)(filter, rows, extra);
}
const row = (h, state = "seeding") => ({ info_hash: h, agent: "local", state });

// A join adds the row, a leave drops it, the chip and the count follow.
let e = env("__active__", [row("aa"), row("bb")]);
e.apply({ count: 2, added: [row("cc")], removed: ["aa"] });
let s = e.state();
assert.deepStrictEqual(s.rows.map(r => r.info_hash).sort(), ["bb", "cc"]);
assert.ok(s.rows.find(r => r.info_hash === "cc")._pg, "an added row is held like a page row");
assert.strictEqual(s.facets.active, 2);
assert.strictEqual(s.f, 2);
assert.ok(s.painted.renders >= 1 && s.painted.counts >= 1);
assert.strictEqual(s.painted.asked, 0, "no request for a plain active list");

// A row that is not a seeder never enters the Active list.
e = env("__active__", []);
e.apply({ count: 1, added: [row("dd", "downloading")], removed: [] });
assert.strictEqual(e.state().rows.length, 0);

// A remote node's copy of the same hash is not touched.
e = env("__active__", [{ info_hash: "aa", agent: "node-2", state: "seeding" }]);
e.apply({ count: 0, added: [], removed: ["aa"] });
assert.strictEqual(e.state().rows.length, 1);

// Another filter on top, or another state: the chip may not move for a
// narrowed count, and the rows are left to the page.
e = env("__active__", [row("aa")], { narrowed: true });
e.apply({ count: 9, added: [row("zz")], removed: ["aa"] });
s = e.state();
assert.strictEqual(s.facets.active, 0, "a narrowed chip is not the library's count");
assert.strictEqual(s.rows.length, 1);
e = env("", [row("aa")]);
e.apply({ count: 4, added: [row("zz")], removed: [] });
s = e.state();
assert.strictEqual(s.facets.active, 4, "the chip follows on any plain view");
assert.strictEqual(s.rows.length, 1, "rows of another view are not touched");

console.log("hoard_active_events: ok");
