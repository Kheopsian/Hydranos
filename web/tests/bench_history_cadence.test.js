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

// The charts hold ~300 points whatever the span: a point is span/300 wide, and
// the history is not asked for again before one has gone by.
assert.strictEqual(every(900), 10000, "15 m: never faster than every 10 s");
assert.strictEqual(every(3600), 12000, "1 h: a point is 12 s");
assert.strictEqual(every(86400), 288000, "24 h: a point is 4.8 min");
assert.strictEqual(every(2592000), 300000, "30 d: capped at 5 min");
console.log("bench_history_cadence: ok");
