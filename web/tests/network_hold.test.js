"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const src = fs.readFileSync(path.join(__dirname, "../static/app.js"), "utf8").replace(/\r\n/g, "\n");
const start = src.indexOf("function _netEngineDisplay(");
const body = src.slice(start, src.indexOf("\n}", start) + 2);
const display = new Function("t", "_netStateWord", `${body}; return _netEngineDisplay;`)(
    (s, args = {}) => s.replace(/\{(\w+)\}/g, (_, k) => args[k]), s => s
);
const engine = { state: "ok", listen_port: 16171, announced_port: 45000 };
assert.equal(display(engine).state, "ok");
assert.match(display(engine).ports, /listening port 16171 · announced port 45000/);
const pending = display({ ...engine, port_pending: true });
assert.equal(pending.state, "warn", "successful exit probes must not hide an announce hold");
assert.match(pending.word, /waiting for forwarded port/);
assert.match(pending.ports, /announced port pending/);
assert.ok(!pending.ports.includes("45000"), "a pending port must not appear ready");
assert.match(display({ ...engine, announce_held: true }).word, /startup pause/);
assert.equal(display({ ...engine, state: "bad", port_pending: true }).state, "bad");
assert.equal(display({ ...engine, state: "off", port_pending: true }).state, "off");
console.log("network announce hold tests passed");
