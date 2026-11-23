"use strict";
const fs = require("fs");
const path = require("path");
const assert = require("assert");
const src = fs.readFileSync(path.join(__dirname, "../static/app.js"), "utf8").replace(/\r\n/g, "\n");
function fn(name) {
    const match = new RegExp("^(?:async )?function " + name + "\\(", "m").exec(src);
    assert.ok(match, name);
    return src.slice(match.index, src.indexOf("\n}", match.index) + 2);
}
function intervals(state, now = 10000) {
    return new Function("s", "Date", `
        const HOARD_REFRESH_MS = 20000, LIVE_REFRESH_MS = 1000;
        const _hoardStateFilter = s.filter, _hoardFacets = s.facets;
        const _hoardMembershipDirty = s.membership;
        const _hoardFullFetchAt = s.fullAt, _hoardFullFetchMs = s.fullMs, _hoardLiveFetchMs = s.liveMs;
        const _sseConn = s.conn, _lastStatusEventAt = s.status, _lastHoardStatsEventAt = s.stats;
        ${fn("_liveStreamFresh")}
        ${fn("_hoardFullRefreshInterval")}
        ${fn("_hoardFastRefresh")}
        ${fn("_hoardRefreshInterval")}
        return { fast: !!_hoardFastRefresh(), interval: _hoardRefreshInterval() };
    `)(state, { now: () => now });
}
const state = { filter: "__active__", facets: {}, membership: false, fullAt: 9000,
    fullMs: 100, liveMs: 10, conn: { readyState: 1 }, status: 10000, stats: 10000 };
for (const filter of ["__active__", "downloading", "seed_only", "checking_files", "__check_waiting__", "__check_reading__", "__error__", "error"]) {
    assert.deepEqual(intervals({ ...state, filter }), { fast: true, interval: 1000 }, filter + " must use indexed membership");
}
assert.equal(intervals({ ...state, liveMs: 900 }).interval, 9000, "slow active queries back off");
assert.equal(intervals({ ...state, filter: "" }).interval, 20000, "no whole-library scan every second");
assert.equal(intervals({ ...state, filter: "", fullMs: 2000 }).interval, 40000, "slow full scans back off");
assert.equal(intervals({ ...state, fullAt: 0 }, 30000).fast, false, "facets get a periodic full refresh");
assert.equal(intervals({ ...state, conn: null }).fast, true, "disconnected active views keep using the cheap index");
assert.equal(intervals({ ...state, stats: 4000 }).fast, true, "silent active streams also have a cheap fallback");
assert.equal(intervals({ ...state, membership: true }).fast, false, "adds and missed events repair membership");
const apply = new Function(`
    let _liveRowRevision = 0;
    const _STATUS_TO_STATE = { 0: "paused", 2: "downloading", 3: "seeding", 4: "error" };
    ${fn("_isLocalAgent")}
    ${fn("_liveRowMatches")}
    ${fn("_applyLiveRows")}
    return _applyLiveRows;
`)();
const rows = ["local-hoard", "local-vpn", "remote-hoard"].map(agent => ({ agent, info_hash: "same", state: "seeding" }));
const mini = { info_hash: "same", state: "seed_only", status: 2, upload_rate: 0,
    download_rate: 20, total_uploaded: 100, total_downloaded: 50, peers_connected: 2,
    progress: 0.5, ratio: 2, checking_progress: null, check_queued: false };
assert.equal(apply(rows, { agent: "local-hoard", torrents: [mini] }), 1);
assert.equal(rows[0].state, "seed_only");
assert.equal(rows[0].ratio, 2);
assert.equal(rows[0].upload_rate, 0, "an upload that stops reaches zero");
assert.equal(rows[1].state, "seeding", "another local engine is untouched");
assert.equal(rows[2].state, "seeding", "remote rows are untouched");
apply(rows, { agent: "local-hoard", torrents: [{ ...mini, state: "stopped" }] });
assert.equal(rows[0].state, "stopped", "server-derived user intent survives the stream");
apply(rows, { agent: "local-hoard", torrents: [{ ...mini, checking_progress: 0.4, check_queued: true }] });
assert.equal(rows[0].state, "checking_files");
assert.equal(rows[0].check_queued, true);
assert.equal(apply([], { torrents: [mini] }), 0);

