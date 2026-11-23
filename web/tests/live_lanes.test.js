// Live updates: race rows follow the stream, and a slow refresh never holds up
// another one. Run with: node web/tests/live_lanes.test.js (from the repository
// root). Pulls the helpers out of app.js, so the test reads the shipped code.
"use strict";
const fs = require("fs");
const path = require("path");
const assert = require("assert");

const src = fs.readFileSync(path.join(__dirname, "..", "static", "app.js"), "utf8");
function grab(name, kind = "function") {
    const start = src.indexOf(`${kind === "async" ? "async " : ""}function ${name}(`);
    assert.ok(start >= 0, `${name} not found in app.js`);
    return src.slice(start, src.indexOf("\n}\n", start) + 2);
}

// ── The stream patches race rows in place ───────────────────────────────────
{
    const env = new Function("rows", `
        const _STATUS_TO_STATE = { 0: "paused", 1: "checking_files", 2: "downloading", 3: "seeding" };
        const _isLocalAgent = a => !a || a === "local";
        let _raceTorrents = rows;
        ${grab("_applyRaceStats")}
        return _applyRaceStats;`);
    const rows = [
        { info_hash: "aa", agent: "local", state: "downloading", upload_rate: 0, progress: 0.1 },
        { info_hash: "bb", agent: "node-2", state: "seeding", upload_rate: 5 },
    ];
    const apply = env(rows);
    const moved = apply([
        { info_hash: "aa", status: 3, upload_rate: 900, download_rate: 0, total_uploaded: 7,
          total_downloaded: 9, peers_connected: 4, progress: 1, checking_progress: null },
        { info_hash: "bb", status: 3, upload_rate: 1 },       // another node's copy: untouched
        { info_hash: "zz", status: 3, upload_rate: 1 },       // not on screen: ignored
    ]);
    assert.strictEqual(moved, true);
    assert.strictEqual(rows[0].state, "seeding");
    assert.strictEqual(rows[0].upload_rate, 900);
    assert.strictEqual(rows[0].num_peers, 4);
    assert.strictEqual(rows[0].progress, 1);
    assert.strictEqual(rows[1].upload_rate, 5, "a remote node's row keeps its own figures");
    // A running check keeps the row under Checking whatever the engine status says.
    apply([{ info_hash: "aa", status: 0, checking_progress: 0.4, check_queued: true }]);
    assert.strictEqual(rows[0].state, "checking_files");
    assert.strictEqual(rows[0].check_queued, true);
    assert.strictEqual(apply([{ info_hash: "zz", status: 3 }]), false, "nothing on screen moved");
}

// ── A stalled lane does not hold up the others ──────────────────────────────
(async () => {
    const lane = new Function(`
        const document = { hidden: false };
        ${src.slice(src.indexOf("const _lanes = new Map();"), src.indexOf("async function poll()"))}
        return _lane;`)();
    let release;
    const stuck = new Promise(r => { release = r; });
    let ran = 0;
    const slow = lane("slow", 1000, () => stuck);          // never answers
    const t0 = Date.now();
    await lane("fast", 1000, async () => { ran++; });
    assert.strictEqual(ran, 1);
    assert.ok(Date.now() - t0 < 100, "another lane ran while the slow one was stuck");
    // The stuck lane is single-flight: a second call does not stack a request.
    let stacked = 0;
    await lane("slow", 1000, async () => { stacked++; });
    assert.strictEqual(stacked, 0, "a busy lane is skipped, not queued");
    // And a lane cannot fire again before its interval is up.
    await lane("fast", 60000, async () => { ran++; });
    assert.strictEqual(ran, 1, "a lane waits out its interval");
    release();
    await slow;
    console.log("live_lanes: ok");
})().catch(e => { console.error(e); process.exit(1); });
