"use strict";
const assert = require("assert");
const fs = require("fs");
const path = require("path");
const src = fs.readFileSync(path.join(__dirname, "../static/app.js"), "utf8").replace(/\r\n/g, "\n");
function fn(name) {
    const start = new RegExp("^(?:async )?function " + name + "\\(", "m").exec(src);
    assert.ok(start, name);
    return src.slice(start.index, src.indexOf("\n}", start.index) + 2);
}
async function pollBudget() {
    const calls = {};
    let now = 100000;
    const names = ["checkHealth", "updateOverview", "updateRaceTorrents", "loadRacePolicy",
        "updateHoardStats", "updateCategories", "updateBenchmark", "updateTrackers", "loadTrackerStats",
        "updateMagnets", "updateRecords", "_maybeUpdateExtraEngines", "updateTabBadges", "fetchPortForward", "updateNetPoly", "fetchPublicIp"];
    const env = { hidden: false, tab: "race", now: () => now };
    const definitions = names.map(name => `const ${name} = async () => { calls.${name} = (calls.${name} || 0) + 1; };`).join("\n");
    const helpers = src.includes("function _refreshUi(") ? `const _uiRefreshes = new Map(); ${fn("_refreshUi")}` : "";
    const poll = new Function("env", "calls", `
        const document = { get hidden() { return env.hidden; }, getElementById: () => null, querySelector: () => ({dataset: {tab: env.tab}}) };
        const Date = { now: env.now }, console = {error: () => {}};
        let _polling = false;
        const _liveStreamFresh = () => true;
        ${definitions} ${helpers} ${fn("poll")}
        return poll;
    `)(env, calls);
    for (let i = 0; i < 60; i++) { await poll(); now += 1000; }
    assert.ok(calls.checkHealth <= 2, `health requested ${calls.checkHealth} times/minute`);
    assert.ok(calls.loadRacePolicy <= 4, "drain panel must not poll every second");
    assert.equal(calls.updateOverview || 0, 0, "healthy SSE supplies status");
    env.tab = "benchmark";
    calls.updateBenchmark = 0;
    for (let i = 0; i < 60; i++) { await poll(); now += 1000; }
    assert.ok(calls.updateBenchmark <= 6, "do not download full chart histories every second");
    env.hidden = true;
    const before = JSON.stringify(calls);
    await poll();
    assert.equal(JSON.stringify(calls), before, "hidden pages do no polling");
}
async function readLifecycle() {
    const pending = [], timers = new Map();
    let timer = 0;
    const api = new Function("env", `
        const API_KEY = "test", _apiReads = new Map(), _apiReadWaiters = [];
        let _apiReadActive = 0;
        const setTimeout = cb => { env.timers.set(++env.timer, cb); return env.timer; };
        const clearTimeout = id => env.timers.delete(id);
        const fetch = env.fetch, promptLogin = () => {}, t = x => x;
        ${fn("api")} ${src.includes("function _apiRequest(") ? fn("_apiRequest") : ""}
        ${src.includes("function _acquireApiRead(") ? fn("_acquireApiRead") : ""}
        return api;
    `)({ timers, timer, fetch: (url, options) => new Promise((resolve, reject) => {
        if (options.signal?.aborted) { reject(new Error("aborted")); return; }
        pending.push({url, options, resolve, reject});
        options.signal?.addEventListener("abort", () => reject(new Error("aborted")), {once: true});
    }) });
    const a = api("/health"), b = api("/health");
    await Promise.resolve();
    assert.equal(pending.length, 1, "concurrent identical reads share one HTTP request");
    pending[0].resolve({ok: true, json: async () => ({ok: true})});
    await Promise.all([a, b]);
    assert.equal(timers.size, 0, "completed reads release their deadline timer");
    const c = api("/health");
    await Promise.resolve();
    assert.equal(pending.length, 2, "completed reads are not a stale cache");
    const deadline = [...timers.values()][0];
    assert.ok(deadline, "stalled reads have a deadline");
    deadline(); await assert.rejects(c);
    const controller = new AbortController();
    const d = api("/page", {signal: controller.signal});
    controller.abort(); await assert.rejects(d);
    const e = api("/change", {method: "POST"}), f = api("/change", {method: "POST"});
    assert.equal(pending.filter(x => x.url === "/change").length, 2, "mutations are never coalesced");
    for (const item of pending.filter(x => x.url === "/change")) {
        assert.equal(item.options.signal, undefined, "no automatic deadline on mutations");
        item.resolve({ok: true, json: async () => ({})});
    }
    await Promise.all([e, f]);
    const count = pending.length;
    const reads = Array.from({length: 6}, (_, i) => api("/unique/" + i));
    await Promise.resolve();
    assert.equal(pending.length - count, 3, "background reads leave browser connection capacity for actions and streams");
    const obsolete = new AbortController();
    const queued = api("/obsolete", {signal: obsolete.signal});
    const cancelled = assert.rejects(queued);
    obsolete.abort(); await cancelled;
    for (let i = count; i < count + 6; i++) {
        pending[i].resolve({ok: true, json: async () => ({})});
        await new Promise(r => setImmediate(r));
    }
    await Promise.all(reads);
    assert.ok(!pending.some(x => x.url === "/obsolete"), "cancelling a queued read never sends it to the backend");
}
async function slowRefreshes() {
    const env = {now: 1000, hidden: false};
    const refresh = new Function("env", `
        const _uiRefreshes = new Map(), document = env;
        const Date = {now: () => env.now}, console = {error: () => {}};
        ${fn("_refreshUi")} return _refreshUi;
    `)(env);
    let release, calls = 0, health = 0;
    const load = () => { calls++; return new Promise(r => {release = r;}); };
    const first = refresh("slow", 1000, load);
    await refresh("slow", 1000, load, true);
    await refresh("health", 1000, () => {health++;});
    assert.equal(calls, 1, "even forced refreshes cannot duplicate an in-flight task");
    assert.equal(health, 1, "one stalled view does not stop other lanes");
    env.now += 10000; release(); await first;
    env.now += 1000;
    await refresh("slow", 1000, load);
    assert.equal(calls, 1, "slow work backs off after completing");
    env.hidden = true;
    await refresh("new", 0, () => {throw new Error("hidden refresh ran");});
}
async function detailRequests() {
    const calls = [];
    const details = new Function("env", `
        const _detailReads = new Map(), api = env.api;
        ${fn("_cancelDetailRead")} ${fn("_readTorrentDetail")}
        return {read: _readTorrentDetail, cancel: _cancelDetailRead};
    `)({api: (url, {signal}) => new Promise((resolve, reject) => {
        calls.push({url, signal, resolve});
        signal.addEventListener("abort", () => reject(new Error("aborted")), {once: true});
    })});
    const a = details.read("hoard", "/first");
    const rejected = assert.rejects(a);
    assert.equal(details.read("hoard", "/first"), a, "detail timers share a pending request");
    const b = details.read("hoard", "/second");
    assert.equal(calls[0].signal.aborted, true, "selecting another torrent cancels the old request");
    await rejected;
    assert.equal(details.read("hoard", "/second"), b, "old cleanup cannot remove the replacement");
    const closed = assert.rejects(b);
    details.cancel("hoard"); await closed;
}
function renderBursts() {
    const env = {timers: [], paints: 0, visible: true};
    const schedule = new Function("env", `
        let _hoardRenderScheduled = false, _hoardRenderDirty = false;
        const document = {hidden: false}, _hoardTabVisible = () => env.visible;
        const requestAnimationFrame = cb => cb(), setTimeout = cb => env.timers.push(cb);
        const renderHoardTable = () => {env.paints++;};
        ${fn("_scheduleHoardRender")} return _scheduleHoardRender;
    `)(env);
    for (let i = 0; i < 100; i++) schedule();
    assert.equal(env.timers.length, 1, "a burst of SSE batches schedules one table repaint");
    env.timers.shift()(); assert.equal(env.paints, 1);
    schedule(); env.visible = false; env.timers.shift()();
    assert.equal(env.paints, 1, "switching tabs cancels deferred table work");
}
async function sessionRequests() {
    let calls = 0, release;
    const ensure = new Function("env", `
        const API_KEY = "test", fetch = env.fetch;
        let _sessionReady = null, _sessionPending = false;
        ${fn("ensureSession")} return ensureSession;
    `)({fetch: () => {calls++; return new Promise(r => {release = r;});}});
    const a = ensure(true), b = ensure(true);
    assert.equal(calls, 1, "simultaneous stream reconnects share session verification");
    release({ok: true}); await Promise.all([a, b]);
    const c = ensure(true);
    assert.equal(calls, 2, "a later reconnect can revalidate a completed session");
    release({ok: true}); await c;
}
Promise.all([pollBudget(), readLifecycle(), slowRefreshes(), detailRequests(), sessionRequests(), Promise.resolve().then(renderBursts)]).then(() => console.log("request_lifecycle: ok"))
    .catch(e => { console.error(e); process.exitCode = 1; });