async function raceRequests() {
    let now = 10000, fresh = true;
    const calls = [];
    let release;
    const race = new Function("env", `
        let _racePageBusy = false, _raceMembershipDirty = false;
        let _raceLastFetchAt = 9000, _raceFetchMs = 0, _raceTorrents = [];
        let _liveRowRevision = 0;
        const HOARD_REFRESH_MS = 20000;
        const Date = { now: () => env.now() };
        const _liveStreamFresh = env.fresh, api = env.api;
        const loadRacePolicy = () => {}, renderRaceTable = () => {};
        ${fn("_preserveLiveRows")}
        ${fn("updateRaceTorrents")}
        return { updateRaceTorrents, changed: () => { _raceMembershipDirty = true; } };
    `)({ now: () => now, fresh: () => fresh, api: async url => {
        calls.push(url);
        await new Promise(r => { release = r; });
        return [];
    } });
    await race.updateRaceTorrents(false);
    assert.equal(calls.length, 0, "healthy SSE avoids repeated full Race downloads");
    race.changed();
    const first = race.updateRaceTorrents(false);
    await race.updateRaceTorrents(false);
    assert.equal(calls.length, 1, "adds refresh Race once, without overlapping requests");
    release(); await first;
    now += 1000;
    fresh = false;
    const recovery = race.updateRaceTorrents(false);
    assert.equal(calls.length, 2, "a stale stream falls back to REST");
    release(); await recovery;
    fresh = true;
    const action = race.updateRaceTorrents();
    assert.equal(calls.length, 3, "user actions bypass the background interval");
    release(); await action;
}

