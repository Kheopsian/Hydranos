"use strict";
const assert = require("assert");
const fs = require("fs");
const path = require("path");
const src = fs.readFileSync(process.env.APP_JS || path.join(__dirname, "../static/app.js"), "utf8").replace(/\r\n/g, "\n");
function fn(name, optional = false) {
    const match = new RegExp("^(?:async )?function " + name + "\\(", "m").exec(src);
    if (!match && optional) return `function ${name}() {}`;
    assert.ok(match, name);
    return src.slice(match.index, src.indexOf("\n}", match.index) + 2);
}
const env = {search: "", category: [], pills: new Map()};
env.pills.set(".chip-state[data-state='__active__']", {innerHTML: "Actively Seeding 10"});
const view = new Function("env", `
    let _hoardFacets = {all: 5000, active: 10, state: {seeding: 4900}, category: {music: 300}};
    let _hoardLiveFacets = null;
    let _lastStatusSnapshot = null;
    const _renderBenchmarkLive = () => {};
    const _hoardCatInc = env.category, _hoardCatExc = [], _hoardTagInc = [], _hoardTagExc = [];
    const _hoardTrackerInc = [], _hoardTrackerExc = [], _hoardErrInc = [], _hoardErrExc = [];
    const _hoardTabVisible = () => true, _chipCount = (key, n) => String(n), t = x => x;
    const _extraTorrents = 0, window = {};
    const fmtInt = String, formatSpeed = String, formatBytes = String;
    const document = {
        getElementById: () => ({value: env.search, style: {}, classList: {}}),
        querySelector: selector => {
            if (!env.pills.has(selector)) env.pills.set(selector, {});
            return env.pills.get(selector);
        }
    };
    ${fn("_hoardStateFacets", true)}
    ${fn("_applyHoardLiveCounts", true)}
    ${fn("_renderHoardStateCounts", true)}
    ${fn("_renderStatus")}
    return {
        status: facets => {
            // The rest of the header is outside this minimal DOM; its SSE caller catches rendering errors too.
            try { _renderStatus({hoard: {state_facets: facets}}); } catch (_) {}
        },
        facets: () => typeof _hoardStateFacets === "function" ? _hoardStateFacets() : _hoardFacets
    };
`)(env);
view.status({all: 6000, active: 18, state: {seeding: 5700, seed_only: 50, downloading: 100, checking_files: 20}, check_waiting: 7});
const pill = state => env.pills.get(`.chip-state[data-state='${state}']`)?.innerHTML;
assert.equal(pill("__active__"), "Actively Seeding 18", "state pills must follow the status snapshot without waiting for a page scan");
assert.equal(pill("seeding"), "Seeding 5750", "seed-only torrents count as seeding");
assert.equal(pill(""), "All 6000", "counts describe the catalogue, not the loaded 500 rows");
assert.equal(env.pills.get(".chip-check[data-check='__check_reading__']")?.innerHTML, "Reading 13");
env.category.push("music");
view.status({all: 7000, active: 99, state: {seeding: 6800}});
assert.equal(pill("__active__"), "Actively Seeding 10", "global counts cannot overwrite filtered facet counts");
env.category.length = 0;
env.search = "album";
view.status({all: 7000, active: 99, state: {seeding: 6800}});
assert.equal(pill("__active__"), "Actively Seeding 10", "search also keeps filtered counts");
env.search = "";
view.status(null);
assert.equal(pill("__active__"), "Actively Seeding 10", "unavailable live counts keep the authoritative page facets");
console.log("live_state_pills: ok");
