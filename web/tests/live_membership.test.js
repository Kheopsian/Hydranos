"use strict";
const fs = require("fs"), path = require("path"), assert = require("assert");
const src = fs.readFileSync(process.env.APP_JS || path.join(__dirname, "../static/app.js"), "utf8").replace(/\r\n/g, "\n");
function fn(name, optional = false) {
    const match = new RegExp("^(?:async )?function " + name + "\\(", "m").exec(src);
    if (!match && optional) return `async function ${name}() {}`;
    assert.ok(match, name);
    return src.slice(match.index, src.indexOf("\n}", match.index) + 2);
}
async function arrivals(filter) {
    const env = {now: 100000, filter, timers: [], calls: [], paints: []};
    const view = new Function("env", `
        const HOARD_REFRESH_MS = 20000, LIVE_REFRESH_MS = 1000, HOARD_RENDER_LIMIT = 500;
        let _hoardStateFilter = env.filter, _hoardFacets = {all: 2}, _hoardLiveDirty = false, _hoardMembershipDirty = true;
        let _hoardFullFetchAt = 0, _hoardFullFetchMs = 1000, _hoardLiveFetchMs = 0, _hoardRowFetchMs = 0;
        let _hoardLastFetchAt = 99000, _hoardPageBusy = 0, _hoardInFlightQuery = "", _hoardPageAbort = null;
        let _hoardFacetAbort = null, _hoardFacetQuery = "", _hoardFacetDirty = true;
        let _hoardPageRefreshTimer = null, _hoardPageSeq = 1, _hoardPageApplied = 1, _liveRowRevision = 0;
        let _hoardAppliedSearch = "", _hoardServerTotal = 2, _hoardServerFiltered = 2, _selAll = null;
        let _hoardAllTorrents = [{info_hash: "a"}, {info_hash: "b"}], _raceTorrents = [], _raceMembershipDirty = false;
        let _sseConn = null, _syncCursor = 0, _lastStatusEventAt = env.now, _lastHoardStatsEventAt = env.now;
        let _lastHoardUploading = 2, _lastHoardLiveRevision = 1, _hoardStatsPainted = true;
        const document = {hidden: false, getElementById: () => null}, EventSource = class {readyState = 1};
        const Date = {now: () => env.now}, _hoardTabVisible = () => true;
        const setTimeout = (callback, delay) => {env.timers.push({callback,delay});return env.timers.length;};
        const _hoardPageQuery = (facets = true) => "limit=500&facets=" + Number(facets) + (env.filter ? "&state=" + env.filter : "");
        let _hoardAppliedQuery = _hoardPageQuery();
        const _renderHoardCounts = () => {}, _refreshHoardPins = async () => {}, _renderHoardStatsHeader = () => {};
        const _renderStatus = () => {}, _preserveLiveRows = () => {}, _hoardMatches = () => true;
        const _applyLiveRows = () => 0, _scheduleHoardRender = () => {}, _updateRowHighlights = () => {};
        const renderHoardTable = () => env.paints.push(_hoardAllTorrents.map(row => row.info_hash));
        const api = (url, options) => new Promise((resolve, reject) => {
            env.calls.push({url, options, resolve});
            options?.signal?.addEventListener("abort", () => reject(new Error("aborted")), {once:true});
        });
        ${fn("_liveStreamFresh")}
        ${fn("_hoardFullRefreshInterval")}
        ${fn("_hoardFastRefresh")}
        ${fn("_hoardRefreshInterval")}
        ${fn("_scheduleHoardPageRefresh")}
        ${fn("_refreshHoardFacets", true)}
        ${fn("fetchHoardPage")}
        ${fn("_fetchHoardPageInner")}
        ${fn("_reconcileHoardRows")}
        ${fn("_openHoardSSE")}
        _openHoardSSE();
        return {startFacets: () => _refreshHoardFacets(),
            message: data => _sseConn.onmessage({data:JSON.stringify(data)}),
            rows: () => _hoardAllTorrents, facets: () => _hoardFacets};
    `)(env);
    const facets = view.startFacets();
    view.message({event: "hoard_stats_snapshot", data: {torrents_uploading: 3, live_revision: 2}});
    view.message({event: "stats_snapshot", data: {mode:"hoard", torrents:[{info_hash:"c",upload_rate:10}]}});
    assert.equal(env.timers.length, 1, `${filter || "All"}: a count/unknown-row change must schedule membership even with stale facets`);
    const request = env.timers.shift().callback();
    const rowCall = env.calls.find(call => new URL(call.url, "http://local").searchParams.get("facets") === "0");
    assert.ok(rowCall, `${filter || "All"}: row queries must proceed independently of the blocked facet request`);
    rowCall.resolve({total:3, filtered:3, facets:null, rows:[{info_hash:"a"},{info_hash:"b"},{info_hash:"c"}]});
    await request;
    assert.deepEqual(view.rows().map(row=>row.info_hash), ["a","b","c"], "the third row appears without clicking the pill");
    assert.ok(env.paints.every(rows => rows.includes("a") && rows.includes("b")), "no intermediate paint drops existing rows");
    env.now += 1000;
    view.message({event:"page_invalidated",data:{mode:"hoard"}});
    assert.equal(env.timers.length, 1, "new-torrent invalidations also schedule the page directly");
    const added = env.timers.shift().callback();
    const secondRowCall = env.calls.filter(call => new URL(call.url,"http://local").searchParams.get("facets") === "0").at(-1);
    secondRowCall.resolve({total:4,filtered:4,rows:[{info_hash:"a"},{info_hash:"b"},{info_hash:"c"},{info_hash:"d"}]});
    await added;
    assert.equal(view.rows().length, 4, "another arrival is admitted while facets are still pending");
    const facetCall = env.calls.find(call => new URL(call.url, "http://local").searchParams.get("facets") === "1");
    assert.ok(facetCall);
    assert.equal(new URL(facetCall.url,"http://local").searchParams.get("limit"), "1", "facet work does not download another full page");
    facetCall.resolve({facets:{all:3}, rows:[{info_hash:"only-a-facet-preview"}]});
    await facets;
    assert.deepEqual(view.rows().map(row=>row.info_hash), ["a","b","c","d"], "a slow facet response never replaces the live page");
}
Promise.all(["__active__", "seeding", "downloading", "checking_files", ""].map(arrivals))
    .then(() => console.log("live_membership: ok")).catch(e => {console.error(e);process.exitCode=1;});
