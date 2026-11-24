// The engine's active_changed event drives the Actively Seeding list and its
// chip. Run with: node web/tests/hoard_active_events.test.js (repo root).
"use strict";
const fs = require("fs");
const path = require("path");
const assert = require("assert");
const src = fs.readFileSync(path.join(__dirname, "..", "static", "app.js"), "utf8");
const from = src.indexOf("function _applyActiveChanged(");
const to = src.indexOf("let _hoardMembershipTimer");
const fFrom = src.indexOf("const _HOARD_STATE_FACETS");
assert.ok(from > 0 && to > from, "_applyActiveChanged not found in app.js");

function env(filter, rows, extra = {}) {
    return new Function("filter", "rows", "extra", `
        const HOARD_RENDER_LIMIT = 500, HOARD_REFRESH_LIVE_MS = 5000;
        const _isLocalAgent = a => !a || a === "local";
        let _hoardStateFilter = filter, _hoardAllTorrents = rows, _hoardPageApplied = 1;
        let _hoardServerFiltered = 0, _hoardServerTotal = 0, _hoardFacets = { active: 0, category: { tv: 2 } };
        const painted = { counts: 0, renders: 0, asked: 0 };
        const _hoardOnlyStateFilter = () => !extra.narrowed;
        const _renderHoardCounts = () => { painted.counts++; };
        const _scheduleHoardRender = () => { painted.renders++; };
        const _scheduleHoardMembership = () => { painted.asked++; };
        ${src.slice(from, to)}
        return { apply: _applyActiveChanged, facets: _applyFacets, state: () => ({ rows: _hoardAllTorrents, f: _hoardServerFiltered, total: _hoardServerTotal, facets: _hoardFacets, painted }) };`)(filter, rows, extra);
}
const row = (h, state = "seeding", agent = "local-hoard") => ({ info_hash: h, agent, state });

// A join adds the row, a leave drops it, the chip and the count follow.
let e = env("__active__", [row("aa"), row("bb")]);
e.apply({ agent: "local-hoard", count: 2, added: [row("cc")], removed: ["aa"] });
let s = e.state();
assert.deepStrictEqual(s.rows.map(r => r.info_hash).sort(), ["bb", "cc"]);
assert.ok(s.rows.find(r => r.info_hash === "cc")._pg, "an added row is held like a page row");
assert.ok(s.painted.renders >= 1);
assert.strictEqual(s.painted.asked, 0, "no request for a plain active list");

// A row that is not a seeder never enters the Active list.
e = env("__active__", []);
e.apply({ agent: "local-hoard", count: 1, added: [row("dd", "downloading")], removed: [] });
assert.strictEqual(e.state().rows.length, 0);

// A remote node's copy of the same hash is not touched, and neither is the
// copy another engine of the same role holds.
e = env("__active__", [row("aa", "seeding", "node-2"), row("aa", "seeding", "local-vpn1")]);
e.apply({ agent: "local-hoard", count: 0, added: [], removed: ["aa"] });
assert.strictEqual(e.state().rows.length, 2);
e.apply({ agent: "local-vpn1", count: 0, added: [], removed: ["aa"] });
assert.deepStrictEqual(e.state().rows.map(r => r.agent), ["node-2"], "only the engine that stopped loses its row");

// Another filter on top, or another view: the rows are left to the page.
e = env("__active__", [row("aa")], { narrowed: true });
e.apply({ agent: "local-hoard", count: 9, added: [row("zz")], removed: ["aa"] });
assert.strictEqual(e.state().rows.length, 1);
e = env("", [row("aa")]);
e.apply({ agent: "local-hoard", count: 4, added: [row("zz")], removed: [] });
assert.strictEqual(e.state().rows.length, 1, "rows of another view are not touched");

// The chips are the server's counts, pushed. Under a state filter only the
// state-independent ones are taken, and the count beside the search follows.
const F = { all: 10, active: 3, tracker_error: 1, torrent_error: 2, pinned: 0, check_waiting: 0,
            state: { seeding: 6, seed_only: 1, downloading: 2, stopped: 1 }, category: { tv: 9 } };
e = env("__active__", []);
e.facets(F);
s = e.state();
assert.strictEqual(s.facets.active, 3);
assert.strictEqual(s.facets.all, 10);
assert.deepStrictEqual(s.facets.category, { tv: 2 }, "a family counted with the state applied is not taken");
assert.strictEqual(s.f, 3, "the count beside the search is the Active chip");
assert.strictEqual(s.total, 10);
assert.ok(s.painted.counts >= 1 && s.painted.renders >= 1);
e = env("seeding", []);
e.facets(F);
assert.strictEqual(e.state().f, 7, "Seeding counts seed only too");
e = env("downloading", []);
e.facets(F);
assert.strictEqual(e.state().f, 2);
e = env("", []);
e.facets(F);
s = e.state();
assert.deepStrictEqual(s.facets.category, { tv: 9 }, "with no state chosen every family is taken");
assert.strictEqual(s.f, 10);
// A narrower list owns its own numbers.
e = env("__active__", [], { narrowed: true });
e.facets(F);
assert.strictEqual(e.state().facets.active, 0, "a narrowed chip is not the library's count");
assert.strictEqual(e.state().painted.counts, 0);

console.log("hoard_active_events: ok");
