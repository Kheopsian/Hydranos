// The Performance tab's read-cache cards and the ZFS-only items. Run with:
// node web/tests/perf_read_cache.test.js (from the repository root).
"use strict";
const fs = require("fs");
const path = require("path");
const assert = require("assert");
const src = fs.readFileSync(path.join(__dirname, "..", "static", "app.js"), "utf8");
const from = src.indexOf("function _showZfsOnly(");
const to = src.indexOf("function _renderBenchmarkCards(");
assert.ok(from > 0 && to > from, "helpers not found in app.js");

const rc = (o = {}) => ({ hits: 0, reads: 0, prefetch_issued: 0, prefetch_wasted: 0, bytes: 100, budget_bytes: 1000, ...o });

// ARC cards and chart: hidden only when the daemon says there is no ZFS.
let zfsOnly = [{ style: {} }, { style: {} }];
const e = new Function("zfsOnly", `
    const document = { querySelectorAll: () => zfsOnly };
    ${src.slice(from, src.indexOf("let _rcPrev"))}
    return _showZfsOnly;`)(zfsOnly);
e(false);
assert.ok(zfsOnly.every(x => x.style.display === "none"), "no ZFS: hidden");
e(true);
assert.ok(zfsOnly.every(x => x.style.display === ""), "ZFS: shown");
e(undefined);
assert.ok(zfsOnly.every(x => x.style.display === ""), "a daemon that does not say keeps them");

// Rates are the difference between two frames, never the history since boot.
const els = {};
const render = new Function("els", `
    const formatBytes = n => n + "B";
    const document = { getElementById: id => (els[id] = els[id] || { textContent: "" }) };
    ${src.slice(src.indexOf("let _rcPrev"), to)}
    return _renderReadCache;`)(els);
render(rc({ hits: 1000000, reads: 500000 }), 0);
assert.strictEqual(els["bm-rc-size"].textContent, "100B / 1000B", "size and budget show at once");
assert.strictEqual(els["bm-rc-hit"], undefined, "the first frame has nothing to difference");
render(rc({ hits: 1000090, reads: 500010 }), 2000);
assert.strictEqual(els["bm-rc-hit"].textContent, "90.0%", "90 hits, 10 reads in the interval");
assert.strictEqual(els["bm-rc-reads"].textContent, "5.0/s", "10 reads over 2 s");
// An interval with no request leaves the hit rate as it was.
render(rc({ hits: 1000090, reads: 500010 }), 4000);
assert.strictEqual(els["bm-rc-hit"].textContent, "90.0%");
assert.strictEqual(els["bm-rc-reads"].textContent, "0.0/s");
// Read-ahead waste is the share of what was issued in the interval.
render(rc({ hits: 1000090, reads: 500010, prefetch_issued: 40, prefetch_wasted: 10 }), 6000);
assert.strictEqual(els["bm-rc-waste"].textContent, "25%");
// A counter that went back (a restart) is not a negative rate.
render(rc({ hits: 5, reads: 1 }), 8000);
assert.strictEqual(els["bm-rc-reads"].textContent, "0.0/s");
render(null, 9000);   // an older daemon: nothing to draw, nothing thrown

console.log("perf_read_cache: ok");
