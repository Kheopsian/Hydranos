// The Categories tab must not ask for orphaned labels on every refresh.
// Run with: node web/tests/categories_orphans_poll.test.js (from the repository
// root). Pulls updateCategories() and the state it keeps out of app.js, so the
// test reads the shipped code, not a copy of it.
"use strict";
const fs = require("fs");
const path = require("path");
const assert = require("assert");

const file = process.env.APP_JS || path.join(__dirname, "..", "static", "app.js");
const src = fs.readFileSync(file, "utf8");
const start = src.indexOf("let _orphanInfo = [];");
assert.ok(start >= 0, "_orphanInfo not found in app.js");
const fnStart = src.indexOf("async function updateCategories(", start);
assert.ok(fnStart >= 0, "updateCategories not found in app.js");
const end = src.indexOf("\n}\n", fnStart);

function harness() {
    const calls = { cats: 0, orphans: 0 };
    const pending = [];
    let hold = false;
    const api = (url) => {
        if (url === "/api/categories") { calls.cats++; return Promise.resolve([{ name: "tv", mode: "hoard", save_path: "/d" }]); }
        if (url === "/api/categories/orphans") {
            calls.orphans++;
            const v = [{ name: "old" + calls.orphans, torrents: 2 }];
            if (hold) return new Promise(r => pending.push(() => r(v)));
            return Promise.resolve(v);
        }
        throw new Error("unexpected " + url);
    };
    const tbody = { _h: "", writes: 0, set innerHTML(v) { this._h = v; this.writes++; }, get innerHTML() { return this._h; } };
    const clock = { now: 1000000 };
    const FakeDate = { now: () => clock.now };
    const body = src.slice(start, end + 2) + "\nreturn { updateCategories };";
    const mod = new Function("api", "document", "Date", "esc", "incoCat", "incoPath", "_catSpreadSummary", "t",
        "refreshCategoryOptions", "console", "let _lastCategoryRows = null;\n" + body)(
        api, { getElementById: () => tbody }, FakeDate, (x) => String(x), (x) => x, (x) => x, () => "", (x) => x,
        () => {}, { error() {} });
    return { calls, tbody, clock, mod, pending, hold: (v) => { hold = v; } };
}

(async () => {
    // Ten refreshes in a row (poll() every second): one orphans call, ten light ones.
    {
        const h = harness();
        for (let i = 0; i < 10; i++) { await h.mod.updateCategories(); h.clock.now += 1000; }
        assert.strictEqual(h.calls.orphans, 1, "orphans asked once over 10 unforced refreshes, got " + h.calls.orphans);
        assert.strictEqual(h.calls.cats, 10, "/api/categories is still refreshed every time");
        assert.ok(h.tbody.innerHTML.includes("old1"), "the cached orphan list is still rendered");
        assert.strictEqual(h.tbody.writes, 1, "DOM touched only when the HTML changed");
    }
    // A forced call (tab opened, save/delete/adopt) asks again.
    {
        const h = harness();
        await h.mod.updateCategories(true);
        await h.mod.updateCategories();
        await h.mod.updateCategories(true);
        assert.strictEqual(h.calls.orphans, 2, "a forced refresh asks again, got " + h.calls.orphans);
        assert.ok(h.tbody.innerHTML.includes("old2"), "the fresh list replaces the old one");
    }
    // After a minute it asks again on its own.
    {
        const h = harness();
        await h.mod.updateCategories();
        h.clock.now += 59000; await h.mod.updateCategories();
        assert.strictEqual(h.calls.orphans, 1, "not before 60 s");
        h.clock.now += 1000; await h.mod.updateCategories();
        assert.strictEqual(h.calls.orphans, 2, "once 60 s have passed");
    }
    // Never two in flight: a forced call while one is pending reuses the old list.
    {
        const h = harness();
        h.hold(true);
        const first = h.mod.updateCategories(true);
        const second = h.mod.updateCategories(true);
        await new Promise(r => setImmediate(r));
        h.pending.forEach(f => f());
        await Promise.all([first, second]);
        assert.strictEqual(h.calls.orphans, 1, "a second orphans call while one is in flight, got " + h.calls.orphans);
    }
    console.log("categories_orphans_poll: ok");
})().catch(e => { console.error(e); process.exit(1); });
