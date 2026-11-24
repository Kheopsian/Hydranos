// How often the Performance charts ask for their history. Run with:
// node web/tests/bench_history_cadence.test.js (from the repository root).
"use strict";
const fs = require("fs");
const path = require("path");
const assert = require("assert");

const src = fs.readFileSync(path.join(__dirname, "..", "static", "app.js"), "utf8");
const start = src.indexOf("function _bmHistoryEvery(");
assert.ok(start >= 0, "_bmHistoryEvery not found in app.js");
const body = src.slice(start, src.indexOf("\n}\n", start) + 2);
const every = span => new Function("_bmSpan", `${body}\nreturn _bmHistoryEvery();`)(span);

// The history is asked for again every span / 60, within 15 s and 5 min.
assert.strictEqual(every(900), 15000, "15 m: every 15 s");
assert.strictEqual(every(3600), 60000, "1 h: every minute");
assert.strictEqual(every(21600), 300000, "6 h: 6 min, capped at 5");
assert.strictEqual(every(86400), 300000, "24 h: capped at 5 min");
assert.strictEqual(every(2592000), 300000, "30 d: capped at 5 min");
console.log("bench_history_cadence: ok");
