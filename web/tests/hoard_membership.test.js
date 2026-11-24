// A torrent crossing the line the hoard page was filtered on is noticed from
// the stream. Run with: node web/tests/hoard_membership.test.js (repo root).
"use strict";
const fs = require("fs");
const path = require("path");
const assert = require("assert");
const src = fs.readFileSync(path.join(__dirname, "..", "static", "app.js"), "utf8");
const from = src.indexOf("function _hoardWantsState(");
const to = src.indexOf("let _hoardMembershipTimer");
assert.ok(from > 0 && to > from, "membership helpers not found in app.js");
const make = (state, sort, rows, other = {}) => new Function("st", "sort", "rows", "other", `
    const document = { getElementById: () => ({ value: other.search || "" }) };
    const _hoardCatInc = other.cat || [], _hoardCatExc = [], _hoardTagInc = [], _hoardTagExc = [],
          _hoardTrackerInc = other.tracker || [], _hoardTrackerExc = [], _hoardErrInc = [], _hoardErrExc = [];
    const _STATUS_TO_STATE = { 0: "paused", 1: "checking_files", 2: "downloading", 3: "seeding" };
    const HOARD_RENDER_LIMIT = 2;
    let _hoardStateFilter = st, _hoardSortCol = sort, _hoardSortAsc = false, _hoardAllTorrents = rows;
    ${src.slice(from, to)}
    return { _hoardJoins, _hoardLeaves };`)(state, sort, rows, other);

let h = make("__active__", "added_time", []);
assert.ok(h._hoardJoins({ status: 3, upload_rate: 5 }), "a seeder that starts uploading joins Active");
assert.ok(!h._hoardJoins({ status: 3, upload_rate: 0 }));
assert.ok(h._hoardLeaves({}, { status: 3, upload_rate: 0 }), "a seeder gone idle leaves Active");
assert.ok(!h._hoardLeaves({}, { status: 3, upload_rate: 9 }));

h = make("seeding", "added_time", [{}, {}]);   // full page (limit 2)
assert.ok(!h._hoardJoins({ status: 3, checking_progress: null }),
    "on a full page an off-page seeder is every frame, not a crossing");
h = make("__active__", "added_time", [{}, {}]);
assert.ok(!h._hoardJoins({ status: 3, upload_rate: 5 }));
assert.ok(h._hoardLeaves({}, { status: 3, upload_rate: 0 }), "a row on the page leaving still counts");

h = make("downloading", "added_time", []);
assert.ok(h._hoardJoins({ status: 2, checking_progress: null }));
assert.ok(h._hoardLeaves({}, { status: 3, checking_progress: null }));

h = make("", "upload_rate", [{ upload_rate: 10 }, { upload_rate: 50 }]);
assert.ok(h._hoardJoins({ upload_rate: 99 }), "faster than the slowest row of a full page");
assert.ok(!h._hoardJoins({ upload_rate: 3 }));

h = make("", "added_time", []);
assert.ok(!h._hoardJoins({ status: 3, upload_rate: 99 }), "no live filter, nothing to ask for");
// A state filter next to a search, category or tracker: the stream cannot say
// whether an off-page torrent matches those, so it never counts as a join.
for (const other of [{ search: "x" }, { cat: ["tv"] }, { tracker: ["t"] }]) {
    h = make("__active__", "added_time", [{}], other);
    assert.ok(!h._hoardJoins({ status: 3, upload_rate: 5 }), "no join with " + JSON.stringify(other));
}
h = make("__active__", "added_time", [{}]);
assert.strictEqual(h._hoardJoins({ status: 3, upload_rate: 5 }), 1);

// A rate sort reports its own kind (2), which the stream handler spaces out by
// the poll interval instead of one second.
h = make("", "upload_rate", [{ upload_rate: 10 }, { upload_rate: 50 }]);
assert.strictEqual(h._hoardJoins({ upload_rate: 99 }), 2);

console.log("hoard_membership: ok");
