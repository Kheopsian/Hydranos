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
        const HOARD_REFRESH_MS = 20000;
        const Date = { now: () => env.now() };
        const _liveStreamFresh = env.fresh, api = env.api;
        const loadRacePolicy = () => {}, renderRaceTable = () => {};
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
Promise.all([requests(), raceRequests()]).then(() => console.log("live_refresh: ok")).catch(e => { console.error(e); process.exitCode = 1; });