async function requests() {
    let now = 10000, fast = true;
    const calls = [];
    let release;
    const fetch = new Function("env", `
        let _selAll = null, _hoardPageBusy = 0, _hoardInFlightQuery = "";
        let _hoardLiveDirty = true, _hoardMembershipDirty = false;
        let _hoardLastFetchAt = 0, _hoardFullFetchAt = 9000;
        let _hoardPageAbort = null;
        let _hoardLiveFetchMs = 0, _hoardFullFetchMs = 0;
        const Date = { now: () => env.now() };
        const _hoardFastRefresh = () => env.fast();
        const _liveStreamFresh = () => true;
        const _hoardRefreshInterval = () => 1000;
        const _hoardPageQuery = facets => "facets=" + Number(facets);
        const _fetchHoardPageInner = env.request;
        ${fn("fetchHoardPage")}
        return { fetchHoardPage, dirty: () => { _hoardLiveDirty = true; } };
    `)({ now: () => now, fast: () => fast, request: async qs => {
        calls.push(qs);
        await new Promise(r => { release = r; });
        return true;
    } });
    const first = fetch.fetchHoardPage();
    assert.deepEqual(calls, ["facets=0&live=1"]);
    await fetch.fetchHoardPage();
    assert.equal(calls.length, 1, "only one automatic request in flight");
    release(); await first;
    now += 2000;
    await fetch.fetchHoardPage();
    assert.equal(calls.length, 1, "unchanged active pages make no requests, even when empty");
    fetch.dirty();
    const changed = fetch.fetchHoardPage();
    release(); await changed;
    assert.equal(calls.length, 2, "a delta requests fresh active membership");
    const forced = fetch.fetchHoardPage(true);
    assert.equal(calls[2], "facets=1", "user actions refresh facets immediately");
    release(); await forced;
}
async function pageCannotOverwriteLiveRows() {
    let now = 10000, release;
    const page = new Function("env", `
        let _hoardPageSeq = 0, _hoardPageApplied = 0, _liveRowRevision = 0;
        let _hoardAllTorrents = [{ agent: "local-hoard", info_hash: "same", upload_rate: 10 }];
        let _hoardAppliedSearch, _hoardServerTotal, _hoardServerFiltered, _hoardFacets;
        const _STATUS_TO_STATE = { 3: "seeding" }, LIVE_REFRESH_MS = 1000;
        const Date = { now: env.now }, document = { getElementById: () => null };
        const api = env.request, _hoardPageQuery = () => "facets=1";
        const renderHoardTable = () => {}, _renderHoardCounts = () => {};
        ${fn("_isLocalAgent")}
        ${fn("_liveRowMatches")}
        ${fn("_applyLiveRows")}
        ${src.includes("function _preserveLiveRows(") ? fn("_preserveLiveRows") : ""}
        ${fn("_fetchHoardPageInner")}
        return { fetch: _fetchHoardPageInner, rows: () => _hoardAllTorrents,
            delta: () => _applyLiveRows(_hoardAllTorrents, { agent: "local-hoard", torrents: [{
                info_hash: "same", state: "seeding", upload_rate: 20, download_rate: 0,
                total_uploaded: 200, total_downloaded: 100, peers_connected: 1, progress: 1, ratio: 2
            }] }) };
    `)({ now: () => now, request: () => new Promise(r => { release = r; }) });
    const response = rate => ({ total: 1, filtered: 1, rows: [
        { agent: "local-hoard", info_hash: "same", upload_rate: rate, name: "fresh metadata" },
        { agent: "local-vpn", info_hash: "same", upload_rate: rate },
        { agent: "remote-hoard", info_hash: "same", upload_rate: rate }
    ] });
    const pending = page.fetch("facets=1&live=1");
    page.delta();
    release(response(10)); await pending;
    assert.equal(page.rows()[0].upload_rate, 20, "REST must not overwrite SSE received during its request");
    assert.equal(page.rows()[0].name, "fresh metadata");
    assert.equal(page.rows()[1].upload_rate, 10, "another engine must remain independent");
    assert.equal(page.rows()[2].upload_rate, 10, "remote rows must remain independent");
    now += 100;
    const cached = page.fetch("facets=1&live=1");
    release(response(10)); await cached;
    assert.equal(page.rows()[0].upload_rate, 20, "a shared cached page must not roll back recent SSE");
    const manual = page.fetch("facets=1");
    page.delta();
    release(response(30)); await manual;
    assert.equal(page.rows()[0].upload_rate, 30, "manual fresh responses supersede queued SSE");
    now += 10000;
    const fallback = page.fetch("facets=1&live=1");
    release(response(40)); await fallback;
    assert.equal(page.rows()[0].upload_rate, 40, "stale SSE must not override REST recovery");
    const controller = new AbortController();
    const cancelled = page.fetch("facets=1&live=1", controller.signal);
    controller.abort();
    release(response(50));
    assert.equal(await cancelled, false);
    assert.equal(page.rows()[0].upload_rate, 40, "an already-resolved but cancelled response must not repaint the page");
}

function membershipEventsScheduleRefresh() {
    let scheduled = 0;
    const conn = new Function("schedule", `
        let _sseConn = null, _syncCursor = 0, _hoardAllTorrents = [];
        let _lastHoardStatsEventAt, _lastHoardUploading, _lastHoardLiveRevision;
        let _hoardLiveDirty = false, _hoardStatsPainted = false;
        const document = { hidden: false }, EventSource = class {};
        const _scheduleHoardPageRefresh = schedule, _renderHoardStatsHeader = () => {};
        ${fn("_openHoardSSE")}
        _openHoardSSE(); return _sseConn;
    `)(() => scheduled++);
    conn.onmessage({ data: JSON.stringify({ event: "hoard_stats_snapshot",
        data: { torrents_uploading: 3, live_revision: 5 } }) });
    assert.equal(scheduled, 1, "membership changes should schedule a page refresh without waiting for poll()");
}

async function membershipRefreshesAreBounded() {
    const env = { now: 10100, interval: 1000, hidden: false, visible: true, fast: true, calls: 0, timers: [] };
    const scheduler = new Function("env", `
        let _hoardPageRefreshTimer = null, _hoardPageBusy = 0, _hoardLastFetchAt = 10000, _hoardLiveDirty = true;
        const LIVE_REFRESH_MS = 1000, Date = { now: () => env.now };
        const document = { get hidden() { return env.hidden; } };
        const _hoardTabVisible = () => env.visible, _hoardFastRefresh = () => env.fast;
        const _hoardRefreshInterval = () => env.interval;
        const setTimeout = (callback, delay) => { env.timers.push({ callback, delay }); return env.timers.length; };
        const fetchHoardPage = async () => { env.calls++; _hoardLiveDirty = false; _hoardLastFetchAt = env.now; };
        ${fn("_scheduleHoardPageRefresh")}
        return { schedule: _scheduleHoardPageRefresh, busy: value => { _hoardPageBusy = value; } };
    `)(env);
    scheduler.schedule(); scheduler.schedule();
    assert.equal(env.timers.length, 1, "bursts coalesce into one scheduled request");
    assert.equal(env.timers[0].delay, 900, "refresh runs when due, independently of the global poll phase");
    env.now = 11000;
    await env.timers.shift().callback();
    assert.equal(env.calls, 1);
    assert.equal(env.timers.length, 0, "no recurring timer after a successful clean refresh");
    env.hidden = true; scheduler.schedule();
    env.hidden = false; env.visible = false; scheduler.schedule();
    env.visible = true; env.fast = false; scheduler.schedule();
    assert.equal(env.timers.length, 0, "hidden and full-scan views keep their existing backstop");
    env.fast = true; env.interval = 5000; env.now = 11100;
    scheduler.schedule();
    assert.equal(env.timers[0].delay, 4900, "slow indexed requests retain adaptive backoff");
    await env.timers.shift().callback();
    env.now = 20000; env.interval = 1000;
    scheduler.busy(1); scheduler.schedule();
    assert.equal(env.timers[0].delay, 1000, "a busy page cannot cause zero-delay timer spinning");
}

async function engineCardRequestsDoNotOverlap() {
    let now = 10000, calls = 0, release;
    const refresh = new Function("env", `
        let _extraEnginesAt = 0, _extraEnginesBusy = false;
        const Date = { now: () => env.now() }, console = { error: () => {} };
        const _updateExtraEngines = env.request;
        ${fn("_maybeUpdateExtraEngines")}
        return _maybeUpdateExtraEngines;
    `)({ now: () => now, request: () => {
        calls++;
        return new Promise(r => { release = r; });
    } });
    for (let i = 0; i < 10; i++) { refresh(); now += 3000; }
    assert.equal(calls, 1, "a stalled engines request must not accumulate another request every three seconds");
    release();
    await new Promise(r => setImmediate(r));
    refresh();
    assert.equal(calls, 2, "polling resumes after the in-flight request finishes");
    release();
}

async function supersededPageRequestsAreCancelled() {
    const env = { query: "first", calls: [] };
    const page = new Function("env", `
        let _selAll = null, _hoardPageBusy = 0, _hoardInFlightQuery = "", _hoardPageAbort = null;
        let _hoardLiveDirty = true, _hoardMembershipDirty = false;
        let _hoardLastFetchAt = 0, _hoardFullFetchAt = 0, _hoardLiveFetchMs = 0, _hoardFullFetchMs = 0;
        const _hoardFastRefresh = () => false, _hoardRefreshInterval = () => 0;
        const _hoardPageQuery = () => "facets=1&state=" + env.query;
        const _fetchHoardPageInner = (qs, signal) => new Promise(resolve => {
            env.calls.push({ qs, signal, resolve });
            signal?.addEventListener("abort", () => resolve(false), { once: true });
        });
        ${fn("fetchHoardPage")}
        return { fetch: fetchHoardPage, state: () => ({ busy: _hoardPageBusy, dirty: _hoardMembershipDirty }) };
    `)(env);
    const first = page.fetch(true);
    env.query = "second";
    const second = page.fetch(true);
    assert.equal(env.calls[0].signal?.aborted, true, "changing pills must cancel the obsolete browser request");
    await first;
    assert.deepEqual(page.state(), { busy: 1, dirty: false }, "a cancelled request must not dirty the replacement page");
    env.calls[1].resolve(true); await second;
    assert.equal(page.state().busy, 0);
}

Promise.all([requests(), raceRequests(), pageCannotOverwriteLiveRows(), membershipRefreshesAreBounded(), engineCardRequestsDoNotOverlap(), supersededPageRequestsAreCancelled(), Promise.resolve().then(membershipEventsScheduleRefresh)])
    .then(() => console.log("live_refresh: ok")).catch(e => { console.error(e); process.exitCode = 1; });
